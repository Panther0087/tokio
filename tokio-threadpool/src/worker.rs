use inner::Inner;
use notifier::Notifier;
use sender::Sender;
use state::State;
use task::Task;
use worker_entry::WorkerEntry;
use worker_state::{
    WorkerState,
    WORKER_SHUTDOWN,
    WORKER_RUNNING,
    WORKER_SLEEPING,
    WORKER_NOTIFIED,
    WORKER_SIGNALED,
};

use std::cell::Cell;
use std::marker::PhantomData;
use std::rc::Rc;
use std::thread;
use std::time::Instant;
use std::sync::atomic::Ordering::{AcqRel, Acquire};
use std::sync::Arc;

use tokio_executor;

/// Thread worker
///
/// This is passed to the `around_worker` callback set on `Builder`. This
/// callback is only expected to call `run` on it.
#[derive(Debug)]
pub struct Worker {
    // Shared scheduler data
    pub(crate) inner: Arc<Inner>,

    // WorkerEntry index
    pub(crate) idx: usize,

    // Set when the worker should finalize on drop
    should_finalize: Cell<bool>,

    // Keep the value on the current thread.
    _p: PhantomData<Rc<()>>,
}

impl Worker {
    pub(crate) fn spawn(idx: usize, inner: &Arc<Inner>) {
        trace!("spawning new worker thread; idx={}", idx);

        let mut th = thread::Builder::new();

        if let Some(ref prefix) = inner.config.name_prefix {
            th = th.name(format!("{}{}", prefix, idx));
        }

        if let Some(stack) = inner.config.stack_size {
            th = th.stack_size(stack);
        }

        let inner = inner.clone();

        th.spawn(move || {
            let worker = Worker {
                inner: inner,
                idx: idx,
                should_finalize: Cell::new(false),
                _p: PhantomData,
            };

            // Make sure the ref to the worker does not move
            let wref = &worker;

            // Create another worker... It's ok, this is just a new type around
            // `Inner` that is expected to stay on the current thread.
            CURRENT_WORKER.with(|c| {
                c.set(wref as *const _);

                let inner = wref.inner.clone();
                let mut sender = Sender { inner };

                // Enter an execution context
                let mut enter = tokio_executor::enter().unwrap();

                tokio_executor::with_default(&mut sender, &mut enter, |enter| {
                    if let Some(ref callback) = wref.inner.config.around_worker {
                        callback.call(wref, enter);
                    } else {
                        wref.run();
                    }
                });
            });
        }).unwrap();
    }

    pub(crate) fn with_current<F: FnOnce(Option<&Worker>) -> R, R>(f: F) -> R {
        CURRENT_WORKER.with(move |c| {
            let ptr = c.get();

            if ptr.is_null() {
                f(None)
            } else {
                f(Some(unsafe { &*ptr }))
            }
        })
    }

    /// Run the worker
    ///
    /// This function blocks until the worker is shutting down.
    pub fn run(&self) {
        // Get the notifier.
        let notify = Arc::new(Notifier {
            inner: Arc::downgrade(&self.inner),
        });
        let mut sender = Sender { inner: self.inner.clone() };

        let mut first = true;
        let mut spin_cnt = 0;

        while self.check_run_state(first) {
            first = false;

            // Poll inbound until empty, transfering all tasks to the internal
            // queue.
            let consistent = self.drain_inbound();

            // Run the next available task
            if self.try_run_task(&notify, &mut sender) {
                spin_cnt = 0;
                // As long as there is work, keep looping.
                continue;
            }

            // No work in this worker's queue, it is time to try stealing.
            if self.try_steal_task(&notify, &mut sender) {
                spin_cnt = 0;
                continue;
            }

            if !consistent {
                spin_cnt = 0;
                continue;
            }

            // Starting to get sleeeeepy
            if spin_cnt < 32 {
                spin_cnt += 1;

                // Don't do anything further
            } else if spin_cnt < 256 {
                spin_cnt += 1;

                // Yield the thread
                thread::yield_now();
            } else {
                if !self.sleep() {
                    return;
                }
            }

            // If there still isn't any work to do, shutdown the worker?
        }

        self.should_finalize.set(true);
    }

    /// Checks the worker's current state, updating it as needed.
    ///
    /// Returns `true` if the worker should run.
    #[inline]
    fn check_run_state(&self, first: bool) -> bool {
        let mut state: WorkerState = self.entry().state.load(Acquire).into();

        loop {
            let pool_state: State = self.inner.state.load(Acquire).into();

            if pool_state.is_terminated() {
                return false;
            }

            let mut next = state;

            match state.lifecycle() {
                WORKER_RUNNING => break,
                WORKER_NOTIFIED | WORKER_SIGNALED => {
                    // transition back to running
                    next.set_lifecycle(WORKER_RUNNING);
                }
                lifecycle => panic!("unexpected worker state; lifecycle={}", lifecycle),
            }

            let actual = self.entry().state.compare_and_swap(
                state.into(), next.into(), AcqRel).into();

            if actual == state {
                break;
            }

            state = actual;
        }

        // If this is the first iteration of the worker loop, then the state can
        // be signaled.
        if !first && state.is_signaled() {
            trace!("Worker::check_run_state; delegate signal");
            // This worker is not ready to be signaled, so delegate the signal
            // to another worker.
            self.inner.signal_work(&self.inner);
        }

        true
    }

    /// Runs the next task on this worker's queue.
    ///
    /// Returns `true` if work was found.
    #[inline]
    fn try_run_task(&self, notify: &Arc<Notifier>, sender: &mut Sender) -> bool {
        use deque::Steal::*;

        // Poll the internal queue for a task to run
        match self.entry().deque.steal() {
            Data(task) => {
                self.run_task(task, notify, sender);
                true
            }
            Empty => false,
            Retry => true,
        }
    }

    /// Tries to steal a task from another worker.
    ///
    /// Returns `true` if work was found
    #[inline]
    fn try_steal_task(&self, notify: &Arc<Notifier>, sender: &mut Sender) -> bool {
        use deque::Steal::*;

        let len = self.inner.workers.len();
        let mut idx = self.inner.rand_usize() % len;
        let mut found_work = false;
        let start = idx;

        loop {
            if idx < len {
                match self.inner.workers[idx].steal.steal() {
                    Data(task) => {
                        trace!("stole task");

                        self.run_task(task, notify, sender);

                        trace!("try_steal_task -- signal_work; self={}; from={}",
                               self.idx, idx);

                        // Signal other workers that work is available
                        self.inner.signal_work(&self.inner);

                        return true;
                    }
                    Empty => {}
                    Retry => found_work = true,
                }

                idx += 1;
            } else {
                idx = 0;
            }

            if idx == start {
                break;
            }
        }

        found_work
    }

    fn run_task(&self, task: Task, notify: &Arc<Notifier>, sender: &mut Sender) {
        use task::Run::*;

        match task.run(notify, sender) {
            Idle => {}
            Schedule => {
                self.entry().push_internal(task);
            }
            Complete => {
                let mut state: State = self.inner.state.load(Acquire).into();

                loop {
                    let mut next = state;
                    next.dec_num_futures();

                    let actual = self.inner.state.compare_and_swap(
                        state.into(), next.into(), AcqRel).into();

                    if actual == state {
                        trace!("task complete; state={:?}", next);

                        if state.num_futures() == 1 {
                            // If the thread pool has been flagged as shutdown,
                            // start terminating workers. This involves waking
                            // up any sleeping worker so that they can notice
                            // the shutdown state.
                            if next.is_terminated() {
                                self.inner.terminate_sleeping_workers();
                            }
                        }

                        // The worker's run loop will detect the shutdown state
                        // next iteration.
                        return;
                    }

                    state = actual;
                }
            }
        }
    }

    /// Drains all tasks on the extern queue and pushes them onto the internal
    /// queue.
    ///
    /// Returns `true` if the operation was able to complete in a consistent
    /// state.
    #[inline]
    fn drain_inbound(&self) -> bool {
        use task::Poll::*;

        let mut found_work = false;

        loop {
            let task = unsafe { self.entry().inbound.poll() };

            match task {
                Empty => {
                    if found_work {
                        trace!("found work while draining; signal_work");
                        self.inner.signal_work(&self.inner);
                    }

                    return true;
                }
                Inconsistent => {
                    if found_work {
                        trace!("found work while draining; signal_work");
                        self.inner.signal_work(&self.inner);
                    }

                    return false;
                }
                Data(task) => {
                    found_work = true;
                    self.entry().push_internal(task);
                }
            }
        }
    }

    /// Put the worker to sleep
    ///
    /// Returns `true` if woken up due to new work arriving.
    #[inline]
    fn sleep(&self) -> bool {
        trace!("Worker::sleep; idx={}", self.idx);

        let mut state: WorkerState = self.entry().state.load(Acquire).into();

        // The first part of the sleep process is to transition the worker state
        // to "pushed". Now, it may be that the worker is already pushed on the
        // sleeper stack, in which case, we don't push again. However, part of
        // this process is also to do some final state checks to avoid entering
        // the mutex if at all possible.

        loop {
            let mut next = state;

            match state.lifecycle() {
                WORKER_RUNNING => {
                    // Try setting the pushed state
                    next.set_pushed();
                }
                WORKER_NOTIFIED | WORKER_SIGNALED => {
                    // No need to sleep, transition back to running and move on.
                    next.set_lifecycle(WORKER_RUNNING);
                }
                actual => panic!("unexpected worker state; {}", actual),
            }

            let actual = self.entry().state.compare_and_swap(
                state.into(), next.into(), AcqRel).into();

            if actual == state {
                if state.is_notified() {
                    // The previous state was notified, so we don't need to
                    // sleep.
                    return true;
                }

                if !state.is_pushed() {
                    debug_assert!(next.is_pushed());

                    trace!("  sleeping -- push to stack; idx={}", self.idx);

                    // We obtained permission to push the worker into the
                    // sleeper queue.
                    if let Err(_) = self.inner.push_sleeper(self.idx) {
                        trace!("  sleeping -- push to stack failed; idx={}", self.idx);
                        // The push failed due to the pool being terminated.
                        //
                        // This is true because the "work" being woken up for is
                        // shutting down.
                        return true;
                    }
                }

                break;
            }

            state = actual;
        }

        // Acquire the sleep mutex, the state is transitioned to sleeping within
        // the mutex in order to avoid losing wakeup notifications.
        let mut lock = self.entry().park_mutex.lock().unwrap();

        // Transition the state to sleeping, a CAS is still needed as other
        // state transitions could happen unrelated to the sleep / wakeup
        // process. We also have to redo the lifecycle check done above as
        // the state could have been transitioned before entering the mutex.
        loop {
            let mut next = state;

            match state.lifecycle() {
                WORKER_RUNNING => {}
                WORKER_NOTIFIED | WORKER_SIGNALED => {
                    // Release the lock, sleep will not happen this call.
                    drop(lock);

                    // Transition back to running
                    loop {
                        let mut next = state;
                        next.set_lifecycle(WORKER_RUNNING);

                        let actual = self.entry().state.compare_and_swap(
                            state.into(), next.into(), AcqRel).into();

                        if actual == state {
                            return true;
                        }

                        state = actual;
                    }
                }
                _ => unreachable!(),
            }

            trace!(" sleeping -- set WORKER_SLEEPING; idx={}", self.idx);

            next.set_lifecycle(WORKER_SLEEPING);

            let actual = self.entry().state.compare_and_swap(
                state.into(), next.into(), AcqRel).into();

            if actual == state {
                break;
            }

            state = actual;
        }

        trace!("    -> starting to sleep; idx={}", self.idx);

        let sleep_until = self.inner.config.keep_alive
            .map(|dur| Instant::now() + dur);

        // The state has been transitioned to sleeping, we can now wait on the
        // condvar. This is done in a loop as condvars can wakeup spuriously.
        loop {
            let mut drop_thread = false;

            lock = match sleep_until {
                Some(when) => {
                    let now = Instant::now();

                    if when >= now {
                        drop_thread = true;
                    }

                    let dur = when - now;

                    self.entry().park_condvar
                        .wait_timeout(lock, dur)
                        .unwrap().0
                }
                None => {
                    self.entry().park_condvar.wait(lock).unwrap()
                }
            };

            trace!("    -> wakeup; idx={}", self.idx);

            // Reload the state
            state = self.entry().state.load(Acquire).into();

            loop {
                match state.lifecycle() {
                    WORKER_SLEEPING => {}
                    WORKER_NOTIFIED | WORKER_SIGNALED => {
                        // Release the lock, done sleeping
                        drop(lock);

                        // Transition back to running
                        loop {
                            let mut next = state;
                            next.set_lifecycle(WORKER_RUNNING);

                            let actual = self.entry().state.compare_and_swap(
                                state.into(), next.into(), AcqRel).into();

                            if actual == state {
                                return true;
                            }

                            state = actual;
                        }
                    }
                    _ => unreachable!(),
                }

                if !drop_thread {
                    break;
                }

                let mut next = state;
                next.set_lifecycle(WORKER_SHUTDOWN);

                let actual = self.entry().state.compare_and_swap(
                    state.into(), next.into(), AcqRel).into();

                if actual == state {
                    // Transitioned to a shutdown state
                    return false;
                }

                state = actual;
            }

            // The worker hasn't been notified, go back to sleep
        }
    }

    fn entry(&self) -> &WorkerEntry {
        &self.inner.workers[self.idx]
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        trace!("shutting down thread; idx={}", self.idx);

        if self.should_finalize.get() {
            // Drain all work
            self.drain_inbound();

            while let Some(_) = self.entry().deque.pop() {
            }

            // TODO: Drain the work queue...
            self.inner.worker_terminated();
        }
    }
}

// Pointer to the current worker info
thread_local!(static CURRENT_WORKER: Cell<*const Worker> = Cell::new(0 as *const _));
