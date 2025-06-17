//! A benchmarking program for the LDOS table data structures
#![feature(test)]

pub(crate) mod generic_stress;
pub mod wait_handler;

extern crate test;

use std::{
    collections::HashMap,
    io::stdout,
    sync::{mpsc, Arc},
    time::Duration,
};

use anyhow::anyhow;
use clap::Parser;
use evalexpr::eval_int;
use itertools::Itertools as _;
use ldos_tables::table_ipc::{spsc, stubs::NonConstructableWeakObserver, Table, WeakObserver};
use serde::{Deserialize, Serialize};

use crate::{
    generic_stress::{
        stress_ping_pong, stress_produce_consume, ExecutionStatistics, StressMessage,
        TableAttachments, TestWeakObserver,
    },
    test_framework_impls::*,
    wait_handler::{WaitBusy, WaitBusyAndThen, WaitCondVar, WaitPark, WaitYield},
};

mod test_framework_impls;

#[derive(Debug, Serialize, Deserialize)]
struct RawConfig {
    n_messages: String,
    #[serde(default)]
    delay: String,
    queue_length: String,
    #[serde(default)]
    n_producers: String,
    #[serde(default)]
    n_consumers: String,
    #[serde(default)]
    n_strong_observers: String,
    #[serde(default)]
    weak_observer_delays: Vec<String>,
    impl_name: String,
    #[serde(default)]
    busy_wait_iterations: String,
}

#[derive(Debug, Serialize)]
struct Config {
    n_messages: usize,
    delay: Duration,
    queue_length: usize,
    n_producers: usize,
    n_consumers: usize,
    n_strong_observers: usize,
    weak_observer_delays: Vec<usize>,
    impl_name: String,
    busy_wait_iterations: u32,
}

#[derive(Debug, Serialize)]
struct BenchmarkResult {
    config: Config,
    git_info: String,
    date: chrono::DateTime<chrono::Utc>,
    result: ExecutionStatistics,
}

fn empty_or_parse<T: TryFrom<i64>>(s: &str, default: T) -> anyhow::Result<T>
where
    <T as TryFrom<i64>>::Error: std::error::Error + Send + Sync + 'static,
{
    if s == "" {
        Ok(default)
    } else {
        Ok(eval_int(s)?.try_into()?)
    }
}

impl Config {
    fn from_raw(c: RawConfig) -> anyhow::Result<Self> {
        Ok(Self {
            n_messages: eval_int(&c.n_messages)?.try_into()?,
            delay: Duration::from_nanos(empty_or_parse(&c.delay, 0)?),
            queue_length: eval_int(&c.queue_length)?.try_into()?,
            n_producers: empty_or_parse(&c.n_producers, 1)?,
            n_consumers: empty_or_parse(&c.n_consumers, 1)?,
            n_strong_observers: empty_or_parse(&c.n_strong_observers, 0)?,
            weak_observer_delays: c
                .weak_observer_delays
                .iter()
                .map(|d| empty_or_parse(&d, 0))
                .try_collect()?,
            impl_name: c.impl_name,
            busy_wait_iterations: empty_or_parse(&c.busy_wait_iterations, 64)?,
        })
    }
}

fn get_git_info() -> anyhow::Result<String> {
    let out = std::process::Command::new("git")
        .args(["describe", "--always", "--long", "--dirty"])
        .output()?;

    Ok(String::from_utf8_lossy(&out.stdout)
        .into_owned()
        .trim_end()
        .replace("\n", "; "))
}

type TestFn = Box<dyn Fn(&Config) -> ExecutionStatistics>;

#[derive(Parser)]
#[command(name = "bench", about = "Run benchmark tests", version = "1.0")]
struct Cli {
    #[arg(
        short,
        long,
        value_name = "FILE",
        help = "Path to the YAML configuration file"
    )]
    config_file: String,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let config_str = std::fs::read_to_string(&cli.config_file)?;
    let config: Vec<RawConfig> = serde_yaml::from_str(&config_str)?;
    let configs = config
        .into_iter()
        .map(|config| Config::from_raw(config))
        .collect::<Result<Vec<_>, _>>()?;

    let mut impl_fns: Vec<(&str, TestFn)> = Default::default();

    fn make_weak_observers<U: Table<StressMessage> + 'static>(
        table: Arc<U>,
        config: &Config,
    ) -> Vec<TestWeakObserver<impl WeakObserver<StressMessage>>> {
        let mut observers = Vec::new();
        for d in config.weak_observer_delays.iter() {
            observers.push(TestWeakObserver {
                inner: table.attach_weak_observer().unwrap(),
                delay: Duration::from_nanos(*d as u64),
            });
        }
        observers
    }

    impl_fns.push((
        "std mpsc::channel",
        Box::new(|config| {
            let (sender, receiver) = mpsc::channel();
            stress_produce_consume(
                vec![sender],
                vec![receiver],
                Vec::<NullReceiver>::new(),
                Vec::<TestWeakObserver<NonConstructableWeakObserver>>::new(),
                config.n_messages,
                config.delay,
            )
        }),
    ));

    impl_fns.push((
        "std mpsc::sync_channel",
        Box::new(|config| {
            let (sender, receiver) = mpsc::sync_channel(config.queue_length);
            stress_produce_consume(
                vec![sender],
                vec![receiver],
                Vec::<NullReceiver>::new(),
                Vec::<TestWeakObserver<NonConstructableWeakObserver>>::new(),
                config.n_messages,
                config.delay,
            )
        }),
    ));

    impl_fns.push((
        "kanal::bounded",
        Box::new(|config| {
            let (sender, receiver) = kanal::bounded(config.queue_length);
            stress_produce_consume(
                vec![sender],
                vec![receiver],
                Vec::<NullReceiver>::new(),
                Vec::<TestWeakObserver<NonConstructableWeakObserver>>::new(),
                config.n_messages,
                config.delay,
            )
        }),
    ));

    impl_fns.push((
        "spsc::SpscTable yield",
        Box::new(|config| {
            let wait_handler = WaitBusyAndThen::new(WaitYield::new(), config.busy_wait_iterations);
            let table = spsc::SpscTable::new(
                config.queue_length,
                config.n_strong_observers,
                config.weak_observer_delays.len(),
            );
            stress_produce_consume(
                vec![WaitProducer::new(
                    table.attach_producer().unwrap(),
                    wait_handler.clone(),
                )],
                vec![WaitConsumer::new(
                    table.attach_consumer().unwrap(),
                    wait_handler.clone(),
                )],
                (0..config.n_strong_observers)
                    .map(|_| {
                        WaitStrongObserver::new(
                            table.attach_strong_observer().unwrap(),
                            wait_handler.clone(),
                        )
                    })
                    .collect(),
                make_weak_observers(table, config),
                config.n_messages,
                config.delay,
            )
        }),
    ));

    impl_fns.push((
        "spsc::SpscTable yield weak",
        Box::new(|config| {
            let wait_handler = WaitBusyAndThen::new(WaitYield::new(), config.busy_wait_iterations);
            let table = spsc::SpscTable::new(
                config.queue_length,
                config.n_strong_observers,
                config.weak_observer_delays.len(),
            );
            stress_produce_consume(
                vec![WaitProducer::new(
                    table.attach_producer().unwrap(),
                    wait_handler.clone(),
                )],
                vec![WaitConsumer::new(
                    table.attach_consumer().unwrap(),
                    wait_handler.clone(),
                )],
                (0..config.n_strong_observers)
                    .map(|_| {
                        WaitStrongObserver::new(
                            table.attach_strong_observer().unwrap(),
                            wait_handler.clone(),
                        )
                    })
                    .collect(),
                make_weak_observers(table, config),
                config.n_messages,
                config.delay,
            )
        }),
    ));

    impl_fns.push((
        "ping-pong spsc::SpscTable yield",
        Box::new(|config| {
            let wait_handler = WaitBusyAndThen::new(WaitYield::new(), config.busy_wait_iterations);
            let call_table = {
                let table = spsc::SpscTable::new(
                    config.queue_length,
                    config.n_strong_observers,
                    config.weak_observer_delays.len(),
                );
                TableAttachments {
                    producer: WaitProducer::new(
                        table.attach_producer().unwrap(),
                        wait_handler.clone(),
                    ),
                    consumer: WaitConsumer::new(
                        table.attach_consumer().unwrap(),
                        wait_handler.clone(),
                    ),
                    observers: Default::default(),
                    weak_observers: Default::default(),
                }
            };
            let reply_table = {
                let table = spsc::SpscTable::new(
                    config.queue_length,
                    config.n_strong_observers,
                    config.weak_observer_delays.len(),
                );
                TableAttachments {
                    producer: WaitProducer::new(
                        table.attach_producer().unwrap(),
                        wait_handler.clone(),
                    ),
                    consumer: WaitConsumer::new(
                        table.attach_consumer().unwrap(),
                        wait_handler.clone(),
                    ),
                    observers: (0..config.n_strong_observers)
                        .map(|_| {
                            WaitStrongObserver::new(
                                table.attach_strong_observer().unwrap(),
                                wait_handler.clone(),
                            )
                        })
                        .collect(),
                    weak_observers: make_weak_observers(table, config),
                }
            };

            stress_ping_pong(
                call_table,
                reply_table,
                config.n_messages,
                config.delay,
            )
        }),
    ));

    impl_fns.push((
        "spsc::SpscTable condvar",
        Box::new(|config| {
            let wait_handler =
                WaitBusyAndThen::new(WaitCondVar::new(), config.busy_wait_iterations);
            let table = spsc::SpscTable::new(
                config.queue_length,
                config.n_strong_observers,
                config.weak_observer_delays.len(),
            );
            stress_produce_consume(
                vec![WaitProducer::new(
                    table.attach_producer().unwrap(),
                    wait_handler.clone(),
                )],
                vec![WaitConsumer::new(
                    table.attach_consumer().unwrap(),
                    wait_handler.clone(),
                )],
                (0..config.n_strong_observers)
                    .map(|_| {
                        WaitStrongObserver::new(
                            table.attach_strong_observer().unwrap(),
                            wait_handler.clone(),
                        )
                    })
                    .collect(),
                make_weak_observers(table, config),
                config.n_messages,
                config.delay,
            )
        }),
    ));

    impl_fns.push((
        "spsc::SpscTable spin",
        Box::new(|config| {
            let wait_handler = WaitBusy::new();
            let table = spsc::SpscTable::new(
                config.queue_length,
                config.n_strong_observers,
                config.weak_observer_delays.len(),
            );
            stress_produce_consume(
                vec![WaitProducer::new(
                    table.attach_producer().unwrap(),
                    wait_handler.clone(),
                )],
                vec![WaitConsumer::new(
                    table.attach_consumer().unwrap(),
                    wait_handler.clone(),
                )],
                (0..config.n_strong_observers)
                    .map(|_| {
                        WaitStrongObserver::new(
                            table.attach_strong_observer().unwrap(),
                            wait_handler.clone(),
                        )
                    })
                    .collect(),
                Vec::<TestWeakObserver<NonConstructableWeakObserver>>::new(),
                config.n_messages,
                config.delay,
            )
        }),
    ));

    impl_fns.push((
        "spsc::SpscTable park",
        Box::new(|config| {
            let wait_handler = WaitBusyAndThen::new(WaitPark::new(), config.busy_wait_iterations);
            let table = spsc::SpscTable::new(
                config.queue_length,
                config.n_strong_observers,
                config.weak_observer_delays.len(),
            );
            // let threads: Arc<Mutex<Vec<Thread>>> = Default::default();
            stress_produce_consume(
                vec![WaitProducer::new(
                    table.attach_producer().unwrap(),
                    wait_handler.clone(),
                )],
                vec![WaitConsumer::new(
                    table.attach_consumer().unwrap(),
                    wait_handler.clone(),
                )],
                (0..config.n_strong_observers)
                    .map(|_| {
                        WaitStrongObserver::new(
                            table.attach_strong_observer().unwrap(),
                            wait_handler.clone(),
                        )
                    })
                    .collect(),
                make_weak_observers(table, config),
                config.n_messages,
                config.delay,
            )
        }),
    ));

    impl_fns.push((
        "spsc::SpscTable call",
        Box::new(|config| {
            let table = spsc::SpscTable::new(
                config.queue_length,
                0,
                config.weak_observer_delays.len(),
            );
            let callback = Default::default();
            stress_produce_consume(
                vec![CallProducer::new(
                    table.attach_producer().unwrap(),
                    Clone::clone(&callback),
                )],
                vec![CallConsumer::new(
                    table.attach_consumer().unwrap(),
                    Clone::clone(&callback),
                )],
                Vec::<NullReceiver>::new(),
                make_weak_observers(table, config),
                config.n_messages,
                config.delay,
            )
        }),
    ));

    impl_fns.push((
        "spsc::SpscTable call no weak",
        Box::new(|config| {
            let table = spsc::SpscTableCustom::<_, true, false>::new(
                config.queue_length,
                0,
                config.weak_observer_delays.len(),
            );
            let callback = Default::default();
            stress_produce_consume(
                vec![CallProducer::new(
                    table.attach_producer().unwrap(),
                    Clone::clone(&callback),
                )],
                vec![CallConsumer::new(
                    table.attach_consumer().unwrap(),
                    Clone::clone(&callback),
                )],
                Vec::<NullReceiver>::new(),
                make_weak_observers(table, config),
                config.n_messages,
                config.delay,
            )
        }),
    ));


    impl_fns.push((
        "direct call",
        Box::new(|config| {
            let callback = Default::default();
            let producer_consumer = DirectCallProducerConsumer::<StressMessage>::new(
                    Clone::clone(&callback),
                );
            stress_produce_consume(
                vec![producer_consumer.clone()],
                vec![producer_consumer.clone()],
                Vec::<NullReceiver>::new(),
                Vec::<TestWeakObserver<NonConstructableWeakObserver>>::new(),
                config.n_messages,
                config.delay,
            )
        }),
    ));

    let impl_fns: HashMap<&str, TestFn> = impl_fns.into_iter().collect();
    let mut results = Vec::new();

    for config in configs {
        if let Some(f) = impl_fns.get(config.impl_name.as_str()) {
            results.push(BenchmarkResult {
                result: f(&config),
                config,
                git_info: get_git_info()?,
                date: chrono::Utc::now(),
            });
        } else {
            eprintln!(
                "Unknown configuration: {}. The supported configurations are:",
                config.impl_name
            );
            // Print supported configurations
            for name in impl_fns.keys().sorted() {
                eprintln!("  {}", name);
            }
            return Err(anyhow!("Unknown configuration: {}", config.impl_name));
        }
    }

    serde_yaml::to_writer(stdout(), &results)?;

    Ok(())
}
