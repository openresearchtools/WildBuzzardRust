use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, ThreadId};

use wild_buzzard_engine::{
    CommandErrorKind, CommandReceipt, EngineCommand, EngineEvent, EngineEventKind,
    EngineEventReceiver, EngineFrame, EngineLimits, EngineStartError, EventReceiveError,
    ExecutionFailure, ExecutionFailureKind, ExecutorOutput, ExecutorShutdownStatus,
    FrameComposition, FrameLeaseError, NavigationEngine, NavigationExecutor, NavigationGeneration,
    NavigationId, NavigationRequest, NavigationStage, PixelSize, TopLevelContextId,
    WorkerStopReason,
};

fn context(raw: u64) -> TopLevelContextId {
    TopLevelContextId::new(raw).expect("test context IDs are nonzero")
}

fn generation(raw: u64) -> NavigationGeneration {
    NavigationGeneration::new(raw).expect("test generations are nonzero")
}

fn request() -> NavigationRequest {
    NavigationRequest::new("http://127.0.0.1:8080/").unwrap()
}

fn limits(
    command_capacity: usize,
    event_capacity: usize,
    max_contexts: usize,
    max_frame_bytes: usize,
    max_retained_frame_bytes: usize,
) -> EngineLimits {
    EngineLimits::new(
        command_capacity,
        event_capacity,
        max_contexts,
        max_frame_bytes,
        max_retained_frame_bytes,
    )
    .unwrap()
}

fn output(navigation: NavigationId) -> Result<ExecutorOutput, ExecutionFailure> {
    let marker = u8::try_from(navigation.generation().get() % 251).unwrap();
    let frame = EngineFrame::from_rgba8(
        PixelSize::new(1, 1).unwrap(),
        vec![marker, 0, 0, 255],
        FrameComposition::Complete,
    )
    .unwrap();
    ExecutorOutput::new(200, frame)
}

#[derive(Default)]
struct ThreadProbe {
    factory_thread: Mutex<Option<ThreadId>>,
    execution_threads: Mutex<Vec<ThreadId>>,
    shutdown_thread: Mutex<Option<ThreadId>>,
    shutdown_count: AtomicUsize,
    drop_count: AtomicUsize,
}

impl ThreadProbe {
    fn record_factory(&self) {
        *self.factory_thread.lock().unwrap() = Some(thread::current().id());
    }

    fn record_execution(&self) {
        self.execution_threads
            .lock()
            .unwrap()
            .push(thread::current().id());
    }

    fn record_shutdown(&self) {
        self.shutdown_count.fetch_add(1, Ordering::SeqCst);
        *self.shutdown_thread.lock().unwrap() = Some(thread::current().id());
    }

    fn record_drop(&self) {
        self.drop_count.fetch_add(1, Ordering::SeqCst);
    }
}

struct ImmediateExecutor {
    probe: Arc<ThreadProbe>,
}

impl NavigationExecutor for ImmediateExecutor {
    fn execute(
        &mut self,
        navigation: NavigationId,
        _request: &NavigationRequest,
        _cancellation: &wild_buzzard_engine::CancellationToken,
    ) -> Result<ExecutorOutput, ExecutionFailure> {
        self.probe.record_execution();
        output(navigation)
    }

    fn shutdown(&mut self) -> Result<(), ExecutionFailure> {
        self.probe.record_shutdown();
        Ok(())
    }
}

impl Drop for ImmediateExecutor {
    fn drop(&mut self) {
        self.probe.record_drop();
    }
}

fn spawn_immediate(
    limits: EngineLimits,
) -> (NavigationEngine, EngineEventReceiver, Arc<ThreadProbe>) {
    let probe = Arc::new(ThreadProbe::default());
    let factory_probe = Arc::clone(&probe);
    let (engine, receiver) = NavigationEngine::spawn_with_executor(limits, move || {
        factory_probe.record_factory();
        Ok(ImmediateExecutor {
            probe: factory_probe,
        })
    })
    .unwrap();
    (engine, receiver, probe)
}

struct GatedExecutor {
    entered: mpsc::Sender<NavigationId>,
    releases: mpsc::Receiver<()>,
    shutdown: mpsc::Sender<()>,
    probe: Arc<ThreadProbe>,
}

impl NavigationExecutor for GatedExecutor {
    fn execute(
        &mut self,
        navigation: NavigationId,
        _request: &NavigationRequest,
        _cancellation: &wild_buzzard_engine::CancellationToken,
    ) -> Result<ExecutorOutput, ExecutionFailure> {
        self.probe.record_execution();
        let _ = self.entered.send(navigation);
        let _ = self.releases.recv();
        output(navigation)
    }

    fn shutdown(&mut self) -> Result<(), ExecutionFailure> {
        self.probe.record_shutdown();
        let _ = self.shutdown.send(());
        Ok(())
    }
}

type GatedSpawn = (
    NavigationEngine,
    EngineEventReceiver,
    mpsc::Receiver<NavigationId>,
    mpsc::Sender<()>,
    mpsc::Receiver<()>,
    Arc<ThreadProbe>,
);

fn spawn_gated(limits: EngineLimits) -> GatedSpawn {
    let probe = Arc::new(ThreadProbe::default());
    let factory_probe = Arc::clone(&probe);
    let (entered_sender, entered_receiver) = mpsc::channel();
    let (release_sender, release_receiver) = mpsc::channel();
    let (shutdown_sender, shutdown_receiver) = mpsc::channel();
    let (engine, receiver) = NavigationEngine::spawn_with_executor(limits, move || {
        factory_probe.record_factory();
        Ok(GatedExecutor {
            entered: entered_sender,
            releases: release_receiver,
            shutdown: shutdown_sender,
            probe: factory_probe,
        })
    })
    .unwrap();
    (
        engine,
        receiver,
        entered_receiver,
        release_sender,
        shutdown_receiver,
        probe,
    )
}

struct CancellationExecutor {
    entered: mpsc::Sender<NavigationId>,
    shutdown: mpsc::Sender<()>,
    probe: Arc<ThreadProbe>,
}

impl NavigationExecutor for CancellationExecutor {
    fn execute(
        &mut self,
        navigation: NavigationId,
        _request: &NavigationRequest,
        cancellation: &wild_buzzard_engine::CancellationToken,
    ) -> Result<ExecutorOutput, ExecutionFailure> {
        self.probe.record_execution();
        let _ = self.entered.send(navigation);
        cancellation.wait();
        Err(ExecutionFailure::new(
            ExecutionFailureKind::Cancelled,
            NavigationStage::Fetch,
        ))
    }

    fn shutdown(&mut self) -> Result<(), ExecutionFailure> {
        self.probe.record_shutdown();
        let _ = self.shutdown.send(());
        Ok(())
    }
}

type CancellationSpawn = (
    NavigationEngine,
    EngineEventReceiver,
    mpsc::Receiver<NavigationId>,
    mpsc::Receiver<()>,
    Arc<ThreadProbe>,
);

fn spawn_cancellation(limits: EngineLimits) -> CancellationSpawn {
    let probe = Arc::new(ThreadProbe::default());
    let factory_probe = Arc::clone(&probe);
    let (entered_sender, entered_receiver) = mpsc::channel();
    let (shutdown_sender, shutdown_receiver) = mpsc::channel();
    let (engine, receiver) = NavigationEngine::spawn_with_executor(limits, move || {
        factory_probe.record_factory();
        Ok(CancellationExecutor {
            entered: entered_sender,
            shutdown: shutdown_sender,
            probe: factory_probe,
        })
    })
    .unwrap();
    (engine, receiver, entered_receiver, shutdown_receiver, probe)
}

fn next(receiver: &mut EngineEventReceiver) -> EngineEvent {
    receiver
        .recv()
        .expect("worker must emit the expected event")
}

fn assert_sequences_are_contiguous(events: &[EngineEvent]) {
    for pair in events.windows(2) {
        assert_eq!(
            pair[1].sequence().get(),
            pair[0].sequence().get() + 1,
            "event sequence must be contiguous"
        );
    }
}

fn frame_ready(event: EngineEvent, expected: NavigationId) -> wild_buzzard_engine::FrameLeaseId {
    match event.kind() {
        EngineEventKind::FrameReady {
            navigation,
            lease,
            metadata,
        } => {
            assert_eq!(navigation, expected);
            assert_eq!(metadata.page().byte_len(), 4);
            assert!(metadata.glyph_proof().is_none());
            lease
        }
        other => panic!("expected frame-ready event, got {other:?}"),
    }
}

#[test]
fn superseded_executor_result_never_publishes_a_stale_frame() {
    let (mut engine, mut receiver, entered, releases, shutdown, _) =
        spawn_gated(limits(4, 16, 2, 4, 8));
    let context = context(1);

    let first = engine.navigate(context, request()).unwrap();
    assert_eq!(entered.recv().unwrap(), first);
    let started_first = next(&mut receiver);
    assert_eq!(
        started_first.kind(),
        EngineEventKind::NavigationStarted { navigation: first }
    );

    let second = engine.navigate(context, request()).unwrap();
    releases.send(()).unwrap();
    assert_eq!(entered.recv().unwrap(), second);
    releases.send(()).unwrap();

    let cancelled_first = next(&mut receiver);
    let started_second = next(&mut receiver);
    let committed_second = next(&mut receiver);
    let ready_second = next(&mut receiver);
    assert_eq!(
        cancelled_first.kind(),
        EngineEventKind::NavigationCancelled { navigation: first }
    );
    assert_eq!(
        started_second.kind(),
        EngineEventKind::NavigationStarted { navigation: second }
    );
    assert_eq!(
        committed_second.kind(),
        EngineEventKind::NavigationCommitted {
            navigation: second,
            http_status: 200,
        }
    );
    let lease = frame_ready(ready_second, second);
    assert_sequences_are_contiguous(&[
        started_first,
        cancelled_first,
        started_second,
        committed_second,
        ready_second,
    ]);
    let frame = receiver.take_frame(lease).unwrap();
    assert_eq!(frame.navigation(), second);
    assert_eq!(frame.page_pixels(), &[2, 0, 0, 255]);

    let status = engine.shutdown();
    assert_eq!(status.reason(), WorkerStopReason::Requested);
    assert_eq!(status.executor(), ExecutorShutdownStatus::Clean);
    shutdown.recv().unwrap();
    assert_eq!(
        next(&mut receiver).kind(),
        EngineEventKind::ShutdownComplete { status }
    );
}

#[test]
fn saturated_navigation_admission_is_transactional_and_retryable() {
    let (mut engine, mut receiver, entered, releases, _, _) = spawn_gated(limits(1, 32, 1, 4, 4));
    let context = context(1);
    let first = engine.navigate(context, request()).unwrap();
    assert_eq!(entered.recv().unwrap(), first);

    let second = NavigationId::new(context, generation(2));
    assert_eq!(
        engine
            .try_send(EngineCommand::Navigate {
                navigation: second,
                request: request(),
            })
            .unwrap(),
        CommandReceipt::NavigationQueued(second)
    );
    let third = NavigationId::new(context, generation(3));
    let rejected_request = request();
    let error = engine
        .try_send(EngineCommand::Navigate {
            navigation: third,
            request: rejected_request.clone(),
        })
        .unwrap_err();
    assert_eq!(error.kind(), CommandErrorKind::QueueFull { capacity: 1 });
    assert_eq!(
        error.into_command(),
        EngineCommand::Navigate {
            navigation: third,
            request: rejected_request,
        }
    );
    assert_eq!(engine.latest_generation(context), Some(generation(2)));

    releases.send(()).unwrap();
    assert_eq!(entered.recv().unwrap(), second);
    assert_eq!(
        engine
            .try_send(EngineCommand::Navigate {
                navigation: third,
                request: request(),
            })
            .unwrap(),
        CommandReceipt::NavigationQueued(third)
    );
    assert_eq!(engine.latest_generation(context), Some(generation(3)));
    releases.send(()).unwrap();
    assert_eq!(entered.recv().unwrap(), third);
    releases.send(()).unwrap();

    let events: Vec<_> = (0..7).map(|_| next(&mut receiver)).collect();
    assert_eq!(
        events.iter().map(|event| event.kind()).collect::<Vec<_>>(),
        vec![
            EngineEventKind::NavigationStarted { navigation: first },
            EngineEventKind::NavigationCancelled { navigation: first },
            EngineEventKind::NavigationStarted { navigation: second },
            EngineEventKind::NavigationCancelled { navigation: second },
            EngineEventKind::NavigationStarted { navigation: third },
            EngineEventKind::NavigationCommitted {
                navigation: third,
                http_status: 200,
            },
            events[6].kind(),
        ]
    );
    let lease = frame_ready(events[6], third);
    assert_sequences_are_contiguous(&events);
    assert_eq!(receiver.take_frame(lease).unwrap().navigation(), third);
    assert_eq!(engine.shutdown().reason(), WorkerStopReason::Requested);
}

#[test]
fn context_and_generation_rejections_do_not_mutate_admission_state() {
    let (mut engine, mut receiver, entered, releases, _, _) = spawn_gated(limits(2, 16, 1, 4, 4));
    let first_context = context(1);
    let second_context = context(2);
    let first = engine.navigate(first_context, request()).unwrap();
    assert_eq!(entered.recv().unwrap(), first);

    let wrong_initial = NavigationId::new(second_context, generation(2));
    assert_eq!(
        engine
            .try_send(EngineCommand::Navigate {
                navigation: wrong_initial,
                request: request(),
            })
            .unwrap_err()
            .kind(),
        CommandErrorKind::InitialGenerationRequired
    );
    assert_eq!(engine.latest_generation(second_context), None);

    let first_for_second_context = NavigationId::new(second_context, NavigationGeneration::INITIAL);
    assert_eq!(
        engine
            .try_send(EngineCommand::Navigate {
                navigation: first_for_second_context,
                request: request(),
            })
            .unwrap_err()
            .kind(),
        CommandErrorKind::ContextLimitReached { maximum: 1 }
    );
    assert_eq!(engine.latest_generation(second_context), None);

    let maximum = NavigationId::new(first_context, generation(u64::MAX));
    engine
        .try_send(EngineCommand::Navigate {
            navigation: maximum,
            request: request(),
        })
        .unwrap();
    assert_eq!(
        engine.latest_generation(first_context),
        Some(generation(u64::MAX))
    );
    let error = engine.navigate(first_context, request()).unwrap_err();
    assert_eq!(error.kind(), CommandErrorKind::GenerationExhausted);
    assert_eq!(
        error.into_command(),
        EngineCommand::Navigate {
            navigation: maximum,
            request: request(),
        }
    );
    assert_eq!(
        engine
            .try_send(EngineCommand::Navigate {
                navigation: maximum,
                request: request(),
            })
            .unwrap_err()
            .kind(),
        CommandErrorKind::GenerationExhausted
    );

    releases.send(()).unwrap();
    assert_eq!(entered.recv().unwrap(), maximum);
    releases.send(()).unwrap();
    let events: Vec<_> = (0..5).map(|_| next(&mut receiver)).collect();
    assert_eq!(
        events.iter().map(|event| event.kind()).collect::<Vec<_>>(),
        vec![
            EngineEventKind::NavigationStarted { navigation: first },
            EngineEventKind::NavigationCancelled { navigation: first },
            EngineEventKind::NavigationStarted {
                navigation: maximum,
            },
            EngineEventKind::NavigationCommitted {
                navigation: maximum,
                http_status: 200,
            },
            events[4].kind(),
        ]
    );
    let lease = frame_ready(events[4], maximum);
    assert_eq!(receiver.take_frame(lease).unwrap().navigation(), maximum);
    assert_eq!(engine.shutdown().reason(), WorkerStopReason::Requested);
}

#[test]
fn explicit_cancellation_is_priority_control_and_emits_one_terminal_event() {
    let (mut engine, mut receiver, entered, _, _) = spawn_cancellation(limits(1, 8, 1, 4, 4));
    let navigation = engine.navigate(context(1), request()).unwrap();
    assert_eq!(entered.recv().unwrap(), navigation);
    assert_eq!(
        next(&mut receiver).kind(),
        EngineEventKind::NavigationStarted { navigation }
    );
    assert_eq!(
        engine
            .try_send(EngineCommand::Cancel { navigation })
            .unwrap(),
        CommandReceipt::CancellationRequested(navigation)
    );
    assert_eq!(
        engine
            .try_send(EngineCommand::Cancel { navigation })
            .unwrap_err()
            .kind(),
        CommandErrorKind::NoActiveNavigation
    );
    assert_eq!(
        next(&mut receiver).kind(),
        EngineEventKind::NavigationCancelled { navigation }
    );
    assert_eq!(receiver.try_recv(), Err(EventReceiveError::Empty));
    assert_eq!(engine.shutdown().reason(), WorkerStopReason::Requested);
}

#[test]
fn a_stale_lease_cannot_remove_the_newer_current_frame() {
    let (mut engine, mut receiver, _) = spawn_immediate(limits(4, 16, 1, 4, 4));
    let context = context(1);
    let first = engine.navigate(context, request()).unwrap();
    assert_eq!(
        next(&mut receiver).kind(),
        EngineEventKind::NavigationStarted { navigation: first }
    );
    assert!(matches!(
        next(&mut receiver).kind(),
        EngineEventKind::NavigationCommitted { navigation, .. } if navigation == first
    ));
    let first_lease = frame_ready(next(&mut receiver), first);

    let second = engine.navigate(context, request()).unwrap();
    assert_eq!(
        next(&mut receiver).kind(),
        EngineEventKind::NavigationStarted { navigation: second }
    );
    assert!(matches!(
        next(&mut receiver).kind(),
        EngineEventKind::NavigationCommitted { navigation, .. } if navigation == second
    ));
    let second_lease = frame_ready(next(&mut receiver), second);

    assert_eq!(
        receiver.take_frame(first_lease).unwrap_err(),
        FrameLeaseError::Stale
    );
    let second_frame = receiver.take_frame(second_lease).unwrap();
    assert_eq!(second_frame.navigation(), second);
    assert_eq!(second_frame.page_pixels(), &[2, 0, 0, 255]);
    assert_eq!(
        receiver.take_frame(second_lease).unwrap_err(),
        FrameLeaseError::Stale
    );
    assert_eq!(engine.shutdown().reason(), WorkerStopReason::Requested);
}

#[test]
fn aggregate_frame_limit_rejects_new_context_without_losing_prior_frame() {
    let (mut engine, mut receiver, _) = spawn_immediate(limits(4, 16, 2, 4, 4));
    let first = engine.navigate(context(1), request()).unwrap();
    assert!(matches!(
        next(&mut receiver).kind(),
        EngineEventKind::NavigationStarted { navigation } if navigation == first
    ));
    assert!(matches!(
        next(&mut receiver).kind(),
        EngineEventKind::NavigationCommitted { navigation, .. } if navigation == first
    ));
    let first_lease = frame_ready(next(&mut receiver), first);

    let second = engine.navigate(context(2), request()).unwrap();
    assert_eq!(
        next(&mut receiver).kind(),
        EngineEventKind::NavigationStarted { navigation: second }
    );
    assert_eq!(
        next(&mut receiver).kind(),
        EngineEventKind::NavigationFailed {
            navigation: second,
            failure: ExecutionFailure::new(
                ExecutionFailureKind::ResourceLimit,
                NavigationStage::Render,
            ),
        }
    );
    assert_eq!(
        receiver.take_frame(first_lease).unwrap().navigation(),
        first
    );
    assert_eq!(engine.shutdown().reason(), WorkerStopReason::Requested);
}

#[test]
fn event_backpressure_stops_worker_and_preserves_previous_frame() {
    let (mut engine, mut receiver, entered, releases, worker_shutdown, _) =
        spawn_gated(limits(2, 3, 2, 4, 4));
    let first_context = context(1);
    let first = engine.navigate(first_context, request()).unwrap();
    assert_eq!(entered.recv().unwrap(), first);
    assert_eq!(
        next(&mut receiver).kind(),
        EngineEventKind::NavigationStarted { navigation: first }
    );
    releases.send(()).unwrap();
    assert!(matches!(
        next(&mut receiver).kind(),
        EngineEventKind::NavigationCommitted { navigation, .. } if navigation == first
    ));
    let first_lease = frame_ready(next(&mut receiver), first);

    let second = engine.navigate(first_context, request()).unwrap();
    assert_eq!(entered.recv().unwrap(), second);
    let third = engine.navigate(context(2), request()).unwrap();
    releases.send(()).unwrap();
    worker_shutdown.recv().unwrap();
    let status = engine.shutdown();
    assert_eq!(status.reason(), WorkerStopReason::EventQueueSaturated);
    assert_eq!(status.executor(), ExecutorShutdownStatus::Clean);
    assert_eq!(
        next(&mut receiver).kind(),
        EngineEventKind::NavigationStarted { navigation: second }
    );
    assert_eq!(
        next(&mut receiver).kind(),
        EngineEventKind::NavigationCommitted {
            navigation: second,
            http_status: 200,
        }
    );
    let second_lease = frame_ready(next(&mut receiver), second);
    assert_eq!(
        receiver.take_frame(first_lease).unwrap_err(),
        FrameLeaseError::Stale
    );
    assert_eq!(
        receiver.take_frame(second_lease).unwrap().navigation(),
        second
    );
    assert_eq!(
        engine.latest_generation(context(2)),
        Some(third.generation())
    );
    assert_eq!(
        next(&mut receiver).kind(),
        EngineEventKind::ShutdownComplete { status }
    );
    assert_eq!(receiver.recv(), Err(EventReceiveError::Closed(status)));
}

#[test]
fn dropping_receiver_cancels_work_and_shutdown_joins_deterministically() {
    let (mut engine, receiver, entered, worker_shutdown, probe) =
        spawn_cancellation(limits(2, 8, 1, 4, 4));
    let navigation = engine.navigate(context(1), request()).unwrap();
    assert_eq!(entered.recv().unwrap(), navigation);
    drop(receiver);
    worker_shutdown.recv().unwrap();
    let status = engine.shutdown();
    assert_eq!(status.reason(), WorkerStopReason::EventReceiverDropped);
    assert_eq!(status.executor(), ExecutorShutdownStatus::Clean);
    assert_eq!(probe.shutdown_count.load(Ordering::SeqCst), 1);
}

#[test]
fn factory_execute_and_exactly_once_shutdown_stay_on_worker_thread() {
    let main_thread = thread::current().id();
    let (mut engine, mut receiver, probe) = spawn_immediate(limits(2, 8, 1, 4, 4));
    let navigation = engine.navigate(context(1), request()).unwrap();
    assert!(matches!(
        next(&mut receiver).kind(),
        EngineEventKind::NavigationStarted { navigation: current } if current == navigation
    ));
    assert!(matches!(
        next(&mut receiver).kind(),
        EngineEventKind::NavigationCommitted { navigation: current, .. } if current == navigation
    ));
    let lease = frame_ready(next(&mut receiver), navigation);
    let _ = receiver.take_frame(lease).unwrap();

    let first_status = engine.shutdown();
    let second_status = engine.shutdown();
    assert_eq!(first_status, second_status);
    assert_eq!(probe.shutdown_count.load(Ordering::SeqCst), 1);
    assert_eq!(probe.drop_count.load(Ordering::SeqCst), 1);
    let factory_thread = probe.factory_thread.lock().unwrap().unwrap();
    let shutdown_thread = probe.shutdown_thread.lock().unwrap().unwrap();
    let execution_threads = probe.execution_threads.lock().unwrap();
    assert_ne!(factory_thread, main_thread);
    assert_eq!(shutdown_thread, factory_thread);
    assert_eq!(execution_threads.as_slice(), &[factory_thread]);
}

struct PanickingExecutor {
    probe: Arc<ThreadProbe>,
}

impl NavigationExecutor for PanickingExecutor {
    fn execute(
        &mut self,
        _navigation: NavigationId,
        _request: &NavigationRequest,
        _cancellation: &wild_buzzard_engine::CancellationToken,
    ) -> Result<ExecutorOutput, ExecutionFailure> {
        self.probe.record_execution();
        panic!("contained fake executor panic")
    }

    fn shutdown(&mut self) -> Result<(), ExecutionFailure> {
        self.probe.record_shutdown();
        Ok(())
    }
}

impl Drop for PanickingExecutor {
    fn drop(&mut self) {
        self.probe.record_drop();
    }
}

#[test]
fn executor_panic_is_contained_and_cleanup_still_runs_once() {
    let probe = Arc::new(ThreadProbe::default());
    let factory_probe = Arc::clone(&probe);
    let (mut engine, mut receiver) =
        NavigationEngine::spawn_with_executor(limits(2, 8, 1, 4, 4), move || {
            factory_probe.record_factory();
            Ok(PanickingExecutor {
                probe: factory_probe,
            })
        })
        .unwrap();
    let navigation = engine.navigate(context(1), request()).unwrap();
    assert_eq!(
        next(&mut receiver).kind(),
        EngineEventKind::NavigationStarted { navigation }
    );
    let status = engine.shutdown();
    assert_eq!(status.reason(), WorkerStopReason::ExecutorPanicked);
    assert_eq!(status.executor(), ExecutorShutdownStatus::Clean);
    assert_eq!(probe.shutdown_count.load(Ordering::SeqCst), 1);
    assert_eq!(probe.drop_count.load(Ordering::SeqCst), 1);
    assert_eq!(
        next(&mut receiver).kind(),
        EngineEventKind::ShutdownComplete { status }
    );
}

#[derive(Clone, Copy)]
enum ShutdownBehavior {
    Clean,
    Fail(ExecutionFailure),
    Panic,
}

struct LifecycleExecutor {
    probe: Arc<ThreadProbe>,
    shutdown_behavior: ShutdownBehavior,
    panic_on_drop: bool,
}

impl NavigationExecutor for LifecycleExecutor {
    fn execute(
        &mut self,
        navigation: NavigationId,
        _request: &NavigationRequest,
        _cancellation: &wild_buzzard_engine::CancellationToken,
    ) -> Result<ExecutorOutput, ExecutionFailure> {
        self.probe.record_execution();
        output(navigation)
    }

    fn shutdown(&mut self) -> Result<(), ExecutionFailure> {
        self.probe.record_shutdown();
        match self.shutdown_behavior {
            ShutdownBehavior::Clean => Ok(()),
            ShutdownBehavior::Fail(failure) => Err(failure),
            ShutdownBehavior::Panic => panic!("contained fake shutdown panic"),
        }
    }
}

impl Drop for LifecycleExecutor {
    fn drop(&mut self) {
        self.probe.record_drop();
        assert!(!self.panic_on_drop, "contained fake destructor panic");
    }
}

fn spawn_lifecycle(
    shutdown_behavior: ShutdownBehavior,
    panic_on_drop: bool,
) -> (NavigationEngine, EngineEventReceiver, Arc<ThreadProbe>) {
    let probe = Arc::new(ThreadProbe::default());
    let factory_probe = Arc::clone(&probe);
    let (engine, receiver) =
        NavigationEngine::spawn_with_executor(limits(1, 3, 1, 4, 4), move || {
            factory_probe.record_factory();
            Ok(LifecycleExecutor {
                probe: factory_probe,
                shutdown_behavior,
                panic_on_drop,
            })
        })
        .unwrap();
    (engine, receiver, probe)
}

#[test]
fn factory_error_and_panic_are_reported_without_executor_cleanup() {
    let failure = ExecutionFailure::new(ExecutionFailureKind::Internal, NavigationStage::Render);
    let error_probe = Arc::new(ThreadProbe::default());
    let factory_probe = Arc::clone(&error_probe);
    let error_result = NavigationEngine::spawn_with_executor::<LifecycleExecutor, _>(
        limits(1, 3, 1, 4, 4),
        move || {
            factory_probe.record_factory();
            Err(failure)
        },
    );
    assert!(matches!(
        error_result,
        Err(EngineStartError::Executor(actual)) if actual == failure
    ));
    assert_eq!(error_probe.shutdown_count.load(Ordering::SeqCst), 0);
    assert_eq!(error_probe.drop_count.load(Ordering::SeqCst), 0);

    let panic_probe = Arc::new(ThreadProbe::default());
    let factory_probe = Arc::clone(&panic_probe);
    let panic_result = NavigationEngine::spawn_with_executor::<LifecycleExecutor, _>(
        limits(1, 3, 1, 4, 4),
        move || {
            factory_probe.record_factory();
            panic!("contained fake factory panic")
        },
    );
    assert!(matches!(
        panic_result,
        Err(EngineStartError::ExecutorPanicked)
    ));
    assert_eq!(panic_probe.shutdown_count.load(Ordering::SeqCst), 0);
    assert_eq!(panic_probe.drop_count.load(Ordering::SeqCst), 0);
}

#[test]
fn shutdown_error_is_reported_and_executor_is_dropped_once() {
    let failure = ExecutionFailure::new(ExecutionFailureKind::Internal, NavigationStage::Shutdown);
    let (mut engine, mut receiver, probe) = spawn_lifecycle(ShutdownBehavior::Fail(failure), false);
    let status = engine.shutdown();
    assert_eq!(status.reason(), WorkerStopReason::Requested);
    assert_eq!(status.executor(), ExecutorShutdownStatus::Failed(failure));
    assert_eq!(probe.shutdown_count.load(Ordering::SeqCst), 1);
    assert_eq!(probe.drop_count.load(Ordering::SeqCst), 1);
    assert_eq!(
        next(&mut receiver).kind(),
        EngineEventKind::ShutdownComplete { status }
    );
}

#[test]
fn shutdown_panic_is_contained_and_executor_is_dropped_once() {
    let (mut engine, mut receiver, probe) = spawn_lifecycle(ShutdownBehavior::Panic, false);
    let status = engine.shutdown();
    assert_eq!(status.reason(), WorkerStopReason::Requested);
    assert_eq!(status.executor(), ExecutorShutdownStatus::Panicked);
    assert_eq!(probe.shutdown_count.load(Ordering::SeqCst), 1);
    assert_eq!(probe.drop_count.load(Ordering::SeqCst), 1);
    assert_eq!(
        next(&mut receiver).kind(),
        EngineEventKind::ShutdownComplete { status }
    );
}

#[test]
fn destructor_panic_cannot_publish_a_clean_shutdown_status() {
    let (mut engine, mut receiver, probe) = spawn_lifecycle(ShutdownBehavior::Clean, true);
    let status = engine.shutdown();
    assert_eq!(status.reason(), WorkerStopReason::ExecutorPanicked);
    assert_eq!(status.executor(), ExecutorShutdownStatus::Panicked);
    assert_eq!(probe.shutdown_count.load(Ordering::SeqCst), 1);
    assert_eq!(probe.drop_count.load(Ordering::SeqCst), 1);
    assert_eq!(
        next(&mut receiver).kind(),
        EngineEventKind::ShutdownComplete { status }
    );
}

#[test]
fn request_and_limit_construction_enforce_hard_bounds() {
    assert!(NavigationRequest::new("").is_err());
    let oversized = "x".repeat(wild_buzzard_engine::MAX_NAVIGATION_URL_BYTES + 1);
    assert!(NavigationRequest::new(&oversized).is_err());
    assert!(EngineLimits::new(0, 3, 1, 4, 4).is_err());
    assert!(EngineLimits::new(1, 1, 1, 4, 4).is_err());
    assert!(EngineLimits::new(1, 2, 1, 4, 4).is_err());
    assert!(EngineLimits::new(1, 3, 1, 8, 4).is_err());
    assert_eq!(TopLevelContextId::new(0), None);
    assert_eq!(NavigationGeneration::new(0), None);
}
