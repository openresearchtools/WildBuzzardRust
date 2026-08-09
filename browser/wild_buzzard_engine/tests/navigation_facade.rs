use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, ThreadId};
use std::time::Duration;

use wild_buzzard_dom::bindings::{
    CreatedNodeToken, ScriptMutationBatch, ScriptMutationCommand, ScriptMutationLimits, ScriptNode,
};
use wild_buzzard_dom::{Document, DocumentVersion, NodeId};

use wild_buzzard_engine::{
    CommandErrorKind, CommandReceipt, DocumentLoadProof, DocumentMutationCommit,
    DocumentOperationFailure, DocumentOperationId, EngineCommand, EngineEvent, EngineEventKind,
    EngineEventReceiver, EngineFrame, EngineLimits, EngineStartError, EventReceiveError,
    ExecutionFailure, ExecutionFailureKind, ExecutorDocumentMutation, ExecutorDocumentRerender,
    ExecutorOutput, ExecutorShutdownStatus, FontSourcePolicy, FrameLeaseError,
    MutationResultLeaseError, NavigationEngine, NavigationExecutor, NavigationGeneration,
    NavigationId, NavigationRequest, NavigationStage, PixelSize, StaticPageConfig,
    TopLevelContextId, WorkerStopReason,
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
    let frame =
        EngineFrame::from_rgba8(PixelSize::new(1, 1).unwrap(), vec![marker, 0, 0, 255]).unwrap();
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
            assert_eq!(metadata.rgba8().byte_len(), 4);
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
    assert_eq!(frame.pixels(), &[2, 0, 0, 255]);

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
        CommandReceipt::NavigationCancellationRequested(navigation)
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
    assert_eq!(second_frame.pixels(), &[2, 0, 0, 255]);
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

fn document_frame(version: DocumentVersion, marker: u8) -> EngineFrame {
    EngineFrame::from_rgba8_for_document(
        PixelSize::new(1, 1).unwrap(),
        vec![marker, 0, 0, 255],
        version,
    )
    .unwrap()
}

struct GatedDocumentExecutor {
    document: Option<Document>,
    frame_version: Option<DocumentVersion>,
    mutation_entered: mpsc::Sender<()>,
    mutation_release: mpsc::Receiver<()>,
    invalidated: Arc<AtomicBool>,
    gate_before_commit: bool,
    gate_rerender: bool,
    fail_newer_navigation: bool,
}

impl NavigationExecutor for GatedDocumentExecutor {
    fn execute(
        &mut self,
        navigation: NavigationId,
        _request: &NavigationRequest,
        _cancellation: &wild_buzzard_engine::CancellationToken,
    ) -> Result<ExecutorOutput, ExecutionFailure> {
        if self.fail_newer_navigation && navigation.generation() != NavigationGeneration::INITIAL {
            return Err(ExecutionFailure::new(
                ExecutionFailureKind::Rejected,
                NavigationStage::Fetch,
            ));
        }
        let document = Document::new();
        let version = document.version();
        let proof = DocumentLoadProof::from_snapshot(&document.snapshot().unwrap());
        self.document = Some(document);
        self.frame_version = Some(version);
        let marker = u8::try_from(navigation.generation().get() % 251).unwrap();
        ExecutorOutput::new_document(200, document_frame(version, marker), proof)
    }

    fn mutate_document(
        &mut self,
        _navigation: NavigationId,
        batch: ScriptMutationBatch,
        cancellation: &wild_buzzard_engine::CancellationToken,
    ) -> ExecutorDocumentMutation {
        let Some(document) = self.document.as_mut() else {
            return ExecutorDocumentMutation::Rejected {
                live_version: None,
                frame_version: None,
                failure: DocumentOperationFailure::NoLiveDocument,
            };
        };
        let previous_live_version = document.version();
        let previous_frame_version = self.frame_version.unwrap();
        if self.gate_before_commit {
            self.mutation_entered.send(()).unwrap();
            self.mutation_release.recv().unwrap();
            if cancellation.is_cancelled() {
                return ExecutorDocumentMutation::Rejected {
                    live_version: Some(previous_live_version),
                    frame_version: Some(previous_frame_version),
                    failure: DocumentOperationFailure::Cancelled,
                };
            }
        }
        let commit = document
            .apply_script_mutations(batch, ScriptMutationLimits::DEFAULT)
            .unwrap();
        let live_version = commit.version();
        let commit = DocumentMutationCommit::from_script_commit(commit);
        if !self.gate_before_commit {
            self.mutation_entered.send(()).unwrap();
            self.mutation_release.recv().unwrap();
            if cancellation.is_cancelled() {
                return ExecutorDocumentMutation::CommittedWithoutFrame {
                    previous_live_version,
                    frame_version: previous_frame_version,
                    commit,
                    failure: DocumentOperationFailure::Cancelled,
                };
            }
        }
        self.frame_version = Some(live_version);
        ExecutorDocumentMutation::Rendered {
            previous_live_version,
            previous_frame_version,
            commit,
            frame: document_frame(live_version, 77),
        }
    }

    fn rerender_document(
        &mut self,
        _navigation: NavigationId,
        expected_live_version: DocumentVersion,
        cancellation: &wild_buzzard_engine::CancellationToken,
    ) -> ExecutorDocumentRerender {
        let Some(document) = self.document.as_ref() else {
            return ExecutorDocumentRerender::Rejected {
                live_version: None,
                frame_version: None,
                failure: DocumentOperationFailure::NoLiveDocument,
            };
        };
        let live_version = document.version();
        let previous_frame_version = self.frame_version.unwrap();
        if live_version != expected_live_version {
            return ExecutorDocumentRerender::Rejected {
                live_version: Some(live_version),
                frame_version: Some(previous_frame_version),
                failure: DocumentOperationFailure::VersionMismatch,
            };
        }
        if self.gate_rerender {
            self.mutation_entered.send(()).unwrap();
            self.mutation_release.recv().unwrap();
            if cancellation.is_cancelled() {
                return ExecutorDocumentRerender::Rejected {
                    live_version: Some(live_version),
                    frame_version: Some(previous_frame_version),
                    failure: DocumentOperationFailure::Cancelled,
                };
            }
        }
        self.frame_version = Some(live_version);
        ExecutorDocumentRerender::Rendered {
            live_version,
            previous_frame_version,
            frame: document_frame(live_version, 88),
        }
    }

    fn invalidate_document(&mut self, _context: TopLevelContextId) {
        self.document = None;
        self.frame_version = None;
        self.invalidated.store(true, Ordering::SeqCst);
    }

    fn close_context(&mut self, _context: TopLevelContextId) {
        self.invalidate_document(context(1));
    }

    fn shutdown(&mut self) -> Result<(), ExecutionFailure> {
        Ok(())
    }
}

type GatedDocumentSpawn = (
    NavigationEngine,
    EngineEventReceiver,
    mpsc::Receiver<()>,
    mpsc::Sender<()>,
    Arc<AtomicBool>,
);

fn spawn_gated_document(limits: EngineLimits) -> GatedDocumentSpawn {
    spawn_configured_gated_document(limits, false, false, false)
}

fn spawn_configured_gated_document(
    limits: EngineLimits,
    gate_before_commit: bool,
    gate_rerender: bool,
    fail_newer_navigation: bool,
) -> GatedDocumentSpawn {
    let (entered_sender, entered_receiver) = mpsc::channel();
    let (release_sender, release_receiver) = mpsc::channel();
    let invalidated = Arc::new(AtomicBool::new(false));
    let executor_invalidated = Arc::clone(&invalidated);
    let (engine, receiver) = NavigationEngine::spawn_with_executor(limits, move || {
        Ok(GatedDocumentExecutor {
            document: None,
            frame_version: None,
            mutation_entered: entered_sender,
            mutation_release: release_receiver,
            invalidated: executor_invalidated,
            gate_before_commit,
            gate_rerender,
            fail_newer_navigation,
        })
    })
    .unwrap();
    (
        engine,
        receiver,
        entered_receiver,
        release_sender,
        invalidated,
    )
}

fn loaded_version(receiver: &mut EngineEventReceiver, navigation: NavigationId) -> DocumentVersion {
    assert_eq!(
        next(receiver).kind(),
        EngineEventKind::NavigationStarted { navigation }
    );
    assert!(matches!(
        next(receiver).kind(),
        EngineEventKind::NavigationCommitted { navigation: current, .. } if current == navigation
    ));
    let ready = next(receiver);
    let EngineEventKind::FrameReady {
        navigation: current,
        lease,
        ..
    } = ready.kind()
    else {
        panic!("expected initial frame");
    };
    assert_eq!(current, navigation);
    receiver
        .take_frame(lease)
        .unwrap()
        .document_version()
        .expect("typed document executor frames name their revision")
}

fn one_created_text(version: DocumentVersion) -> ScriptMutationBatch {
    ScriptMutationBatch::new(
        version,
        vec![ScriptMutationCommand::CreateText {
            token: CreatedNodeToken::from_index(0),
            data: "worker mutation".into(),
        }],
    )
}

fn mutation_operation(receipt: CommandReceipt) -> DocumentOperationId {
    let CommandReceipt::DocumentMutationQueued { operation, .. } = receipt else {
        panic!("mutation admission must return its operation identity");
    };
    operation
}

fn rerender_operation(receipt: CommandReceipt) -> DocumentOperationId {
    let CommandReceipt::DocumentRerenderQueued { operation, .. } = receipt else {
        panic!("rerender admission must return its operation identity");
    };
    operation
}

struct ZeroResultExecutor {
    document: Option<Document>,
    text: Option<NodeId>,
    live_version: Option<DocumentVersion>,
    frame_version: Option<DocumentVersion>,
}

impl NavigationExecutor for ZeroResultExecutor {
    fn execute(
        &mut self,
        navigation: NavigationId,
        _request: &NavigationRequest,
        _cancellation: &wild_buzzard_engine::CancellationToken,
    ) -> Result<ExecutorOutput, ExecutionFailure> {
        let mut document = Document::new();
        let html = document.create_html_element("html").unwrap();
        let text = document.create_text("zero-result").unwrap();
        document.append_child(html, text).unwrap();
        document
            .append_child(document.document_node(), html)
            .unwrap();
        let version = document.version();
        let proof = DocumentLoadProof::from_snapshot(&document.snapshot().unwrap());
        self.document = Some(document);
        self.text = Some(text);
        self.live_version = Some(version);
        self.frame_version = Some(version);
        let marker = u8::try_from(navigation.generation().get() % 251).unwrap();
        ExecutorOutput::new_document(200, document_frame(version, marker), proof)
    }

    fn mutate_document(
        &mut self,
        _navigation: NavigationId,
        _batch: ScriptMutationBatch,
        _cancellation: &wild_buzzard_engine::CancellationToken,
    ) -> ExecutorDocumentMutation {
        let previous_live_version = self.live_version.unwrap();
        let previous_frame_version = self.frame_version.unwrap();
        let batch = ScriptMutationBatch::new(
            previous_live_version,
            vec![ScriptMutationCommand::SetCharacterData {
                node: ScriptNode::Existing(self.text.unwrap()),
                data: "updated without creation".into(),
            }],
        );
        let commit = self
            .document
            .as_mut()
            .unwrap()
            .apply_script_mutations(batch, ScriptMutationLimits::DEFAULT)
            .unwrap();
        let live_version = commit.version();
        let commit = DocumentMutationCommit::from_script_commit(commit);
        self.live_version = Some(live_version);
        self.frame_version = Some(live_version);
        ExecutorDocumentMutation::Rendered {
            previous_live_version,
            previous_frame_version,
            commit,
            frame: document_frame(live_version, 99),
        }
    }

    fn shutdown(&mut self) -> Result<(), ExecutionFailure> {
        Ok(())
    }
}

fn zero_create_batch(version: DocumentVersion) -> ScriptMutationBatch {
    ScriptMutationBatch::new(version, Vec::new())
}

#[test]
fn empty_created_maps_consume_and_release_a_bounded_result_lease_unit() {
    let result_limited = limits(4, 8, 1, 4, 4)
        .with_max_retained_mutation_result_nodes(1)
        .unwrap();
    let (mut engine, mut receiver) = NavigationEngine::spawn_with_executor(result_limited, || {
        Ok(ZeroResultExecutor {
            document: None,
            text: None,
            live_version: None,
            frame_version: None,
        })
    })
    .unwrap();
    let navigation = engine.navigate(context(1), request()).unwrap();
    let initial = loaded_version(&mut receiver, navigation);
    engine
        .mutate_document(navigation, zero_create_batch(initial))
        .unwrap();
    let first = next(&mut receiver);
    let EngineEventKind::DocumentMutationRendered {
        live_version,
        result,
        frame,
        created_nodes: 0,
        ..
    } = first.kind()
    else {
        panic!("zero-create mutation must still publish a bounded result lease");
    };
    receiver.take_frame(frame).unwrap();

    assert_eq!(
        engine
            .mutate_document(navigation, zero_create_batch(live_version))
            .unwrap_err()
            .kind(),
        CommandErrorKind::MutationResultNodeLimit { maximum: 1 }
    );
    assert!(
        receiver
            .take_mutation_result(result)
            .unwrap()
            .created_nodes()
            .is_empty()
    );
    engine
        .mutate_document(navigation, zero_create_batch(live_version))
        .unwrap();
    assert!(matches!(
        next(&mut receiver).kind(),
        EngineEventKind::DocumentMutationRendered {
            created_nodes: 0,
            ..
        }
    ));
    assert_eq!(engine.shutdown().reason(), WorkerStopReason::Requested);
}

struct InvalidatingRerenderExecutor {
    version: Option<DocumentVersion>,
}

impl NavigationExecutor for InvalidatingRerenderExecutor {
    fn execute(
        &mut self,
        _navigation: NavigationId,
        _request: &NavigationRequest,
        _cancellation: &wild_buzzard_engine::CancellationToken,
    ) -> Result<ExecutorOutput, ExecutionFailure> {
        let document = Document::new();
        let version = document.version();
        let proof = DocumentLoadProof::from_snapshot(&document.snapshot().unwrap());
        self.version = Some(version);
        ExecutorOutput::new_document(200, document_frame(version, 1), proof)
    }

    fn rerender_document(
        &mut self,
        _navigation: NavigationId,
        _expected_live_version: DocumentVersion,
        _cancellation: &wild_buzzard_engine::CancellationToken,
    ) -> ExecutorDocumentRerender {
        ExecutorDocumentRerender::Invalidated
    }

    fn shutdown(&mut self) -> Result<(), ExecutionFailure> {
        Ok(())
    }
}

struct ProvenMutationExecutor {
    document: Option<Document>,
    frame_version: Option<DocumentVersion>,
    substitute_valid_batch: bool,
    preexisting: Arc<Mutex<Option<NodeId>>>,
    invalidated: Arc<AtomicBool>,
}

impl NavigationExecutor for ProvenMutationExecutor {
    fn execute(
        &mut self,
        _navigation: NavigationId,
        _request: &NavigationRequest,
        _cancellation: &wild_buzzard_engine::CancellationToken,
    ) -> Result<ExecutorOutput, ExecutionFailure> {
        let mut document = Document::new();
        let preexisting = document.create_text("pre-existing").unwrap();
        let version = document.version();
        let proof = DocumentLoadProof::from_snapshot(&document.snapshot().unwrap());
        *self.preexisting.lock().unwrap() = Some(preexisting);
        self.document = Some(document);
        self.frame_version = Some(version);
        ExecutorOutput::new_document(200, document_frame(version, 1), proof)
    }

    fn mutate_document(
        &mut self,
        _navigation: NavigationId,
        batch: ScriptMutationBatch,
        _cancellation: &wild_buzzard_engine::CancellationToken,
    ) -> ExecutorDocumentMutation {
        let document = self.document.as_mut().unwrap();
        let previous_live_version = document.version();
        let previous_frame_version = self.frame_version.unwrap();
        let batch = if self.substitute_valid_batch {
            one_created_text(previous_live_version)
        } else {
            batch
        };
        let commit = document
            .apply_script_mutations(batch, ScriptMutationLimits::DEFAULT)
            .unwrap();
        let live_version = commit.version();
        let commit = DocumentMutationCommit::from_script_commit(commit);
        self.frame_version = Some(live_version);
        ExecutorDocumentMutation::Rendered {
            previous_live_version,
            previous_frame_version,
            commit,
            frame: document_frame(live_version, 2),
        }
    }

    fn invalidate_document(&mut self, _context: TopLevelContextId) {
        self.invalidated.store(true, Ordering::SeqCst);
    }

    fn shutdown(&mut self) -> Result<(), ExecutionFailure> {
        Ok(())
    }
}

fn spawn_proven_mutation(
    substitute_valid_batch: bool,
) -> (
    NavigationEngine,
    EngineEventReceiver,
    Arc<AtomicBool>,
    Arc<Mutex<Option<NodeId>>>,
) {
    let invalidated = Arc::new(AtomicBool::new(false));
    let executor_invalidated = Arc::clone(&invalidated);
    let preexisting = Arc::new(Mutex::new(None));
    let executor_preexisting = Arc::clone(&preexisting);
    let (engine, receiver) =
        NavigationEngine::spawn_with_executor(limits(2, 6, 1, 4, 4), move || {
            Ok(ProvenMutationExecutor {
                document: None,
                frame_version: None,
                substitute_valid_batch,
                preexisting: executor_preexisting,
                invalidated: executor_invalidated,
            })
        })
        .unwrap();
    (engine, receiver, invalidated, preexisting)
}

fn assert_contract_violation_shutdown(
    engine: &mut NavigationEngine,
    receiver: &mut EngineEventReceiver,
    invalidated: &AtomicBool,
) {
    let terminal = next(receiver);
    let EngineEventKind::ShutdownComplete { status } = terminal.kind() else {
        panic!("invalid successful mutation contract must publish only terminal shutdown");
    };
    assert_eq!(status.reason(), WorkerStopReason::ExecutorContractViolation);
    assert!(invalidated.load(Ordering::SeqCst));
    assert_eq!(engine.shutdown(), status);
}

#[test]
fn successful_mutation_with_out_of_order_create_token_invalidates_and_stops() {
    let (mut engine, mut receiver, invalidated, _) = spawn_proven_mutation(true);
    let navigation = engine.navigate(context(1), request()).unwrap();
    let version = loaded_version(&mut receiver, navigation);
    let batch = ScriptMutationBatch::new(
        version,
        vec![ScriptMutationCommand::CreateText {
            token: CreatedNodeToken::from_index(1),
            data: "out of order".into(),
        }],
    );
    engine.mutate_document(navigation, batch).unwrap();
    assert_contract_violation_shutdown(&mut engine, &mut receiver, &invalidated);
}

#[test]
fn one_create_proof_cannot_substitute_a_preexisting_node() {
    let (mut engine, mut receiver, invalidated, preexisting) = spawn_proven_mutation(false);
    let navigation = engine.navigate(context(1), request()).unwrap();
    let version = loaded_version(&mut receiver, navigation);
    let preexisting = preexisting.lock().unwrap().unwrap();
    engine
        .mutate_document(navigation, one_created_text(version))
        .unwrap();
    let EngineEventKind::DocumentMutationRendered { result, frame, .. } =
        next(&mut receiver).kind()
    else {
        panic!("the actual DOM commit proof must publish");
    };
    let mapping = receiver.take_mutation_result(result).unwrap();
    assert_eq!(mapping.created_nodes().len(), 1);
    assert_ne!(mapping.created_nodes()[0], preexisting);
    receiver.take_frame(frame).unwrap();
    assert!(!invalidated.load(Ordering::SeqCst));
    assert_eq!(engine.shutdown().reason(), WorkerStopReason::Requested);
}

#[test]
fn hidden_frame_change_without_valid_output_invalidates_context_and_stops_worker() {
    let (mut engine, mut receiver) =
        NavigationEngine::spawn_with_executor(limits(2, 6, 1, 4, 4), || {
            Ok(InvalidatingRerenderExecutor { version: None })
        })
        .unwrap();
    let navigation = engine.navigate(context(1), request()).unwrap();
    let version = loaded_version(&mut receiver, navigation);
    engine.rerender_document(navigation, version).unwrap();
    let terminal = next(&mut receiver);
    let EngineEventKind::ShutdownComplete { status } = terminal.kind() else {
        panic!("invalid hidden frame state must not publish a rerender event");
    };
    assert_eq!(status.reason(), WorkerStopReason::ExecutorContractViolation);
    assert_eq!(engine.shutdown(), status);
}

#[test]
fn explicit_cancel_after_dom_commit_preserves_l_and_result_map_then_rerenders_f() {
    let (mut engine, mut receiver, entered, release, invalidated) =
        spawn_gated_document(limits(4, 8, 1, 4, 4));
    let navigation = engine.navigate(context(1), request()).unwrap();
    let initial = loaded_version(&mut receiver, navigation);

    let receipt = engine
        .mutate_document(navigation, one_created_text(initial))
        .unwrap();
    let CommandReceipt::DocumentMutationQueued { operation, .. } = receipt else {
        panic!("mutation admission must return its operation identity");
    };
    entered.recv().unwrap();
    assert_eq!(
        engine
            .cancel_document_operation(navigation, operation)
            .unwrap(),
        CommandReceipt::DocumentOperationCancellationRequested {
            navigation,
            operation,
        }
    );
    release.send(()).unwrap();

    let event = next(&mut receiver);
    let EngineEventKind::DocumentMutationCommittedWithoutFrame {
        operation: published_operation,
        live_version,
        frame_version,
        result,
        created_nodes,
        failure,
        ..
    } = event.kind()
    else {
        panic!("post-commit cancellation must remain observable as committed");
    };
    assert_eq!(published_operation, operation);
    assert_eq!(live_version.revision(), initial.revision() + 1);
    assert_eq!(frame_version, initial);
    assert_eq!(created_nodes, 1);
    assert_eq!(failure, DocumentOperationFailure::Cancelled);
    let mapping = receiver.take_mutation_result(result).unwrap();
    assert_eq!(mapping.operation(), operation);
    assert_eq!(mapping.live_version(), live_version);
    assert_eq!(mapping.created_nodes().len(), 1);
    assert!(!invalidated.load(Ordering::SeqCst));

    let rerender_operation =
        rerender_operation(engine.rerender_document(navigation, live_version).unwrap());
    assert!(rerender_operation.get() > operation.get());
    let rerender = next(&mut receiver);
    let EngineEventKind::DocumentRerendered {
        operation: published_rerender_operation,
        live_version: rendered,
        previous_frame_version,
        frame,
        ..
    } = rerender.kind()
    else {
        panic!("the committed live revision must be repairable");
    };
    assert_eq!(published_rerender_operation, rerender_operation);
    assert_eq!(rendered, live_version);
    assert_eq!(previous_frame_version, initial);
    assert_eq!(
        receiver.take_frame(frame).unwrap().document_version(),
        Some(live_version)
    );
    assert_eq!(engine.shutdown().reason(), WorkerStopReason::Requested);
}

#[test]
fn restored_document_operation_has_exact_cancel_identity_after_newer_navigation_fails() {
    let (mut engine, mut receiver, entered, release, invalidated) =
        spawn_configured_gated_document(limits(6, 16, 1, 4, 4), false, false, true);
    let first = engine.navigate(context(1), request()).unwrap();
    let initial = loaded_version(&mut receiver, first);

    let second = engine.navigate(context(1), request()).unwrap();
    assert_eq!(
        next(&mut receiver).kind(),
        EngineEventKind::NavigationStarted { navigation: second }
    );
    assert_eq!(
        next(&mut receiver).kind(),
        EngineEventKind::NavigationFailed {
            navigation: second,
            failure: ExecutionFailure::new(ExecutionFailureKind::Rejected, NavigationStage::Fetch,),
        }
    );
    assert_eq!(
        engine.latest_generation(context(1)),
        Some(second.generation())
    );

    let operation = mutation_operation(
        engine
            .mutate_document(first, one_created_text(initial))
            .unwrap(),
    );
    entered.recv().unwrap();
    assert_eq!(
        engine.cancel_navigation(first).unwrap_err().kind(),
        CommandErrorKind::NotCurrentNavigation
    );
    assert_eq!(
        engine.cancel_navigation(second).unwrap_err().kind(),
        CommandErrorKind::NoActiveNavigation
    );
    assert_eq!(
        engine
            .cancel_document_operation(second, operation)
            .unwrap_err()
            .kind(),
        CommandErrorKind::DocumentOperationNavigationMismatch { current: first }
    );
    assert_eq!(
        engine.cancel_document_operation(first, operation).unwrap(),
        CommandReceipt::DocumentOperationCancellationRequested {
            navigation: first,
            operation,
        }
    );
    release.send(()).unwrap();

    let EngineEventKind::DocumentMutationCommittedWithoutFrame {
        navigation,
        operation: published_operation,
        live_version,
        frame_version,
        result,
        failure: DocumentOperationFailure::Cancelled,
        ..
    } = next(&mut receiver).kind()
    else {
        panic!("postcommit cancellation must publish the restored operation");
    };
    assert_eq!(navigation, first);
    assert_eq!(published_operation, operation);
    assert_eq!(live_version.revision(), initial.revision() + 1);
    assert_eq!(frame_version, initial);
    assert_eq!(
        receiver.take_mutation_result(result).unwrap().operation(),
        operation
    );

    assert_eq!(
        engine
            .try_send(EngineCommand::Navigate {
                navigation: first,
                request: request(),
            })
            .unwrap_err()
            .kind(),
        CommandErrorKind::NonMonotonicGeneration {
            latest: second.generation(),
        }
    );
    assert_eq!(
        engine.close_context(first).unwrap_err().kind(),
        CommandErrorKind::NotCurrentNavigation
    );
    assert_eq!(
        engine.close_context(second).unwrap(),
        CommandReceipt::ContextCloseRequested(second)
    );
    assert_eq!(
        next(&mut receiver).kind(),
        EngineEventKind::ContextClosed { navigation: second }
    );
    assert!(invalidated.load(Ordering::SeqCst));
    assert_eq!(engine.shutdown().reason(), WorkerStopReason::Requested);
}

#[test]
fn precommit_document_operation_cancel_is_atomic_and_navigation_cancel_is_isolated() {
    let (mut engine, mut receiver, entered, release, _) =
        spawn_configured_gated_document(limits(4, 12, 1, 4, 4), true, false, false);
    let navigation = engine.navigate(context(1), request()).unwrap();
    let initial = loaded_version(&mut receiver, navigation);
    let operation = mutation_operation(
        engine
            .mutate_document(navigation, one_created_text(initial))
            .unwrap(),
    );
    entered.recv().unwrap();

    assert_eq!(
        engine.cancel_navigation(navigation).unwrap_err().kind(),
        CommandErrorKind::NoActiveNavigation
    );
    assert_eq!(
        engine
            .cancel_document_operation(navigation, operation)
            .unwrap(),
        CommandReceipt::DocumentOperationCancellationRequested {
            navigation,
            operation,
        }
    );
    assert_eq!(
        engine
            .cancel_document_operation(navigation, operation)
            .unwrap_err()
            .kind(),
        CommandErrorKind::NoActiveDocumentOperation
    );
    release.send(()).unwrap();

    assert_eq!(
        next(&mut receiver).kind(),
        EngineEventKind::DocumentMutationRejected {
            navigation,
            operation,
            live_version: Some(initial),
            frame_version: Some(initial),
            failure: DocumentOperationFailure::Cancelled,
        }
    );
    let rerender_operation =
        rerender_operation(engine.rerender_document(navigation, initial).unwrap());
    assert!(rerender_operation.get() > operation.get());
    let EngineEventKind::DocumentRerendered {
        operation: published_operation,
        live_version,
        frame,
        ..
    } = next(&mut receiver).kind()
    else {
        panic!("atomic precommit cancellation must retain an exact rerenderable document");
    };
    assert_eq!(published_operation, rerender_operation);
    assert_eq!(live_version, initial);
    assert_eq!(
        receiver.take_frame(frame).unwrap().document_version(),
        Some(initial)
    );
    assert_eq!(engine.shutdown().reason(), WorkerStopReason::Requested);
}

#[test]
fn rerender_cancel_uses_exact_operation_identity_and_cleans_up_before_later_work() {
    let (mut engine, mut receiver, entered, release, _) =
        spawn_configured_gated_document(limits(4, 12, 1, 4, 4), false, true, false);
    let navigation = engine.navigate(context(1), request()).unwrap();
    let initial = loaded_version(&mut receiver, navigation);

    let first_operation =
        rerender_operation(engine.rerender_document(navigation, initial).unwrap());
    entered.recv().unwrap();
    assert_eq!(
        engine.cancel_navigation(navigation).unwrap_err().kind(),
        CommandErrorKind::NoActiveNavigation
    );
    assert_eq!(
        engine
            .cancel_document_operation(navigation, first_operation)
            .unwrap(),
        CommandReceipt::DocumentOperationCancellationRequested {
            navigation,
            operation: first_operation,
        }
    );
    release.send(()).unwrap();
    assert_eq!(
        next(&mut receiver).kind(),
        EngineEventKind::DocumentRerenderRejected {
            navigation,
            operation: first_operation,
            live_version: Some(initial),
            frame_version: Some(initial),
            failure: DocumentOperationFailure::Cancelled,
        }
    );
    assert_eq!(
        engine
            .cancel_document_operation(navigation, first_operation)
            .unwrap_err()
            .kind(),
        CommandErrorKind::NoActiveDocumentOperation
    );

    let second_operation =
        rerender_operation(engine.rerender_document(navigation, initial).unwrap());
    entered.recv().unwrap();
    assert!(second_operation.get() > first_operation.get());
    assert_eq!(
        engine
            .cancel_document_operation(navigation, first_operation)
            .unwrap_err()
            .kind(),
        CommandErrorKind::NotCurrentDocumentOperation {
            current: second_operation,
        }
    );
    release.send(()).unwrap();
    let EngineEventKind::DocumentRerendered {
        operation: published_operation,
        live_version,
        frame,
        ..
    } = next(&mut receiver).kind()
    else {
        panic!("stale cancellation must not affect the later rerender");
    };
    assert_eq!(published_operation, second_operation);
    assert_eq!(live_version, initial);
    assert_eq!(
        receiver.take_frame(frame).unwrap().document_version(),
        Some(initial)
    );
    assert_eq!(
        engine
            .cancel_document_operation(navigation, second_operation)
            .unwrap_err()
            .kind(),
        CommandErrorKind::NoActiveDocumentOperation
    );
    assert_eq!(engine.shutdown().reason(), WorkerStopReason::Requested);
}

#[test]
fn delayed_document_operation_id_cannot_cancel_later_work_under_same_navigation() {
    let (mut engine, mut receiver, entered, release, _) =
        spawn_gated_document(limits(6, 16, 1, 4, 4));
    let navigation = engine.navigate(context(1), request()).unwrap();
    let initial = loaded_version(&mut receiver, navigation);

    let first_operation = mutation_operation(
        engine
            .mutate_document(navigation, one_created_text(initial))
            .unwrap(),
    );
    entered.recv().unwrap();
    release.send(()).unwrap();
    let EngineEventKind::DocumentMutationRendered {
        operation: published_first,
        live_version,
        result,
        frame,
        ..
    } = next(&mut receiver).kind()
    else {
        panic!("first mutation must publish");
    };
    assert_eq!(published_first, first_operation);
    assert_eq!(
        receiver.take_mutation_result(result).unwrap().operation(),
        first_operation
    );
    receiver.take_frame(frame).unwrap();

    let second_operation = mutation_operation(
        engine
            .mutate_document(navigation, one_created_text(live_version))
            .unwrap(),
    );
    assert!(second_operation.get() > first_operation.get());
    entered.recv().unwrap();
    assert_eq!(
        engine
            .cancel_document_operation(navigation, first_operation)
            .unwrap_err()
            .kind(),
        CommandErrorKind::NotCurrentDocumentOperation {
            current: second_operation,
        }
    );
    let wrong_generation = NavigationId::new(
        navigation.context(),
        navigation.generation().checked_next().unwrap(),
    );
    assert_eq!(
        engine
            .cancel_document_operation(wrong_generation, second_operation)
            .unwrap_err()
            .kind(),
        CommandErrorKind::DocumentOperationNavigationMismatch {
            current: navigation,
        }
    );
    assert_eq!(
        engine
            .cancel_document_operation(
                NavigationId::new(context(2), NavigationGeneration::INITIAL),
                second_operation,
            )
            .unwrap_err()
            .kind(),
        CommandErrorKind::UnknownContext
    );
    assert_eq!(
        engine.cancel_navigation(navigation).unwrap_err().kind(),
        CommandErrorKind::NoActiveNavigation
    );
    release.send(()).unwrap();

    let EngineEventKind::DocumentMutationRendered {
        operation: published_second,
        result,
        frame,
        ..
    } = next(&mut receiver).kind()
    else {
        panic!("wrong and stale controls must not cancel the later operation");
    };
    assert_eq!(published_second, second_operation);
    assert_eq!(
        receiver.take_mutation_result(result).unwrap().operation(),
        second_operation
    );
    receiver.take_frame(frame).unwrap();
    assert_eq!(
        engine
            .cancel_document_operation(navigation, second_operation)
            .unwrap_err()
            .kind(),
        CommandErrorKind::NoActiveDocumentOperation
    );
    assert_eq!(engine.shutdown().reason(), WorkerStopReason::Requested);
}

#[test]
fn document_operation_owner_prevents_cross_engine_sequence_collision() {
    let (mut first_engine, mut first_receiver, first_entered, first_release, _) =
        spawn_gated_document(limits(4, 12, 1, 4, 4));
    let (mut second_engine, mut second_receiver, second_entered, second_release, _) =
        spawn_gated_document(limits(4, 12, 1, 4, 4));
    let first_navigation = first_engine.navigate(context(1), request()).unwrap();
    let second_navigation = second_engine.navigate(context(1), request()).unwrap();
    let first_version = loaded_version(&mut first_receiver, first_navigation);
    let second_version = loaded_version(&mut second_receiver, second_navigation);
    let first_operation = mutation_operation(
        first_engine
            .mutate_document(first_navigation, one_created_text(first_version))
            .unwrap(),
    );
    let second_operation = mutation_operation(
        second_engine
            .mutate_document(second_navigation, one_created_text(second_version))
            .unwrap(),
    );
    first_entered.recv().unwrap();
    second_entered.recv().unwrap();
    assert_eq!(first_operation.get(), second_operation.get());
    assert_ne!(first_operation, second_operation);
    assert_eq!(
        second_engine
            .cancel_document_operation(second_navigation, first_operation)
            .unwrap_err()
            .kind(),
        CommandErrorKind::NotCurrentDocumentOperation {
            current: second_operation,
        }
    );

    first_release.send(()).unwrap();
    second_release.send(()).unwrap();
    for (receiver, operation) in [
        (&mut first_receiver, first_operation),
        (&mut second_receiver, second_operation),
    ] {
        let EngineEventKind::DocumentMutationRendered {
            operation: published,
            result,
            frame,
            ..
        } = next(receiver).kind()
        else {
            panic!("foreign-owner cancellation must not affect either engine");
        };
        assert_eq!(published, operation);
        assert_eq!(
            receiver.take_mutation_result(result).unwrap().operation(),
            operation
        );
        receiver.take_frame(frame).unwrap();
    }
    assert_eq!(
        first_engine.shutdown().reason(),
        WorkerStopReason::Requested
    );
    assert_eq!(
        second_engine.shutdown().reason(),
        WorkerStopReason::Requested
    );
}

#[test]
fn superseding_navigation_discards_a_hidden_committed_mutation_without_stale_events() {
    let (mut engine, mut receiver, entered, release, invalidated) =
        spawn_gated_document(limits(4, 12, 1, 4, 4));
    let first = engine.navigate(context(1), request()).unwrap();
    let initial = loaded_version(&mut receiver, first);
    let operation = mutation_operation(
        engine
            .mutate_document(first, one_created_text(initial))
            .unwrap(),
    );
    entered.recv().unwrap();

    let second = engine.navigate(context(1), request()).unwrap();
    assert_eq!(
        engine
            .cancel_document_operation(first, operation)
            .unwrap_err()
            .kind(),
        CommandErrorKind::NotCurrentNavigation
    );
    release.send(()).unwrap();
    let second_version = loaded_version(&mut receiver, second);
    assert_ne!(second_version.document_id(), initial.document_id());
    assert!(invalidated.load(Ordering::SeqCst));
    assert!(matches!(receiver.try_recv(), Err(EventReceiveError::Empty)));
    assert_eq!(engine.shutdown().reason(), WorkerStopReason::Requested);
}

#[test]
fn successful_navigation_replacement_revokes_prior_document_result_leases() {
    let (mut engine, mut receiver, entered, release, _) =
        spawn_gated_document(limits(4, 12, 1, 4, 4));
    let first = engine.navigate(context(1), request()).unwrap();
    let initial = loaded_version(&mut receiver, first);
    engine
        .mutate_document(first, one_created_text(initial))
        .unwrap();
    entered.recv().unwrap();
    release.send(()).unwrap();
    let mutation = next(&mut receiver);
    let EngineEventKind::DocumentMutationRendered { result, frame, .. } = mutation.kind() else {
        panic!("the mutation must publish before replacement");
    };
    receiver.take_frame(frame).unwrap();

    let second = engine.navigate(context(1), request()).unwrap();
    let _ = loaded_version(&mut receiver, second);
    assert!(matches!(
        receiver.take_mutation_result(result),
        Err(MutationResultLeaseError::Stale)
    ));
    assert_eq!(engine.shutdown().reason(), WorkerStopReason::Requested);
}

struct TypedThenNavigationOnlyExecutor {
    executions: usize,
    document: Option<Document>,
    frame_version: Option<DocumentVersion>,
    pending_navigation_only: Option<NavigationId>,
}

impl NavigationExecutor for TypedThenNavigationOnlyExecutor {
    fn execute(
        &mut self,
        navigation: NavigationId,
        _request: &NavigationRequest,
        _cancellation: &wild_buzzard_engine::CancellationToken,
    ) -> Result<ExecutorOutput, ExecutionFailure> {
        self.executions += 1;
        if self.executions == 2 {
            self.pending_navigation_only = Some(navigation);
            return ExecutorOutput::new(
                200,
                EngineFrame::from_rgba8(PixelSize::new(1, 1).unwrap(), vec![2, 0, 0, 255]).unwrap(),
            );
        }

        let mut document = Document::new();
        let html = document.create_html_element("html").unwrap();
        document
            .append_child(document.document_node(), html)
            .unwrap();
        let version = document.version();
        let proof = DocumentLoadProof::from_snapshot(&document.snapshot().unwrap());
        self.document = Some(document);
        self.frame_version = Some(version);
        ExecutorOutput::new_document(200, document_frame(version, 3), proof)
    }

    fn mutate_document(
        &mut self,
        _navigation: NavigationId,
        batch: ScriptMutationBatch,
        _cancellation: &wild_buzzard_engine::CancellationToken,
    ) -> ExecutorDocumentMutation {
        let document = self.document.as_mut().unwrap();
        let previous_live_version = document.version();
        let previous_frame_version = self.frame_version.unwrap();
        let commit = document
            .apply_script_mutations(batch, ScriptMutationLimits::DEFAULT)
            .unwrap();
        let live_version = commit.version();
        let commit = DocumentMutationCommit::from_script_commit(commit);
        self.frame_version = Some(live_version);
        ExecutorDocumentMutation::Rendered {
            previous_live_version,
            previous_frame_version,
            commit,
            frame: document_frame(live_version, 4),
        }
    }

    fn acknowledge_navigation_publication(&mut self, navigation: NavigationId, published: bool) {
        if self.pending_navigation_only == Some(navigation) {
            if published {
                self.document = None;
                self.frame_version = None;
            }
            self.pending_navigation_only = None;
        }
    }

    fn invalidate_document(&mut self, _context: TopLevelContextId) {
        self.document = None;
        self.frame_version = None;
        self.pending_navigation_only = None;
    }

    fn close_context(&mut self, context: TopLevelContextId) {
        self.invalidate_document(context);
    }

    fn shutdown(&mut self) -> Result<(), ExecutionFailure> {
        Ok(())
    }
}

#[test]
fn navigation_only_replacement_retires_typed_document_charge_and_result_leases() {
    let document_limited = limits(6, 16, 2, 4, 8)
        .with_max_retained_document_nodes(3)
        .unwrap();
    let (mut engine, mut receiver) =
        NavigationEngine::spawn_with_executor(document_limited, || {
            Ok(TypedThenNavigationOnlyExecutor {
                executions: 0,
                document: None,
                frame_version: None,
                pending_navigation_only: None,
            })
        })
        .unwrap();

    let first = engine.navigate(context(1), request()).unwrap();
    let first_version = loaded_version(&mut receiver, first);
    engine
        .mutate_document(first, one_created_text(first_version))
        .unwrap();
    let EngineEventKind::DocumentMutationRendered {
        live_version,
        result,
        frame,
        ..
    } = next(&mut receiver).kind()
    else {
        panic!("the typed document mutation must publish before replacement");
    };
    receiver.take_frame(frame).unwrap();

    let second = engine.navigate(context(1), request()).unwrap();
    assert_eq!(
        next(&mut receiver).kind(),
        EngineEventKind::NavigationStarted { navigation: second }
    );
    assert!(matches!(
        next(&mut receiver).kind(),
        EngineEventKind::NavigationCommitted { navigation, .. } if navigation == second
    ));
    let second_frame = frame_ready(next(&mut receiver), second);
    assert_eq!(
        receiver
            .take_frame(second_frame)
            .unwrap()
            .document_version(),
        None
    );
    assert_eq!(
        receiver.take_mutation_result(result).unwrap_err(),
        MutationResultLeaseError::Stale
    );
    assert_eq!(
        engine
            .mutate_document(second, one_created_text(live_version))
            .unwrap_err()
            .kind(),
        CommandErrorKind::NoLiveDocument
    );

    let third = engine.navigate(context(2), request()).unwrap();
    let _ = loaded_version(&mut receiver, third);
    assert_eq!(engine.shutdown().reason(), WorkerStopReason::Requested);
}

#[test]
fn stale_generation_controls_cannot_target_the_current_context_incarnation() {
    let (mut engine, mut receiver, _) = spawn_immediate(limits(4, 16, 1, 4, 4));
    let first = engine.navigate(context(1), request()).unwrap();
    assert_eq!(
        next(&mut receiver).kind(),
        EngineEventKind::NavigationStarted { navigation: first }
    );
    assert!(matches!(
        next(&mut receiver).kind(),
        EngineEventKind::NavigationCommitted { navigation, .. } if navigation == first
    ));
    let first_frame = frame_ready(next(&mut receiver), first);
    receiver.take_frame(first_frame).unwrap();

    let second = engine.navigate(context(1), request()).unwrap();
    assert_eq!(
        next(&mut receiver).kind(),
        EngineEventKind::NavigationStarted { navigation: second }
    );
    assert!(matches!(
        next(&mut receiver).kind(),
        EngineEventKind::NavigationCommitted { navigation, .. } if navigation == second
    ));
    let second_frame = frame_ready(next(&mut receiver), second);
    receiver.take_frame(second_frame).unwrap();

    assert_eq!(
        engine
            .try_send(EngineCommand::Cancel { navigation: first })
            .unwrap_err()
            .kind(),
        CommandErrorKind::NotCurrentNavigation
    );
    assert_eq!(
        engine.close_context(first).unwrap_err().kind(),
        CommandErrorKind::NotCurrentNavigation
    );
    assert_eq!(
        engine
            .try_send(EngineCommand::Navigate {
                navigation: first,
                request: request(),
            })
            .unwrap_err()
            .kind(),
        CommandErrorKind::NonMonotonicGeneration {
            latest: second.generation(),
        }
    );
    assert_eq!(
        engine.close_context(second).unwrap(),
        CommandReceipt::ContextCloseRequested(second)
    );
    assert_eq!(
        next(&mut receiver).kind(),
        EngineEventKind::ContextClosed { navigation: second }
    );
    assert_eq!(engine.shutdown().reason(), WorkerStopReason::Requested);
}

#[test]
fn close_context_invalidates_inflight_state_and_permanently_retires_its_identity() {
    let (mut engine, mut receiver, entered, release, invalidated) =
        spawn_gated_document(limits(4, 12, 1, 4, 4));
    let first = engine.navigate(context(1), request()).unwrap();
    let initial = loaded_version(&mut receiver, first);
    let operation = mutation_operation(
        engine
            .mutate_document(first, one_created_text(initial))
            .unwrap(),
    );
    entered.recv().unwrap();
    assert_eq!(
        engine.close_context(first).unwrap(),
        CommandReceipt::ContextCloseRequested(first)
    );
    assert_eq!(
        engine
            .cancel_document_operation(first, operation)
            .unwrap_err()
            .kind(),
        CommandErrorKind::UnknownContext
    );
    assert_eq!(
        engine.navigate(context(1), request()).unwrap_err().kind(),
        CommandErrorKind::ContextClosing
    );
    assert_eq!(
        engine.navigate(context(2), request()).unwrap_err().kind(),
        CommandErrorKind::ContextLimitReached { maximum: 1 }
    );
    release.send(()).unwrap();
    assert_eq!(
        next(&mut receiver).kind(),
        EngineEventKind::ContextClosed { navigation: first }
    );
    assert!(invalidated.load(Ordering::SeqCst));

    assert_eq!(
        engine
            .try_send(EngineCommand::Cancel { navigation: first })
            .unwrap_err()
            .kind(),
        CommandErrorKind::UnknownContext
    );
    assert_eq!(
        engine.close_context(first).unwrap_err().kind(),
        CommandErrorKind::UnknownContext
    );
    assert!(matches!(
        engine.navigate(context(1), request()).unwrap_err().kind(),
        CommandErrorKind::ContextIdentityRetired { latest } if latest == context(1)
    ));
    assert!(matches!(
        engine
            .try_send(EngineCommand::Navigate {
                navigation: first,
                request: request(),
            })
            .unwrap_err()
            .kind(),
        CommandErrorKind::ContextIdentityRetired { latest } if latest == context(1)
    ));

    let replacement = engine.navigate(context(2), request()).unwrap();
    assert_eq!(replacement.generation(), NavigationGeneration::INITIAL);
    let _ = loaded_version(&mut receiver, replacement);
    assert_eq!(engine.shutdown().reason(), WorkerStopReason::Requested);
}

const REAL_DOCUMENT: &str =
    "<!doctype html><style>body{margin:0;background:#246}</style><p>live</p>";

fn serve_page() -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        consume_head(&mut stream);
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            REAL_DOCUMENT.len(),
            REAL_DOCUMENT
        );
        stream.write_all(response.as_bytes()).unwrap();
    });
    (format!("http://{address}/page"), server)
}

fn consume_head(stream: &mut TcpStream) {
    let mut received = Vec::new();
    let mut buffer = [0_u8; 256];
    while !received.ends_with(b"\r\n\r\n") {
        let count = stream.read(&mut buffer).unwrap();
        assert!(count > 0);
        received.extend_from_slice(&buffer[..count]);
        assert!(received.len() <= 8 * 1024);
    }
}

fn real_config() -> StaticPageConfig {
    StaticPageConfig {
        viewport_width: 64,
        viewport_height: 32,
        operation_timeout: Duration::from_secs(15),
        font_source: FontSourcePolicy::EmbeddedOnly,
        network: wild_buzzard_net::ClientConfig::default()
            .with_max_body_bytes(64 * 1024)
            .with_connect_timeout(Duration::from_secs(1))
            .with_read_timeout(Duration::from_secs(2))
            .with_write_timeout(Duration::from_secs(2)),
        headless: wild_buzzard_headless::HeadlessLimits::default()
            .with_max_width(64)
            .with_max_height(32)
            .with_max_pixel_bytes(64 * 32 * 4),
        ..StaticPageConfig::default()
    }
}

fn navigate_real(
    engine: &NavigationEngine,
    receiver: &mut EngineEventReceiver,
    context: TopLevelContextId,
) -> DocumentVersion {
    let (url, server) = serve_page();
    let navigation = engine
        .navigate(context, NavigationRequest::new(&url).unwrap())
        .unwrap();
    let version = loaded_version(receiver, navigation);
    server.join().unwrap();
    version
}

#[test]
fn real_worker_retains_independent_context_pages_for_mutation_and_exact_rerender() {
    let frame_bytes = 64 * 32 * 4;
    let engine_limits = limits(8, 16, 2, frame_bytes, frame_bytes * 2);
    let (mut engine, mut receiver) = NavigationEngine::spawn(real_config(), engine_limits).unwrap();
    let first_context = context(1);
    let second_context = context(2);
    let first_version = navigate_real(&engine, &mut receiver, first_context);
    let second_version = navigate_real(&engine, &mut receiver, second_context);
    assert_ne!(first_version.document_id(), second_version.document_id());

    let first_navigation = NavigationId::new(first_context, NavigationGeneration::INITIAL);
    engine
        .mutate_document(first_navigation, one_created_text(first_version))
        .unwrap();
    let mutation = next(&mut receiver);
    let EngineEventKind::DocumentMutationRendered {
        live_version,
        result,
        frame,
        ..
    } = mutation.kind()
    else {
        panic!("the first context must retain its exact page after loading the second");
    };
    assert_eq!(live_version.revision(), first_version.revision() + 1);
    assert_eq!(
        receiver
            .take_mutation_result(result)
            .unwrap()
            .created_nodes()
            .len(),
        1
    );
    assert_eq!(
        receiver.take_frame(frame).unwrap().document_version(),
        Some(live_version)
    );

    let second_navigation = NavigationId::new(second_context, NavigationGeneration::INITIAL);
    engine
        .rerender_document(second_navigation, second_version)
        .unwrap();
    let rerender = next(&mut receiver);
    let EngineEventKind::DocumentRerendered {
        live_version: rendered,
        frame,
        ..
    } = rerender.kind()
    else {
        panic!("the second context must retain its independent exact page");
    };
    assert_eq!(rendered, second_version);
    assert_eq!(
        receiver.take_frame(frame).unwrap().document_version(),
        Some(second_version)
    );
    assert_eq!(engine.shutdown().reason(), WorkerStopReason::Requested);
}

struct PendingDocumentBudgetExecutor {
    documents: BTreeMap<TopLevelContextId, (Document, DocumentVersion)>,
    gated_context: TopLevelContextId,
    navigation_entered: mpsc::Sender<()>,
    navigation_release: mpsc::Receiver<()>,
    pending_new_context: Option<TopLevelContextId>,
}

impl NavigationExecutor for PendingDocumentBudgetExecutor {
    fn execute(
        &mut self,
        navigation: NavigationId,
        _request: &NavigationRequest,
        _cancellation: &wild_buzzard_engine::CancellationToken,
    ) -> Result<ExecutorOutput, ExecutionFailure> {
        let mut document = Document::new();
        let html = document.create_html_element("html").unwrap();
        document
            .append_child(document.document_node(), html)
            .unwrap();
        let version = document.version();
        let proof = DocumentLoadProof::from_snapshot(&document.snapshot().unwrap());
        assert!(
            self.documents
                .insert(navigation.context(), (document, version))
                .is_none()
        );
        self.pending_new_context = Some(navigation.context());
        if navigation.context() == self.gated_context {
            self.navigation_entered.send(()).unwrap();
            self.navigation_release.recv().unwrap();
        }
        ExecutorOutput::new_document(200, document_frame(version, 1), proof)
    }

    fn mutate_document(
        &mut self,
        navigation: NavigationId,
        batch: ScriptMutationBatch,
        _cancellation: &wild_buzzard_engine::CancellationToken,
    ) -> ExecutorDocumentMutation {
        let (document, frame_version) = self.documents.get_mut(&navigation.context()).unwrap();
        let previous_live_version = document.version();
        let previous_frame_version = *frame_version;
        let commit = document
            .apply_script_mutations(batch, ScriptMutationLimits::DEFAULT)
            .unwrap();
        let live_version = commit.version();
        let commit = DocumentMutationCommit::from_script_commit(commit);
        *frame_version = live_version;
        ExecutorDocumentMutation::Rendered {
            previous_live_version,
            previous_frame_version,
            commit,
            frame: document_frame(live_version, 2),
        }
    }

    fn acknowledge_navigation_publication(&mut self, navigation: NavigationId, published: bool) {
        assert_eq!(self.pending_new_context, Some(navigation.context()));
        if !published {
            self.documents.remove(&navigation.context());
        }
        self.pending_new_context = None;
    }

    fn invalidate_document(&mut self, context: TopLevelContextId) {
        self.documents.remove(&context);
    }

    fn close_context(&mut self, context: TopLevelContextId) {
        self.documents.remove(&context);
    }

    fn shutdown(&mut self) -> Result<(), ExecutionFailure> {
        Ok(())
    }
}

#[test]
fn pending_mutation_nodes_block_a_cross_context_navigation_without_losing_the_mutation() {
    let second_context = context(2);
    let (entered_sender, entered_receiver) = mpsc::channel();
    let (release_sender, release_receiver) = mpsc::channel();
    let document_limited = limits(4, 12, 2, 4, 8)
        .with_max_retained_document_nodes(3)
        .unwrap();
    let (mut engine, mut receiver) =
        NavigationEngine::spawn_with_executor(document_limited, move || {
            Ok(PendingDocumentBudgetExecutor {
                documents: BTreeMap::new(),
                gated_context: second_context,
                navigation_entered: entered_sender,
                navigation_release: release_receiver,
                pending_new_context: None,
            })
        })
        .unwrap();

    let first = engine.navigate(context(1), request()).unwrap();
    let first_version = loaded_version(&mut receiver, first);
    let second = engine.navigate(second_context, request()).unwrap();
    entered_receiver.recv().unwrap();
    assert_eq!(
        next(&mut receiver).kind(),
        EngineEventKind::NavigationStarted { navigation: second }
    );
    engine
        .mutate_document(first, one_created_text(first_version))
        .unwrap();
    release_sender.send(()).unwrap();

    assert_eq!(
        next(&mut receiver).kind(),
        EngineEventKind::NavigationFailed {
            navigation: second,
            failure: ExecutionFailure::new(
                ExecutionFailureKind::ResourceLimit,
                NavigationStage::Document,
            ),
        }
    );
    let EngineEventKind::DocumentMutationRendered {
        live_version,
        result,
        frame,
        created_nodes: 1,
        ..
    } = next(&mut receiver).kind()
    else {
        panic!("the reserved first-context mutation must still publish");
    };
    assert_eq!(live_version.revision(), first_version.revision() + 1);
    assert_eq!(
        receiver
            .take_mutation_result(result)
            .unwrap()
            .created_nodes()
            .len(),
        1
    );
    assert_eq!(
        receiver.take_frame(frame).unwrap().document_version(),
        Some(live_version)
    );
    assert_eq!(engine.shutdown().reason(), WorkerStopReason::Requested);
}

struct SaturationDocumentExecutor {
    documents: BTreeMap<TopLevelContextId, DocumentVersion>,
    gated_context: TopLevelContextId,
    navigation_entered: mpsc::Sender<()>,
    navigation_release: mpsc::Receiver<()>,
    mutation_entered: Arc<AtomicBool>,
    shutdown: mpsc::Sender<()>,
}

impl NavigationExecutor for SaturationDocumentExecutor {
    fn execute(
        &mut self,
        navigation: NavigationId,
        _request: &NavigationRequest,
        _cancellation: &wild_buzzard_engine::CancellationToken,
    ) -> Result<ExecutorOutput, ExecutionFailure> {
        let document = Document::new();
        let version = document.version();
        let proof = DocumentLoadProof::from_snapshot(&document.snapshot().unwrap());
        self.documents.insert(navigation.context(), version);
        if navigation.context() == self.gated_context {
            self.navigation_entered.send(()).unwrap();
            self.navigation_release.recv().unwrap();
        }
        ExecutorOutput::new_document(200, document_frame(version, 1), proof)
    }

    fn mutate_document(
        &mut self,
        navigation: NavigationId,
        _batch: ScriptMutationBatch,
        _cancellation: &wild_buzzard_engine::CancellationToken,
    ) -> ExecutorDocumentMutation {
        self.mutation_entered.store(true, Ordering::SeqCst);
        let version = self.documents.get(&navigation.context()).copied();
        ExecutorDocumentMutation::Rejected {
            live_version: version,
            frame_version: version,
            failure: DocumentOperationFailure::MutationRejected,
        }
    }

    fn shutdown(&mut self) -> Result<(), ExecutionFailure> {
        let _ = self.shutdown.send(());
        Ok(())
    }
}

#[test]
fn saturated_event_queue_rejects_dynamic_work_before_executor_entry() {
    let second_context = context(2);
    let (entered_sender, entered_receiver) = mpsc::channel();
    let (release_sender, release_receiver) = mpsc::channel();
    let (shutdown_sender, shutdown_receiver) = mpsc::channel();
    let mutation_entered = Arc::new(AtomicBool::new(false));
    let executor_mutation_entered = Arc::clone(&mutation_entered);
    let (mut engine, mut receiver) =
        NavigationEngine::spawn_with_executor(limits(4, 3, 2, 4, 8), move || {
            Ok(SaturationDocumentExecutor {
                documents: BTreeMap::new(),
                gated_context: second_context,
                navigation_entered: entered_sender,
                navigation_release: release_receiver,
                mutation_entered: executor_mutation_entered,
                shutdown: shutdown_sender,
            })
        })
        .unwrap();
    let first_context = context(1);
    let first_navigation = engine.navigate(first_context, request()).unwrap();
    let first_version = loaded_version(&mut receiver, first_navigation);

    let second = engine.navigate(second_context, request()).unwrap();
    entered_receiver.recv().unwrap();
    engine
        .mutate_document(first_navigation, one_created_text(first_version))
        .unwrap();
    release_sender.send(()).unwrap();
    shutdown_receiver.recv().unwrap();
    assert!(!mutation_entered.load(Ordering::SeqCst));

    assert_eq!(
        next(&mut receiver).kind(),
        EngineEventKind::NavigationStarted { navigation: second }
    );
    assert!(matches!(
        next(&mut receiver).kind(),
        EngineEventKind::NavigationCommitted { navigation, .. } if navigation == second
    ));
    assert!(matches!(
        next(&mut receiver).kind(),
        EngineEventKind::FrameReady { navigation, .. } if navigation == second
    ));
    let terminal = next(&mut receiver);
    let EngineEventKind::ShutdownComplete { status } = terminal.kind() else {
        panic!("the reserved terminal slot must expose backpressure shutdown");
    };
    assert_eq!(status.reason(), WorkerStopReason::EventQueueSaturated);
    assert_eq!(engine.shutdown(), status);
}
