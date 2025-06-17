//! A trait and implementations for waiting for some event to occur. Instances are shared between threads and waiters
//! will call to wait for an event and notifiers can trigger waiters to wake up.

use std::{
    sync::{Arc, Condvar, Mutex},
    thread::{self, Thread}, time::Duration,
};

use ldos_tables::stubs::yield_thread;

pub trait Waiter {
    fn wait(&mut self);
}

struct FuncWaiter<F: FnMut()> {
    func: F,
}

impl<F: FnMut()> FuncWaiter<F> {
    fn new(func: F) -> Self {
        Self { func }
    }
}

impl<F: FnMut()> Waiter for FuncWaiter<F> {
    #[inline(always)]
    fn wait(&mut self) {
        (self.func)();
    }
}

pub trait WaitHandler: Send + Sync {
    fn new_waiter(&self) -> impl Waiter;
    fn wait_until(&self, p: impl Fn() -> bool) {
        let mut waiter = self.new_waiter();
        while !p() {
            waiter.wait();
        }
    }
    fn notify(&self);
}

/// A wait handler which waits using a condition variable and an associated mutex.
#[derive(Clone)]
pub struct WaitCondVar {
    condvar_mutex: Arc<(Condvar, Mutex<bool>)>,
}

impl WaitCondVar {
    pub fn new() -> Self {
        WaitCondVar {
            condvar_mutex: Arc::new((Condvar::new(), Mutex::new(false))),
        }
    }
}

impl WaitHandler for WaitCondVar {
    fn notify(&self) {
        let (condvar, mutex) = &*self.condvar_mutex;
        let guard = mutex.lock().unwrap();
        condvar.notify_all();
        drop(guard);
    }

    fn new_waiter(&self) -> impl Waiter {
        FuncWaiter::new(|| {
            let (condvar, mutex) = &*self.condvar_mutex;
            let guard = mutex.lock().unwrap();
            drop(condvar.wait(guard));
        })
    }
}

/// A wait handler which tracks waiting threads in a `Vec` (inside a `Mutex`). The waiting threads park themselves, and
/// notification unparks them.
#[derive(Clone)]
pub struct WaitPark {
    threads: Arc<Mutex<Vec<Thread>>>,
}

impl WaitPark {
    pub fn new() -> Self {
        WaitPark {
            threads: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

pub struct WaiterPark {
    threads: Arc<Mutex<Vec<Thread>>>,
}

impl WaiterPark {
    pub fn new(threads: Arc<Mutex<Vec<Thread>>>) -> Self {
        {
            let mut threads = threads.lock().unwrap();
            threads.push(thread::current());
        }

        Self { threads }
    }
}

impl Drop for WaiterPark {
    fn drop(&mut self) {
        let mut threads = self.threads.lock().unwrap();
        threads.retain(|t| t.id() != thread::current().id());
    }
}

impl Waiter for WaiterPark {
    fn wait(&mut self) {
        thread::park_timeout(Duration::from_nanos(1000));
    }
}

impl WaitHandler for WaitPark {
    fn notify(&self) {
        let threads = self.threads.lock().unwrap();
        for t in threads.iter() {
            t.unpark();
        }
    }

    fn new_waiter(&self) -> impl Waiter {
        WaiterPark::new(self.threads.clone())
    }
}

/// A wait handler which busy waits.
#[derive(Clone, Copy)]
pub struct WaitBusy {}

impl WaitBusy {
    pub fn new() -> Self {
        Self {}
    }
}

impl WaitHandler for WaitBusy {
    fn notify(&self) {}

    fn new_waiter(&self) -> impl Waiter {
        FuncWaiter::new(|| {
            // FIXME: Using a loop counter here is deemed incorrect by intel see
            // https://www.intel.com/content/www/us/en/developer/articles/technical/a-common-construct-to-avoid-the-contention-of-threads-architecture-agnostic-spin-wait-loops.html
            core::hint::spin_loop()
        })
    }
}

/// A wait handler which yields back to the OS thread scheduler.
#[derive(Clone, Copy)]
pub struct WaitYield {}

impl WaitYield {
    pub fn new() -> Self {
        Self {}
    }
}

impl WaitHandler for WaitYield {
    fn notify(&self) {}

    fn new_waiter(&self) -> impl Waiter {
        FuncWaiter::new(|| yield_thread())
    }
}

/// A wait handler which busy waits for a given number of loops and then call a different waiter.
#[derive(Clone, Copy)]
pub struct WaitBusyAndThen<N: WaitHandler> {
    next: N,
    n: u32,
}

impl<N: WaitHandler> WaitBusyAndThen<N> {
    pub fn new(next: N, n: u32) -> Self {
        Self { next, n }
    }
}

impl<N: WaitHandler> WaitHandler for WaitBusyAndThen<N> {
    fn new_waiter(&self) -> impl Waiter {
        let mut next_waiter = self.next.new_waiter();
        // FIXME: Using a loop counter here is deemed incorrect by intel see
        // https://www.intel.com/content/www/us/en/developer/articles/technical/a-common-construct-to-avoid-the-contention-of-threads-architecture-agnostic-spin-wait-loops.html
        let mut i = 0;
        FuncWaiter::new(move || {
            if i > self.n {
                next_waiter.wait();
            } else {
                core::hint::spin_loop();
            }
            i += 1;
        })
    }

    fn notify(&self) {
        self.next.notify();
    }
}
