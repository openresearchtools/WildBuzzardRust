//! Runtime-neutral cancellation, lifecycle, event, and task primitives.
//!
//! The crate deliberately does not choose an async executor, create threads,
//! read a clock, or call an operating-system API. Consumers can drive its
//! bounded queues from a Linux event loop or an async-runtime adapter later.

#![forbid(unsafe_code)]

use std::collections::VecDeque;
use std::error::Error;
use std::fmt;
use std::num::{NonZeroU64, NonZeroUsize};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Condvar, Mutex};

/// The error returned when cancelled work checks its token.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Cancelled;

impl fmt::Display for Cancelled {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("operation cancelled")
    }
}

impl Error for Cancelled {}

#[derive(Debug)]
struct CancellationInner {
    cancelled: AtomicBool,
    wait_lock: Mutex<()>,
    waiters: Condvar,
}

/// The owner that can request cancellation exactly once.
#[derive(Clone, Debug)]
pub struct CancellationSource {
    inner: Arc<CancellationInner>,
}

impl CancellationSource {
    /// Creates a source and a corresponding observation token.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(CancellationInner {
                cancelled: AtomicBool::new(false),
                wait_lock: Mutex::new(()),
                waiters: Condvar::new(),
            }),
        }
    }

    /// Returns a cheap observer for this cancellation state.
    #[must_use]
    pub fn token(&self) -> CancellationToken {
        CancellationToken {
            inner: Arc::clone(&self.inner),
        }
    }

    /// Requests cancellation. Exactly one racing caller receives `true`.
    pub fn cancel(&self) -> bool {
        let changed = self
            .inner
            .cancelled
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok();
        if changed {
            let guard = self
                .inner
                .wait_lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            self.inner.waiters.notify_all();
            drop(guard);
        }
        changed
    }
}

impl Default for CancellationSource {
    fn default() -> Self {
        Self::new()
    }
}

/// A read-only cancellation observer.
#[derive(Clone, Debug)]
pub struct CancellationToken {
    inner: Arc<CancellationInner>,
}

impl CancellationToken {
    /// Returns whether cancellation has been requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::Acquire)
    }

    /// Converts the current state into a convenient early-return result.
    pub fn check(&self) -> Result<(), Cancelled> {
        if self.is_cancelled() {
            Err(Cancelled)
        } else {
            Ok(())
        }
    }

    /// Blocks without polling until cancellation is observed.
    pub fn wait(&self) {
        if self.is_cancelled() {
            return;
        }
        let mut guard = self
            .inner
            .wait_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while !self.is_cancelled() {
            guard = self
                .inner
                .waiters
                .wait(guard)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }
}

/// Monotonic application or subsystem lifecycle states.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum LifecycleState {
    /// Constructed, but not accepting normal work.
    Created = 0,
    /// Started and accepting normal work.
    Running = 1,
    /// Shutdown has begun and cannot be reversed.
    Stopping = 2,
    /// Shutdown and cleanup have completed.
    Stopped = 3,
}

/// The result of an idempotent lifecycle operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransitionOutcome {
    /// State observed before the operation completed.
    pub previous: LifecycleState,
    /// State after the operation completed.
    pub current: LifecycleState,
    /// Whether this caller performed the transition.
    pub changed: bool,
}

/// A rejected non-monotonic lifecycle operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LifecycleError {
    /// Starting is valid only from `Created`; stopped components do not restart.
    CannotStart {
        /// State that rejected the operation.
        state: LifecycleState,
    },
    /// Finishing shutdown is valid only after shutdown has begun.
    ShutdownNotStarted {
        /// State that rejected the operation.
        state: LifecycleState,
    },
}

impl fmt::Display for LifecycleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CannotStart { state } => write!(formatter, "cannot start from {state:?}"),
            Self::ShutdownNotStarted { state } => {
                write!(formatter, "cannot finish shutdown from {state:?}")
            }
        }
    }
}

impl Error for LifecycleError {}

/// A lock-free monotonic lifecycle state machine.
#[derive(Debug)]
pub struct Lifecycle {
    state: AtomicU8,
}

impl Lifecycle {
    /// Creates a lifecycle in [`LifecycleState::Created`].
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: AtomicU8::new(LifecycleState::Created as u8),
        }
    }

    /// Returns the current lifecycle state.
    #[must_use]
    pub fn state(&self) -> LifecycleState {
        decode_lifecycle(self.state.load(Ordering::Acquire))
    }

    /// Starts the component. Concurrent or repeated starts are idempotent.
    pub fn start(&self) -> Result<TransitionOutcome, LifecycleError> {
        loop {
            let previous = self.state();
            match previous {
                LifecycleState::Created => {
                    if self
                        .state
                        .compare_exchange(
                            LifecycleState::Created as u8,
                            LifecycleState::Running as u8,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        return Ok(TransitionOutcome {
                            previous,
                            current: LifecycleState::Running,
                            changed: true,
                        });
                    }
                }
                LifecycleState::Running => {
                    return Ok(TransitionOutcome {
                        previous,
                        current: previous,
                        changed: false,
                    });
                }
                LifecycleState::Stopping | LifecycleState::Stopped => {
                    return Err(LifecycleError::CannotStart { state: previous });
                }
            }
        }
    }

    /// Begins irreversible shutdown from either `Created` or `Running`.
    pub fn begin_shutdown(&self) -> TransitionOutcome {
        loop {
            let previous = self.state();
            match previous {
                LifecycleState::Created | LifecycleState::Running => {
                    if self
                        .state
                        .compare_exchange(
                            previous as u8,
                            LifecycleState::Stopping as u8,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        return TransitionOutcome {
                            previous,
                            current: LifecycleState::Stopping,
                            changed: true,
                        };
                    }
                }
                LifecycleState::Stopping | LifecycleState::Stopped => {
                    return TransitionOutcome {
                        previous,
                        current: previous,
                        changed: false,
                    };
                }
            }
        }
    }

    /// Completes shutdown after all owned queues and resources are drained.
    pub fn finish_shutdown(&self) -> Result<TransitionOutcome, LifecycleError> {
        loop {
            let previous = self.state();
            match previous {
                LifecycleState::Stopping => {
                    if self
                        .state
                        .compare_exchange(
                            LifecycleState::Stopping as u8,
                            LifecycleState::Stopped as u8,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        return Ok(TransitionOutcome {
                            previous,
                            current: LifecycleState::Stopped,
                            changed: true,
                        });
                    }
                }
                LifecycleState::Stopped => {
                    return Ok(TransitionOutcome {
                        previous,
                        current: previous,
                        changed: false,
                    });
                }
                LifecycleState::Created | LifecycleState::Running => {
                    return Err(LifecycleError::ShutdownNotStarted { state: previous });
                }
            }
        }
    }
}

impl Default for Lifecycle {
    fn default() -> Self {
        Self::new()
    }
}

fn decode_lifecycle(value: u8) -> LifecycleState {
    match value {
        value if value == LifecycleState::Created as u8 => LifecycleState::Created,
        value if value == LifecycleState::Running as u8 => LifecycleState::Running,
        value if value == LifecycleState::Stopping as u8 => LifecycleState::Stopping,
        value if value == LifecycleState::Stopped as u8 => LifecycleState::Stopped,
        _ => unreachable!("Lifecycle stores only private enum discriminants"),
    }
}

/// State of a bounded event queue.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueueState {
    /// New events are accepted.
    Open,
    /// New events are rejected while existing events drain.
    Draining,
    /// New events are rejected and no queued events remain.
    Closed,
}

/// The result of an idempotent queue shutdown operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QueueTransitionOutcome {
    /// Queue state observed before the operation completed.
    pub previous: QueueState,
    /// Queue state after the operation completed.
    pub current: QueueState,
    /// Whether this caller performed a transition.
    pub changed: bool,
}

/// A failed event-queue operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueueError {
    /// The configured bounded capacity has been reached.
    Full {
        /// Maximum number of queued events.
        capacity: usize,
    },
    /// Shutdown has begun, so new events are rejected.
    Closed,
    /// A thread panicked while owning the queue lock.
    Poisoned,
}

impl fmt::Display for QueueError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full { capacity } => write!(formatter, "event queue capacity {capacity} reached"),
            Self::Closed => formatter.write_str("event queue is closed to new work"),
            Self::Poisoned => formatter.write_str("event queue lock is poisoned"),
        }
    }
}

impl Error for QueueError {}

/// The result of attempting to receive one event.
#[derive(Debug, Eq, PartialEq)]
pub enum PopResult<T> {
    /// One queued event, in FIFO order.
    Item(T),
    /// The queue remains open but currently contains no event.
    Empty,
    /// Shutdown is complete and no future event can arrive.
    Closed,
}

#[derive(Debug)]
struct QueueInner<T> {
    state: QueueState,
    events: VecDeque<T>,
}

/// A bounded, thread-safe, runtime-neutral FIFO event queue.
///
/// `EventQueue<T>` is `Send + Sync` when `T: Send`. Closing is deterministic:
/// producers are rejected immediately, existing events remain readable in FIFO
/// order, and the state becomes `Closed` when the last event is removed.
#[derive(Debug)]
pub struct EventQueue<T> {
    capacity: NonZeroUsize,
    inner: Mutex<QueueInner<T>>,
    ready: Condvar,
}

impl<T> EventQueue<T> {
    /// Creates an empty queue with an explicit non-zero bound.
    #[must_use]
    pub fn new(capacity: NonZeroUsize) -> Self {
        Self {
            capacity,
            inner: Mutex::new(QueueInner {
                state: QueueState::Open,
                events: VecDeque::new(),
            }),
            ready: Condvar::new(),
        }
    }

    /// Enqueues one event or reports backpressure/shutdown explicitly.
    pub fn try_push(&self, event: T) -> Result<(), QueueError> {
        let mut inner = self.inner.lock().map_err(|_| QueueError::Poisoned)?;
        if inner.state != QueueState::Open {
            return Err(QueueError::Closed);
        }
        if inner.events.len() == self.capacity.get() {
            return Err(QueueError::Full {
                capacity: self.capacity.get(),
            });
        }
        inner.events.push_back(event);
        self.ready.notify_one();
        Ok(())
    }

    /// Receives one event without blocking.
    pub fn try_pop(&self) -> Result<PopResult<T>, QueueError> {
        let mut inner = self.inner.lock().map_err(|_| QueueError::Poisoned)?;
        Ok(pop_locked(&mut inner))
    }

    /// Waits for one event or a fully drained shutdown.
    pub fn wait_pop(&self) -> Result<PopResult<T>, QueueError> {
        let mut inner = self.inner.lock().map_err(|_| QueueError::Poisoned)?;
        loop {
            match pop_locked(&mut inner) {
                PopResult::Empty => {
                    inner = self.ready.wait(inner).map_err(|_| QueueError::Poisoned)?;
                }
                result => return Ok(result),
            }
        }
    }

    /// Rejects new events and allows already queued events to drain.
    pub fn begin_shutdown(&self) -> Result<QueueTransitionOutcome, QueueError> {
        let mut inner = self.inner.lock().map_err(|_| QueueError::Poisoned)?;
        let previous = inner.state;
        if previous == QueueState::Open {
            inner.state = if inner.events.is_empty() {
                QueueState::Closed
            } else {
                QueueState::Draining
            };
        }
        let current = inner.state;
        self.ready.notify_all();
        Ok(QueueTransitionOutcome {
            previous,
            current,
            changed: previous != current,
        })
    }

    /// Returns the current queue state.
    pub fn state(&self) -> Result<QueueState, QueueError> {
        self.inner
            .lock()
            .map(|inner| inner.state)
            .map_err(|_| QueueError::Poisoned)
    }

    /// Returns the number of queued events.
    pub fn len(&self) -> Result<usize, QueueError> {
        self.inner
            .lock()
            .map(|inner| inner.events.len())
            .map_err(|_| QueueError::Poisoned)
    }

    /// Returns whether the queue contains no events.
    pub fn is_empty(&self) -> Result<bool, QueueError> {
        self.len().map(|len| len == 0)
    }
}

fn pop_locked<T>(inner: &mut QueueInner<T>) -> PopResult<T> {
    if let Some(event) = inner.events.pop_front() {
        if inner.events.is_empty() && inner.state == QueueState::Draining {
            inner.state = QueueState::Closed;
        }
        PopResult::Item(event)
    } else if inner.state == QueueState::Open {
        PopResult::Empty
    } else {
        inner.state = QueueState::Closed;
        PopResult::Closed
    }
}

/// A non-zero identity assigned to one accepted task.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TaskId(NonZeroU64);

impl TaskId {
    /// Returns the monotonically assigned local value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

struct ScheduledTask {
    id: TaskId,
    run: Box<dyn FnOnce() + Send + 'static>,
}

impl fmt::Debug for ScheduledTask {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ScheduledTask")
            .field("id", &self.id)
            .finish()
    }
}

/// A task-dispatch failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DispatchError {
    /// The bounded task queue is full.
    Full {
        /// Maximum number of queued tasks.
        capacity: usize,
    },
    /// Shutdown has begun.
    Closed,
    /// Every non-zero `u64` task identity has been issued.
    IdExhausted,
    /// The queue lock was poisoned.
    Poisoned,
}

impl fmt::Display for DispatchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full { capacity } => write!(formatter, "task queue capacity {capacity} reached"),
            Self::Closed => formatter.write_str("task queue is closed to new work"),
            Self::IdExhausted => formatter.write_str("task identity space exhausted"),
            Self::Poisoned => formatter.write_str("task queue lock is poisoned"),
        }
    }
}

impl Error for DispatchError {}

impl From<QueueError> for DispatchError {
    fn from(error: QueueError) -> Self {
        match error {
            QueueError::Full { capacity } => Self::Full { capacity },
            QueueError::Closed => Self::Closed,
            QueueError::Poisoned => Self::Poisoned,
        }
    }
}

/// Result of manually driving one queued task.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunResult {
    /// A task ran to completion.
    Ran(TaskId),
    /// The queue is open and currently idle.
    Idle,
    /// The queue was shut down and has fully drained.
    Closed,
}

/// A bounded FIFO task queue driven explicitly by its consumer.
///
/// Tasks are `Send + 'static`; the queue and shared references to it are
/// `Send + Sync`. The crate does not decide which thread runs a task.
#[derive(Debug)]
pub struct ManualTaskQueue {
    capacity: NonZeroUsize,
    inner: Mutex<ManualTaskQueueInner>,
}

#[derive(Debug)]
struct ManualTaskQueueInner {
    next_id: Option<NonZeroU64>,
    state: QueueState,
    tasks: VecDeque<ScheduledTask>,
}

impl ManualTaskQueue {
    /// Creates an empty task queue with an explicit capacity.
    #[must_use]
    pub fn new(capacity: NonZeroUsize) -> Self {
        Self {
            capacity,
            inner: Mutex::new(ManualTaskQueueInner {
                next_id: Some(NonZeroU64::MIN),
                state: QueueState::Open,
                tasks: VecDeque::new(),
            }),
        }
    }

    /// Accepts a task without selecting or waking a runtime executor.
    pub fn dispatch<F>(&self, task: F) -> Result<TaskId, DispatchError>
    where
        F: FnOnce() + Send + 'static,
    {
        let mut inner = self.inner.lock().map_err(|_| DispatchError::Poisoned)?;
        if inner.state != QueueState::Open {
            return Err(DispatchError::Closed);
        }
        if inner.tasks.len() == self.capacity.get() {
            return Err(DispatchError::Full {
                capacity: self.capacity.get(),
            });
        }
        let id = TaskId(inner.next_id.ok_or(DispatchError::IdExhausted)?);
        inner.next_id = NonZeroU64::new(id.get().wrapping_add(1));
        inner.tasks.push_back(ScheduledTask {
            id,
            run: Box::new(task),
        });
        Ok(id)
    }

    /// Runs at most one task on the calling thread.
    pub fn run_one(&self) -> Result<RunResult, QueueError> {
        let task = {
            let mut inner = self.inner.lock().map_err(|_| QueueError::Poisoned)?;
            if let Some(task) = inner.tasks.pop_front() {
                if inner.tasks.is_empty() && inner.state == QueueState::Draining {
                    inner.state = QueueState::Closed;
                }
                Some(task)
            } else if inner.state == QueueState::Open {
                return Ok(RunResult::Idle);
            } else {
                inner.state = QueueState::Closed;
                return Ok(RunResult::Closed);
            }
        };
        match task {
            Some(task) => {
                let id = task.id;
                (task.run)();
                Ok(RunResult::Ran(id))
            }
            None => unreachable!("all empty task-queue states return before task execution"),
        }
    }

    /// Rejects new tasks and leaves accepted tasks available to drain.
    pub fn begin_shutdown(&self) -> Result<QueueTransitionOutcome, QueueError> {
        let mut inner = self.inner.lock().map_err(|_| QueueError::Poisoned)?;
        let previous = inner.state;
        if previous == QueueState::Open {
            inner.state = if inner.tasks.is_empty() {
                QueueState::Closed
            } else {
                QueueState::Draining
            };
        }
        let current = inner.state;
        Ok(QueueTransitionOutcome {
            previous,
            current,
            changed: previous != current,
        })
    }

    /// Begins shutdown and deterministically runs accepted tasks in FIFO order.
    pub fn shutdown_and_drain(&self) -> Result<Vec<TaskId>, QueueError> {
        let _ = self.begin_shutdown()?;
        let mut completed = Vec::new();
        loop {
            match self.run_one()? {
                RunResult::Ran(id) => completed.push(id),
                RunResult::Idle => {
                    unreachable!("a draining task queue cannot become open and idle")
                }
                RunResult::Closed => return Ok(completed),
            }
        }
    }

    /// Returns the underlying queue state.
    pub fn state(&self) -> Result<QueueState, QueueError> {
        self.inner
            .lock()
            .map(|inner| inner.state)
            .map_err(|_| QueueError::Poisoned)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CancellationSource, DispatchError, EventQueue, Lifecycle, LifecycleError, LifecycleState,
        ManualTaskQueue, PopResult, QueueError, QueueState, RunResult,
    };
    use std::num::NonZeroUsize;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier, Mutex};
    use std::thread;

    fn capacity(value: usize) -> NonZeroUsize {
        NonZeroUsize::new(value).unwrap()
    }

    #[test]
    fn exactly_one_racing_caller_requests_cancellation() {
        let source = Arc::new(CancellationSource::new());
        let token = source.token();
        let barrier = Arc::new(Barrier::new(17));
        let winners = Arc::new(AtomicUsize::new(0));
        let mut threads = Vec::new();

        for _ in 0..16 {
            let source = Arc::clone(&source);
            let barrier = Arc::clone(&barrier);
            let winners = Arc::clone(&winners);
            threads.push(thread::spawn(move || {
                barrier.wait();
                if source.cancel() {
                    winners.fetch_add(1, Ordering::AcqRel);
                }
            }));
        }
        barrier.wait();
        for thread in threads {
            thread.join().unwrap();
        }

        assert_eq!(winners.load(Ordering::Acquire), 1);
        assert!(token.is_cancelled());
        assert!(token.check().is_err());
    }

    #[test]
    fn cancellation_wait_has_no_lost_wakeup() {
        let source = CancellationSource::new();
        let token = source.token();
        let waiter = thread::spawn(move || token.wait());
        assert!(source.cancel());
        waiter.join().unwrap();
    }

    #[test]
    fn lifecycle_shutdown_race_has_one_transition_winner() {
        let lifecycle = Arc::new(Lifecycle::new());
        assert!(lifecycle.start().unwrap().changed);
        let barrier = Arc::new(Barrier::new(17));
        let winners = Arc::new(AtomicUsize::new(0));
        let mut threads = Vec::new();

        for _ in 0..16 {
            let lifecycle = Arc::clone(&lifecycle);
            let barrier = Arc::clone(&barrier);
            let winners = Arc::clone(&winners);
            threads.push(thread::spawn(move || {
                barrier.wait();
                if lifecycle.begin_shutdown().changed {
                    winners.fetch_add(1, Ordering::AcqRel);
                }
            }));
        }
        barrier.wait();
        for thread in threads {
            thread.join().unwrap();
        }

        assert_eq!(winners.load(Ordering::Acquire), 1);
        assert_eq!(lifecycle.state(), LifecycleState::Stopping);
        assert!(lifecycle.finish_shutdown().unwrap().changed);
        assert!(!lifecycle.finish_shutdown().unwrap().changed);
        assert_eq!(lifecycle.state(), LifecycleState::Stopped);
        assert_eq!(
            lifecycle.start(),
            Err(LifecycleError::CannotStart {
                state: LifecycleState::Stopped,
            })
        );
    }

    #[test]
    fn bounded_event_queue_applies_backpressure_and_drains_fifo() {
        let queue = EventQueue::new(capacity(2));
        queue.try_push(1).unwrap();
        queue.try_push(2).unwrap();
        assert_eq!(queue.try_push(3), Err(QueueError::Full { capacity: 2 }));

        assert!(queue.begin_shutdown().unwrap().changed);
        assert_eq!(queue.state().unwrap(), QueueState::Draining);
        assert_eq!(queue.try_push(3), Err(QueueError::Closed));
        assert_eq!(queue.try_pop().unwrap(), PopResult::Item(1));
        assert_eq!(queue.try_pop().unwrap(), PopResult::Item(2));
        assert_eq!(queue.state().unwrap(), QueueState::Closed);
        assert_eq!(queue.try_pop().unwrap(), PopResult::Closed);
    }

    #[test]
    fn task_shutdown_is_deterministic_and_rejects_late_dispatch() {
        let queue = ManualTaskQueue::new(capacity(4));
        let observed = Arc::new(Mutex::new(Vec::new()));
        let mut expected_ids = Vec::new();

        for value in [10, 20, 30] {
            let observed = Arc::clone(&observed);
            expected_ids.push(
                queue
                    .dispatch(move || observed.lock().unwrap().push(value))
                    .unwrap(),
            );
        }

        let completed = queue.shutdown_and_drain().unwrap();
        assert_eq!(completed, expected_ids);
        assert_eq!(*observed.lock().unwrap(), vec![10, 20, 30]);
        assert_eq!(queue.state().unwrap(), QueueState::Closed);
        assert_eq!(queue.dispatch(|| {}), Err(DispatchError::Closed));
        assert_eq!(queue.run_one().unwrap(), RunResult::Closed);
    }

    #[test]
    fn rejected_dispatch_does_not_consume_a_task_id() {
        let queue = ManualTaskQueue::new(capacity(1));
        let first = queue.dispatch(|| {}).unwrap();
        assert_eq!(first.get(), 1);
        assert_eq!(
            queue.dispatch(|| {}),
            Err(DispatchError::Full { capacity: 1 })
        );
        assert_eq!(queue.run_one().unwrap(), RunResult::Ran(first));

        let second = queue.dispatch(|| {}).unwrap();
        assert_eq!(second.get(), 2);
        let _ = queue.begin_shutdown().unwrap();
        assert_eq!(queue.dispatch(|| {}), Err(DispatchError::Closed));
        assert_eq!(queue.run_one().unwrap(), RunResult::Ran(second));
        assert_eq!(queue.run_one().unwrap(), RunResult::Closed);
    }

    #[test]
    fn public_concurrency_primitives_are_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<CancellationSource>();
        assert_send_sync::<Lifecycle>();
        assert_send_sync::<EventQueue<String>>();
        assert_send_sync::<ManualTaskQueue>();
    }
}
