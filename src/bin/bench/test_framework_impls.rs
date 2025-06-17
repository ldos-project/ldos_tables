use std::cell::Cell;

use std::sync::Mutex;
use std::sync::{mpsc, Condvar};

use std::sync::Arc;

use capture_it::capture;
use ldos_tables::stubs::yield_thread;
use ldos_tables::table_ipc::{Consumer, Producer, StrongObserver};

use std::marker::PhantomData;

use crate::generic_stress::{TestProducer, TestReceiver};
use crate::wait_handler::{WaitHandler, Waiter};

pub(crate) const N_BUSY_LOOPS: usize = 64;

impl<T: Copy + Send> TestProducer<T> for mpsc::Sender<T> {
    fn send(&self, v: T) {
        self.send(v).unwrap()
    }
}

impl<T: Copy + Send> TestProducer<T> for mpsc::SyncSender<T> {
    fn send(&self, v: T) {
        self.send(v).unwrap();
    }
}

impl<T: Copy + Send> TestReceiver<T> for mpsc::Receiver<T> {
    fn handle(self, mut f: impl FnMut(T) -> bool + Send + 'static) {
        loop {
            if f(self.recv().unwrap()) {
                break;
            }
        }
    }
}

pub(crate) struct NullReceiver;

impl<T: Copy + Send> TestReceiver<T> for NullReceiver {
    fn handle(self, mut _f: impl FnMut(T) -> bool + Send + 'static) {
        unreachable!()
    }
}

impl<T: Copy + Send> TestProducer<T> for kanal::Sender<T> {
    fn send(&self, v: T) {
        self.send(v).unwrap();
    }
}

impl<T: Copy + Send> TestReceiver<T> for kanal::Receiver<T> {
    fn handle(self, mut f: impl FnMut(T) -> bool + Send + 'static) {
        loop {
            if f(self.recv().unwrap()) {
                break;
            }
        }
    }
}

pub(crate) struct WaitProducer<T: Copy + Send, P: Producer<T>, W: WaitHandler> {
    pub(crate) inner: P,
    pub(crate) wait_handler: W,
    pub(crate) _phantom: PhantomData<T>,
}

impl<T: Copy + Send, P: Producer<T>, W: WaitHandler> WaitProducer<T, P, W> {
    pub(crate) fn new(inner: P, wait_handler: W) -> Self {
        Self {
            inner,
            wait_handler,
            _phantom: PhantomData,
        }
    }
}

impl<T: Copy + Send, P: Producer<T>, W: WaitHandler> TestProducer<T> for WaitProducer<T, P, W> {
    fn send(&self, v: T) {
        let mut waiter = self.wait_handler.new_waiter();
        let mut v = Some(v);
        while let Some(v0) = self.inner.try_put(v.take().unwrap()) {
            v.replace(v0);
            waiter.wait();
        }
        self.wait_handler.notify();
    }
}

pub(crate) struct WaitConsumer<T: Copy + Send, C: Consumer<T>, W: WaitHandler> {
    pub(crate) inner: C,
    pub(crate) wait_handler: W,
    pub(crate) _phantom: PhantomData<T>,
}

impl<T: Copy + Send, C: Consumer<T>, W: WaitHandler> WaitConsumer<T, C, W> {
    pub(crate) fn new(inner: C, wait_handler: W) -> Self {
        Self {
            inner,
            wait_handler,
            _phantom: PhantomData,
        }
    }
}

impl<T: Copy + Send, C: Consumer<T>, W: WaitHandler> TestReceiver<T> for WaitConsumer<T, C, W> {
    fn handle(self, mut f: impl FnMut(T) -> bool + Send + 'static) {
        let mut waiter = self.wait_handler.new_waiter();
        loop {
            if let Some(v) = self.inner.try_take() {
                if f(v) {
                    break;
                }
                // Replace the waiter to reset any internal state used for each overall wait.
                waiter = self.wait_handler.new_waiter();
                self.wait_handler.notify();
            } else {
                waiter.wait();
            }
        }
    }
}

pub(crate) struct WaitStrongObserver<T: Copy + Send, C: StrongObserver<T>, W: WaitHandler> {
    pub(crate) inner: C,
    pub(crate) wait_handler: W,
    pub(crate) _phantom: PhantomData<T>,
}

impl<T: Copy + Send, C: StrongObserver<T>, W: WaitHandler> WaitStrongObserver<T, C, W> {
    pub(crate) fn new(inner: C, wait_handler: W) -> Self {
        Self {
            inner,
            wait_handler,
            _phantom: PhantomData,
        }
    }
}

impl<T: Copy + Send, C: StrongObserver<T>, W: WaitHandler> TestReceiver<T>
    for WaitStrongObserver<T, C, W>
{
    fn handle(self, mut f: impl FnMut(T) -> bool + Send + 'static) {
        let mut waiter = self.wait_handler.new_waiter();
        loop {
            if let Some(v) = self.inner.try_strong_observe() {
                if f(v) {
                    break;
                }
                // Replace the waiter to reset any internal state used for each overall wait.
                waiter = self.wait_handler.new_waiter();
                self.wait_handler.notify();
            } else {
                waiter.wait();
            }
        }
    }
}

pub(crate) struct CallProducer<T: Copy + Send, P: Producer<T>> {
    pub(crate) inner: P,
    pub(crate) callback: Arc<Mutex<Option<Box<dyn FnMut() -> bool + Send>>>>,
    pub(crate) _phantom: PhantomData<T>,
}

impl<T: Copy + Send, P: Producer<T>> CallProducer<T, P> {
    pub(crate) fn new(
        inner: P,
        callback: Arc<Mutex<Option<Box<dyn FnMut() -> bool + Send>>>>,
    ) -> Self {
        Self {
            inner,
            callback,
            _phantom: PhantomData,
        }
    }
}

pub(crate) struct CallConsumer<T: Copy + Send, C: Consumer<T>> {
    pub(crate) inner: C,
    pub(crate) callback: Arc<Mutex<Option<Box<dyn FnMut() -> bool + Send>>>>,
    pub(crate) _phantom: PhantomData<T>,
}

impl<T: Copy + Send, C: Consumer<T>> CallConsumer<T, C> {
    pub(crate) fn new(
        inner: C,
        callback: Arc<Mutex<Option<Box<dyn FnMut() -> bool + Send>>>>,
    ) -> Self {
        Self {
            inner,
            callback,
            _phantom: PhantomData,
        }
    }
}

impl<T: Copy + Send, P: Producer<T>> TestProducer<T> for CallProducer<T, P> {
    fn send(&self, v: T) {
        let mut v = Some(v);
        let mut i = 0;
        while let Some(v0) = self.inner.try_put(v.take().unwrap()) {
            v.replace(v0);
            if i > N_BUSY_LOOPS {
                yield_thread();
            } else {
                core::hint::spin_loop();
            }
            i += 1;
        }
        loop {
            let mut guard = self.callback.lock().unwrap();
            if let Some(callback) = guard.as_mut() {
                callback();
                break;
            }
        }
    }
}

impl<T: Copy + Send, C: Consumer<T> + 'static> TestReceiver<T> for CallConsumer<T, C> {
    fn handle(self, mut f: impl FnMut(T) -> bool + Send + 'static) {
        let mutex = Arc::new(Mutex::new(Cell::new(false)));
        let condvar = Arc::new(Condvar::new());
        let guard = mutex.lock().unwrap();
        self.callback
            .lock()
            .unwrap()
            .replace(Box::new(capture!([mutex, condvar], move || {
                if let Some(v) = self.inner.try_take() {
                    let b = f(v);
                    if b {
                        let guard = mutex.lock().unwrap();
                        guard.set(true);
                        condvar.notify_all();
                    }
                    b
                } else {
                    false
                }
            })));

        drop(condvar.wait_while(guard, |b| !b.get()).unwrap());
    }
}

#[derive(Clone)]
pub(crate) struct DirectCallProducerConsumer<T: Copy + Send> {
    pub(crate) callback: Arc<Mutex<Option<Box<dyn FnMut(T) -> bool + Send>>>>,
    pub(crate) _phantom: PhantomData<T>,
}

impl<T: Copy + Send> DirectCallProducerConsumer<T> {
    pub(crate) fn new(callback: Arc<Mutex<Option<Box<dyn FnMut(T) -> bool + Send>>>>) -> Self {
        Self {
            callback,
            _phantom: PhantomData,
        }
    }
}

impl<T: Copy + Send> TestProducer<T> for DirectCallProducerConsumer<T> {
    fn send(&self, v: T) {
        loop {
            let mut guard = self.callback.lock().unwrap();
            if let Some(callback) = guard.as_mut() {
                callback(v);
                break;
            }
        }
    }
}

impl<T: Copy + Send> TestReceiver<T> for DirectCallProducerConsumer<T> {
    fn handle(self, mut f: impl FnMut(T) -> bool + Send + 'static) {
        let mutex = Arc::new(Mutex::new(Cell::new(false)));
        let condvar = Arc::new(Condvar::new());
        let guard = mutex.lock().unwrap();
        self.callback
            .lock()
            .unwrap()
            .replace(Box::new(capture!([mutex, condvar], move |v| {
                let b = f(v);
                if b {
                    let guard = mutex.lock().unwrap();
                    guard.set(true);
                    condvar.notify_all();
                }
                b
            })));

        drop(condvar.wait_while(guard, |b| !b.get()).unwrap());
    }
}
