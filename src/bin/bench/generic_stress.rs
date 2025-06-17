use std::{
    hash::{DefaultHasher, Hash, Hasher}, hint::black_box, sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Condvar, Mutex,
    }, thread, time::Duration
};

use capture_it::capture;
use ldos_tables::table_ipc::WeakObserver;
use serde::{ser::SerializeStruct, Serialize, Serializer};

pub fn measure_run_time(f: impl Fn()) -> (Duration, Duration) {
    let start_cpu_time = cpu_time::ProcessTime::now();
    let start_time = std::time::Instant::now();
    f();
    let end_time = std::time::Instant::now();
    let end_cpu_time = cpu_time::ProcessTime::now();
    (
        end_time.duration_since(start_time),
        end_cpu_time.duration_since(start_cpu_time),
    )
}

pub trait TestReceiver<T: Copy + Send>: Send {
    /// Handle a stream of messages. Return true to finish.
    fn handle(self, f: impl FnMut(T) -> bool + Send + 'static);
}

pub trait TestProducer<T: Copy + Send>: Send {
    fn send(&self, v: T);
}

#[derive(Default, Clone, Copy, Hash)]
pub struct StressMessage {
    #[allow(unused)]
    src: usize,
    #[allow(unused)]
    x: usize,
}

#[derive(Default, Clone, Copy)]
struct StressInnerState {
    running: bool,
    n_completed: usize,
}

struct StressState {
    mutex: Mutex<StressInnerState>,
    condvar: Condvar,
    explected_complete: usize,
}

impl StressState {
    fn new(explected_complete: usize) -> Self {
        StressState {
            mutex: Mutex::new(StressInnerState::default()),
            condvar: Condvar::new(),
            explected_complete,
        }
    }

    fn wait_for_start(&self) {
        let mut state = self.mutex.lock().unwrap();
        while !state.running {
            state = self.condvar.wait(state).unwrap();
        }
    }

    fn trigger_start(&self) {
        let mut state = self.mutex.lock().unwrap();
        state.running = true;
        self.condvar.notify_all();
    }

    fn wait_for_complete(&self) {
        let mut state = self.mutex.lock().unwrap();
        while state.n_completed < self.explected_complete {
            state = self.condvar.wait(state).unwrap();
        }
    }

    fn complete(&self) {
        let mut state = self.mutex.lock().unwrap();
        state.n_completed += 1;
        self.condvar.notify_all();
    }
}

#[derive(Clone)]
struct MessageTracker {
    hash: Arc<AtomicU64>,
}

impl MessageTracker {
    fn new() -> Self {
        MessageTracker {
            hash: Arc::new(AtomicU64::new(0)),
        }
    }

    fn update(&self, v: StressMessage) {
        if cfg!(debug_assertions) {
            let mut hasher = DefaultHasher::new();
            v.hash(&mut hasher);
            self.hash.fetch_xor(hasher.finish(), Ordering::Relaxed);
        }
    }

    fn combine(&self, other: &Self) {
        self.hash
            .fetch_xor(other.hash.load(Ordering::Relaxed), Ordering::Relaxed);
    }

    fn get(&self) -> u64 {
        self.hash.load(Ordering::Relaxed)
    }
}

pub struct TestWeakObserver<T: WeakObserver<StressMessage>> {
    pub inner: T,
    pub delay: Duration,
}

pub fn stress_produce_consume(
    producers: Vec<impl TestProducer<StressMessage> + 'static>,
    consumers: Vec<impl TestReceiver<StressMessage> + 'static>,
    observers: Vec<impl TestReceiver<StressMessage> + 'static>,
    weak_observers: Vec<TestWeakObserver<impl WeakObserver<StressMessage> + 'static>>,
    n_messages: usize,
    delay: Duration,
) -> ExecutionStatistics {
    let state = Arc::new(StressState::new(
        producers.len() + consumers.len() + observers.len() + weak_observers.len(),
    ));
    // Check that the number of messages is divisible by the number of producers/consumers.
    assert_eq!(n_messages % producers.len(), 0);
    assert_eq!(n_messages % consumers.len(), 0);
    let n_messages_per_producer = n_messages / producers.len();
    let n_messages_per_consumer = n_messages / consumers.len();

    let consumer_threads = consumers
        .into_iter()
        .enumerate()
        .map(|(_t, c)| {
            let f = capture!([state], move || {
                let mut count = 0;
                let tracker = MessageTracker::new();
                state.wait_for_start();
                c.handle(capture!([tracker], move |v| {
                    black_box(v);
                    tracker.update(v);
                    count += 1;
                    // The each message is received by one consumer so we handle fewer per thread.
                    count >= n_messages_per_consumer
                }));
                state.complete();
                tracker
            });
            thread::spawn(f)
        })
        .collect::<Vec<_>>();

    let observer_threads = observers
        .into_iter()
        .enumerate()
        .map(|(_t, c)| {
            let f = capture!([state], move || {
                let mut count = 0;
                let tracker = MessageTracker::new();
                state.wait_for_start();
                c.handle(capture!([tracker], move |v| {
                    black_box(v);
                    tracker.update(v);
                    count += 1;
                    // Each message is received by all observers so we handle them all in every thread.
                    count >= n_messages
                }));
                state.complete();
                tracker
            });
            thread::spawn(f)
        })
        .collect::<Vec<_>>();

    let weak_observer_threads = weak_observers
        .into_iter()
        .enumerate()
        .map(|(_t, observer)| {
            let f = capture!([state], move || {
                let tracker = MessageTracker::new();
                state.wait_for_start();
                while observer.inner.recent_cursor().index() < (n_messages - 1) {
                    let first = observer.inner.oldest_cursor();
                    let last = observer.inner.recent_cursor();
                    for c in first..last {
                        if let Some(v) = observer.inner.weak_observe(c) {
                            tracker.update(v);
                        }
                    }
                    spin_sleep::sleep(observer.delay);
                }
                state.complete();
                tracker
            });
            thread::spawn(f)
        })
        .collect::<Vec<_>>();

    let producer_threads = producers
        .into_iter()
        .enumerate()
        .map(|(t, p)| {
            // let p = Arc::new(p);
            thread::spawn(capture!([state], move || {
                state.wait_for_start();
                let tracker = MessageTracker::new();
                for i in 0..n_messages_per_producer {
                    let v = StressMessage { src: t, x: i };
                    p.send(v);
                    tracker.update(v);
                    thread::sleep(delay);
                }
                state.complete();
                tracker
            }))
        })
        .collect::<Vec<_>>();

    let (duration, cpu_duration) = measure_run_time(|| {
        state.trigger_start();
        state.wait_for_complete();
    });

    let overall_produce_tracker = MessageTracker::new();
    for h in producer_threads {
        let tracker = h.join().expect("failed to join producer");
        overall_produce_tracker.combine(&tracker);
    }

    let overall_consume_tracker = MessageTracker::new();
    for h in consumer_threads {
        let tracker = h.join().expect("failed to join consumer");
        overall_consume_tracker.combine(&tracker);
    }

    assert_eq!(overall_produce_tracker.get(), overall_consume_tracker.get());

    for h in observer_threads {
        let tracker = h.join().expect("failed to join observer");
        assert_eq!(overall_produce_tracker.get(), tracker.get());
    }

    for h in weak_observer_threads {
        let tracker = h.join().expect("failed to join weak observer");
        println!("Observed messages with hash {}.", tracker.get());
    }

    ExecutionStatistics {
        n_messages,
        duration,
        cpu_duration,
    }
}

pub struct TableAttachments<
    P: TestProducer<StressMessage> + 'static,
    C: TestReceiver<StressMessage> + 'static,
    O: TestReceiver<StressMessage> + 'static,
    W: WeakObserver<StressMessage> + 'static,
> {
    pub producer: P,
    pub consumer: C,
    pub observers: Vec<O>,
    pub weak_observers: Vec<TestWeakObserver<W>>,
}

pub fn stress_ping_pong<
    P: TestProducer<StressMessage>,
    C: TestReceiver<StressMessage>,
    O: TestReceiver<StressMessage>,
    W: WeakObserver<StressMessage>,
>(
    call_table: TableAttachments<P, C, O, W>,
    reply_table: TableAttachments<P, C, O, W>,
    n_messages: usize,
    delay: Duration,
) -> ExecutionStatistics {
    let state = Arc::new(StressState::new(
        2 + call_table.observers.len()
            + call_table.weak_observers.len()
            + reply_table.observers.len()
            + reply_table.weak_observers.len(),
    ));

    // Put the first message into the call table before starting the threads. A thread will take ownership of the
    // producer, so we can't do it later.
    call_table.producer.send(StressMessage { src: 0, x: 0 });

    let make_thread = |consumer: C, producer: P| {
        let f = capture!([state], move || {
            let mut count = 0;
            let tracker = MessageTracker::new();
            state.wait_for_start();
            consumer.handle(capture!([tracker], move |v| {
                black_box(v);
                spin_sleep::SpinSleeper::default().with_spin_strategy(spin_sleep::SpinStrategy::SpinLoopHint).sleep(delay);
                producer.send(StressMessage { x: v.x + 1, ..v });
                tracker.update(v);
                count += 1;
                // The each message is received by one consumer so we handle fewer per thread.
                count >= n_messages
            }));
            state.complete();
            tracker
        });
        thread::spawn(f)
    };

    let ping_pong_threads = [
        make_thread(call_table.consumer, reply_table.producer),
        make_thread(reply_table.consumer, call_table.producer),
    ];

    let observer_threads = reply_table
        .observers
        .into_iter()
        .chain(call_table.observers.into_iter())
        .enumerate()
        .map(|(_t, c)| {
            let f = capture!([state], move || {
                let mut count = 0;
                let tracker = MessageTracker::new();
                state.wait_for_start();
                c.handle(capture!([tracker], move |v| {
                    black_box(v);
                    tracker.update(v);
                    count += 1;
                    // Each message is received by all observers so we handle them all in every thread.
                    count >= n_messages
                }));
                state.complete();
                tracker
            });
            thread::spawn(f)
        })
        .collect::<Vec<_>>();

    let weak_observer_threads = reply_table
        .weak_observers
        .into_iter()
        .chain(call_table.weak_observers.into_iter())
        .enumerate()
        .map(|(_t, observer)| {
            let f = capture!([state], move || {
                let tracker = MessageTracker::new();
                state.wait_for_start();
                while observer.inner.recent_cursor().index() < (n_messages - 1) {
                    let first = observer.inner.oldest_cursor();
                    let last = observer.inner.recent_cursor();
                    for c in first..last {
                        if let Some(v) = observer.inner.weak_observe(c) {
                            tracker.update(v);
                        }
                    }
                    spin_sleep::SpinSleeper::default().with_spin_strategy(spin_sleep::SpinStrategy::SpinLoopHint).sleep(observer.delay);
                }
                state.complete();
                tracker
            });
            thread::spawn(f)
        })
        .collect::<Vec<_>>();

    let (duration, cpu_duration) = measure_run_time(|| {
        state.trigger_start();
        state.wait_for_complete();
    });

    let overall_produce_tracker = MessageTracker::new();
    for h in ping_pong_threads {
        let tracker = h.join().expect("failed to join producer");
        overall_produce_tracker.combine(&tracker);
    }
    
    for h in observer_threads {
        let tracker = h.join().expect("failed to join observer");
        assert_eq!(overall_produce_tracker.get(), tracker.get());
    }

    for h in weak_observer_threads {
        let tracker = h.join().expect("failed to join weak observer");
        println!("Observed messages with hash {}.", tracker.get());
    }

    ExecutionStatistics {
        n_messages,
        duration,
        cpu_duration,
    }
}

#[derive(Debug)]
pub struct ExecutionStatistics {
    n_messages: usize,
    duration: Duration,
    cpu_duration: Duration,
}

impl Serialize for ExecutionStatistics {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        // 3 is the number of fields in the struct.
        let mut state = serializer.serialize_struct("ExecutionStatistics", 4)?;
        state.serialize_field("n_messages", &self.n_messages)?;

        state.serialize_field("duration", &self.duration)?;
        let per_message = self.duration.div_f64(self.n_messages as f64);
        // state.serialize_field("duration_per_message", &per_message)?;
        state.serialize_field("duration_per_message_human", &format!("{per_message:?}"))?;

        state.serialize_field("cpu_duration", &self.cpu_duration)?;
        let per_message = self.cpu_duration.div_f64(self.n_messages as f64);
        // state.serialize_field("cpu_duration_per_message", &per_message)?;
        state.serialize_field(
            "cpu_duration_per_message_human",
            &format!("{per_message:?}"),
        )?;

        state.end()
    }
}
