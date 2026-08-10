use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::rc::Rc;

use wild_buzzard_engine::{
    FrameLeaseError, MutationResultLeaseError, NavigationNetworkCapability, NavigationRequest,
};
use wild_buzzard_linux::{InputOrigin, LinuxBackend, LinuxWindowEvent};
use wild_buzzard_platform::{
    EventSequence, EventTimestampMicros, InputDeviceId, InputEvent, InputMetadata, KeyEvent,
    KeyLocation, KeyState, LogicalPoint, Modifiers, PhysicalKeyCode, PhysicalSize, PixelFormat,
    PointerEvent, PointerId, PointerKind, PointerPhase, ScaleFactor, SeatId, SurfaceDescriptor,
    SurfaceId, SurfaceIdAllocator, SurfaceNamespace, SurfaceRole,
};
use wild_buzzard_ui::{
    AddressSelection, BrowserCommand, BrowserCommandOutcome, BrowserNavigationMode, BrowserSession,
    BrowserTabId, BrowserWindowId, EngineDocumentVersion, EngineFrameDescriptor, EngineFrameLease,
    EnginePort, EnginePortError, EnginePortEvent, EnginePortEventKind, EnginePortExecutorShutdown,
    EnginePortFrameLeaseId, EnginePortMutationLeaseId, EnginePortSequence,
    EnginePortShutdownStatus, EnginePortStopReason, EnginePumpOutcome, ExecutionFailure,
    ExecutionFailureKind, LinuxEventOutcome, NavigationGeneration, NavigationId, NavigationPhase,
    NavigationStage, SessionError, SessionFailure, SessionLifecycle, SessionLimits,
    TopLevelContextId,
};

fn clean_shutdown() -> EnginePortShutdownStatus {
    EnginePortShutdownStatus::new(
        EnginePortStopReason::Requested,
        EnginePortExecutorShutdown::Clean,
    )
}

struct FakeFrame {
    navigation: NavigationId,
    width: u32,
    height: u32,
    pixels: Vec<u8>,
    document_version: Option<EngineDocumentVersion>,
}

struct FakeState {
    generations: BTreeMap<TopLevelContextId, NavigationGeneration>,
    navigation_override: Option<NavigationId>,
    navigations: Vec<(TopLevelContextId, Box<str>)>,
    navigation_capabilities: Vec<NavigationNetworkCapability>,
    cancellations: Vec<NavigationId>,
    close_calls: Vec<NavigationId>,
    close_failure_on: Option<usize>,
    events: VecDeque<EnginePortEvent>,
    next_event_sequence: u64,
    frames: BTreeMap<EnginePortFrameLeaseId, FakeFrame>,
    stale_frames: BTreeSet<EnginePortFrameLeaseId>,
    frame_transfers: Vec<(NavigationId, EnginePortFrameLeaseId)>,
    panic_on_poll: bool,
    receiver_closed: Option<EnginePortShutdownStatus>,
    shutdown_calls: usize,
}

impl Default for FakeState {
    fn default() -> Self {
        Self {
            generations: BTreeMap::new(),
            navigation_override: None,
            navigations: Vec::new(),
            navigation_capabilities: Vec::new(),
            cancellations: Vec::new(),
            close_calls: Vec::new(),
            close_failure_on: None,
            events: VecDeque::new(),
            next_event_sequence: 1,
            frames: BTreeMap::new(),
            stale_frames: BTreeSet::new(),
            frame_transfers: Vec::new(),
            panic_on_poll: false,
            receiver_closed: None,
            shutdown_calls: 0,
        }
    }
}

#[derive(Clone)]
struct FakeHandle(Rc<RefCell<FakeState>>);

impl FakeHandle {
    fn push(&self, kind: EnginePortEventKind) {
        let mut state = self.0.borrow_mut();
        let sequence = EnginePortSequence::new(state.next_event_sequence).unwrap();
        state.next_event_sequence += 1;
        state.events.push_back(EnginePortEvent::new(sequence, kind));
    }

    fn push_at(&self, sequence: u64, kind: EnginePortEventKind) {
        self.0.borrow_mut().events.push_back(EnginePortEvent::new(
            EnginePortSequence::new(sequence).unwrap(),
            kind,
        ));
    }

    fn register_frame(
        &self,
        navigation: NavigationId,
        lease: u64,
        rgba: [u8; 4],
    ) -> (
        EnginePortFrameLeaseId,
        EngineFrameDescriptor,
        EngineDocumentVersion,
    ) {
        let lease = EnginePortFrameLeaseId::new(lease).unwrap();
        let document_version = initial_document_version(navigation);
        self.0.borrow_mut().frames.insert(
            lease,
            FakeFrame {
                navigation,
                width: 1,
                height: 1,
                pixels: rgba.to_vec(),
                document_version: Some(document_version),
            },
        );
        (
            lease,
            EngineFrameDescriptor::rgba8(1, 1, 4).unwrap(),
            document_version,
        )
    }
}

struct FakePort {
    state: Rc<RefCell<FakeState>>,
}

impl FakePort {
    fn pair() -> (Self, FakeHandle) {
        let state = Rc::new(RefCell::new(FakeState::default()));
        (
            Self {
                state: Rc::clone(&state),
            },
            FakeHandle(state),
        )
    }
}

impl EnginePort for FakePort {
    fn navigate(
        &mut self,
        context: TopLevelContextId,
        request: NavigationRequest,
    ) -> Result<NavigationId, EnginePortError> {
        let mut state = self.state.borrow_mut();
        state
            .navigations
            .push((context, request.url().to_owned().into_boxed_str()));
        state
            .navigation_capabilities
            .push(request.network_capability());
        if let Some(navigation) = state.navigation_override.take() {
            return Ok(navigation);
        }
        let generation = state
            .generations
            .get(&context)
            .copied()
            .map_or(
                Some(NavigationGeneration::INITIAL),
                NavigationGeneration::checked_next,
            )
            .ok_or(EnginePortError::Command(
                wild_buzzard_ui::CommandErrorKind::GenerationExhausted,
            ))?;
        state.generations.insert(context, generation);
        Ok(NavigationId::new(context, generation))
    }

    fn cancel_navigation(&mut self, navigation: NavigationId) -> Result<(), EnginePortError> {
        self.state.borrow_mut().cancellations.push(navigation);
        Ok(())
    }

    fn close_context(&mut self, navigation: NavigationId) -> Result<(), EnginePortError> {
        let mut state = self.state.borrow_mut();
        let call = state.close_calls.len() + 1;
        if state.close_failure_on == Some(call) {
            return Err(EnginePortError::Command(
                wild_buzzard_ui::CommandErrorKind::UnknownContext,
            ));
        }
        state.close_calls.push(navigation);
        Ok(())
    }

    fn poll_event(&mut self) -> Result<Option<EnginePortEvent>, EnginePortError> {
        let mut state = self.state.borrow_mut();
        assert!(!state.panic_on_poll, "injected fake-port panic");
        if let Some(status) = state.receiver_closed {
            return Err(EnginePortError::ReceiverClosed(status));
        }
        Ok(state.events.pop_front())
    }

    fn take_frame(
        &mut self,
        navigation: NavigationId,
        lease: EnginePortFrameLeaseId,
    ) -> Result<EngineFrameLease, EnginePortError> {
        let mut state = self.state.borrow_mut();
        if state.stale_frames.remove(&lease) {
            state.frames.remove(&lease);
            return Err(EnginePortError::FrameLease(FrameLeaseError::Stale));
        }
        let Some(frame) = state.frames.remove(&lease) else {
            return Err(EnginePortError::FrameLease(FrameLeaseError::Unknown));
        };
        if frame.navigation != navigation {
            let bound = frame.navigation;
            state.frames.insert(lease, frame);
            return Err(EnginePortError::LeaseNavigationMismatch {
                expected: navigation,
                bound,
            });
        }
        state.frame_transfers.push((navigation, lease));
        EngineFrameLease::from_owned_rgba8(
            navigation,
            lease,
            frame.width,
            frame.height,
            frame.pixels,
            frame.document_version,
        )
        .map_err(|_| EnginePortError::ContractViolation("fake frame construction failed"))
    }

    fn take_mutation_result(
        &mut self,
        _navigation: NavigationId,
        _lease: EnginePortMutationLeaseId,
    ) -> Result<wild_buzzard_ui::EngineMutationResultLease, EnginePortError> {
        Err(EnginePortError::MutationLease(
            MutationResultLeaseError::Unknown,
        ))
    }

    fn shutdown(&mut self) -> EnginePortShutdownStatus {
        self.state.borrow_mut().shutdown_calls += 1;
        clean_shutdown()
    }
}

struct PanicShutdownDropPort {
    shutdowns: Rc<Cell<usize>>,
    drops: Rc<Cell<usize>>,
}

impl EnginePort for PanicShutdownDropPort {
    fn navigate(
        &mut self,
        _context: TopLevelContextId,
        _request: NavigationRequest,
    ) -> Result<NavigationId, EnginePortError> {
        unreachable!("shutdown test never navigates")
    }

    fn cancel_navigation(&mut self, _navigation: NavigationId) -> Result<(), EnginePortError> {
        unreachable!("shutdown test never cancels")
    }

    fn close_context(&mut self, _navigation: NavigationId) -> Result<(), EnginePortError> {
        unreachable!("shutdown test never closes a context")
    }

    fn poll_event(&mut self) -> Result<Option<EnginePortEvent>, EnginePortError> {
        unreachable!("shutdown test never polls")
    }

    fn take_frame(
        &mut self,
        _navigation: NavigationId,
        _lease: EnginePortFrameLeaseId,
    ) -> Result<EngineFrameLease, EnginePortError> {
        unreachable!("shutdown test never transfers a frame")
    }

    fn take_mutation_result(
        &mut self,
        _navigation: NavigationId,
        _lease: EnginePortMutationLeaseId,
    ) -> Result<wild_buzzard_ui::EngineMutationResultLease, EnginePortError> {
        unreachable!("shutdown test never transfers a result")
    }

    fn shutdown(&mut self) -> EnginePortShutdownStatus {
        self.shutdowns.set(self.shutdowns.get() + 1);
        panic!("injected shutdown panic")
    }
}

impl Drop for PanicShutdownDropPort {
    fn drop(&mut self) {
        self.drops.set(self.drops.get() + 1);
        panic!("injected engine-owner drop panic")
    }
}

fn limits(max_closing: usize, max_history: usize, pump: usize) -> SessionLimits {
    SessionLimits::new(
        8,
        16,
        32,
        max_closing,
        max_history,
        64 * 1024,
        64 * 1024,
        16 * 1024,
        pump,
    )
    .unwrap()
}

fn initial_ids() -> (BrowserWindowId, BrowserTabId) {
    (
        BrowserWindowId::new(1).unwrap(),
        BrowserTabId::new(1).unwrap(),
    )
}

fn queued(outcome: BrowserCommandOutcome) -> NavigationId {
    match outcome {
        BrowserCommandOutcome::NavigationQueued { navigation, .. } => navigation,
        other => panic!("expected queued navigation, received {other:?}"),
    }
}

fn initial_document_version(navigation: NavigationId) -> EngineDocumentVersion {
    let document = navigation
        .context()
        .get()
        .checked_mul(10_000)
        .and_then(|base| base.checked_add(navigation.generation().get()))
        .expect("test document identity has room");
    EngineDocumentVersion::new(document, 0)
}

fn push_started_committed(handle: &FakeHandle, navigation: NavigationId) {
    handle.push(EnginePortEventKind::NavigationStarted { navigation });
    handle.push(EnginePortEventKind::NavigationCommitted {
        navigation,
        http_status: 200,
    });
}

fn navigate_and_publish_ready(
    session: &mut BrowserSession<FakePort>,
    handle: &FakeHandle,
    tab: BrowserTabId,
    address: &str,
    lease: u64,
    rgba: [u8; 4],
) -> NavigationId {
    let navigation = queued(session.navigate_new(tab, address).unwrap());
    let (lease, descriptor, document_version) = handle.register_frame(navigation, lease, rgba);
    push_started_committed(handle, navigation);
    handle.push(EnginePortEventKind::FrameReady {
        navigation,
        lease,
        descriptor,
        document_version: Some(document_version),
    });
    for _ in 0..3 {
        assert_eq!(
            session.poll_engine_once().unwrap(),
            EnginePumpOutcome::Applied
        );
    }
    navigation
}

#[test]
fn general_web_session_preserves_authority_across_address_history_and_reload() {
    let (port, handle) = FakePort::pair();
    let mut session = BrowserSession::new_with_navigation_mode(
        port,
        limits(8, 50, 8),
        BrowserNavigationMode::GeneralWeb,
    )
    .unwrap();
    let (_, tab) = initial_ids();

    session.navigate_new(tab, "https://first.example/").unwrap();
    session
        .navigate_new(tab, "https://second.example/")
        .unwrap();
    session.dispatch(BrowserCommand::Back { tab }).unwrap();
    session.dispatch(BrowserCommand::Forward { tab }).unwrap();
    session.reload(tab).unwrap();
    session
        .address_mut(tab)
        .unwrap()
        .set_text("https://submitted.example/")
        .unwrap();
    session.submit_address(tab).unwrap();

    assert_eq!(session.navigation_mode(), BrowserNavigationMode::GeneralWeb);
    assert_eq!(
        handle.0.borrow().navigation_capabilities,
        vec![NavigationNetworkCapability::GeneralWeb; 6]
    );
}

#[test]
fn tab_and_context_identities_never_reuse_and_active_close_chooses_successor() {
    let (port, _handle) = FakePort::pair();
    let mut session = BrowserSession::new(port, limits(8, 50, 8)).unwrap();
    let (window, first) = initial_ids();
    let second = match session.open_tab(window).unwrap() {
        BrowserCommandOutcome::TabOpened { tab, .. } => tab,
        other => panic!("unexpected {other:?}"),
    };
    let third = match session.open_tab(window).unwrap() {
        BrowserCommandOutcome::TabOpened { tab, .. } => tab,
        other => panic!("unexpected {other:?}"),
    };
    session.activate_tab(second).unwrap();
    session.close_tab(second).unwrap();
    assert_eq!(session.window_snapshot(window).unwrap().active, third);

    let fourth = match session.open_tab(window).unwrap() {
        BrowserCommandOutcome::TabOpened { tab, .. } => tab,
        other => panic!("unexpected {other:?}"),
    };
    assert!(fourth > third);
    assert!(
        session.tab_snapshot(fourth).unwrap().context
            > session.tab_snapshot(first).unwrap().context
    );
}

#[test]
fn address_draft_selection_and_focus_are_retained_per_tab() {
    let (port, _handle) = FakePort::pair();
    let mut session = BrowserSession::new(port, limits(8, 50, 8)).unwrap();
    let (window, first) = initial_ids();
    session
        .address_mut(first)
        .unwrap()
        .set_text("first🦅")
        .unwrap();
    let second = match session.open_tab(window).unwrap() {
        BrowserCommandOutcome::TabOpened { tab, .. } => tab,
        other => panic!("unexpected {other:?}"),
    };
    session
        .address_mut(second)
        .unwrap()
        .set_text("second")
        .unwrap();
    session.focus_content(second).unwrap();

    session.activate_tab(first).unwrap();
    session.focus_address(window).unwrap();
    let first_state = session.tab_snapshot(first).unwrap();
    assert_eq!(first_state.address.as_ref(), "first🦅");
    assert!(first_state.address_focused);
    assert_eq!(
        first_state.address_selection,
        AddressSelection::new("first🦅", 0, "first🦅".len()).unwrap()
    );
    session.activate_tab(second).unwrap();
    let second_state = session.tab_snapshot(second).unwrap();
    assert_eq!(second_state.address.as_ref(), "second");
    assert!(!second_state.address_focused);
}

#[test]
fn closing_the_last_tab_closes_the_session_and_shutdown_is_repeatable() {
    let (port, handle) = FakePort::pair();
    let mut session = BrowserSession::new(port, limits(8, 50, 8)).unwrap();
    let (_, tab) = initial_ids();
    assert_eq!(
        session.close_tab(tab).unwrap(),
        BrowserCommandOutcome::SessionClosed {
            status: clean_shutdown(),
        }
    );
    assert_eq!(session.window_count(), 0);
    assert_eq!(session.tab_count(), 0);
    assert_eq!(session.shutdown(), clean_shutdown());
    assert_eq!(handle.0.borrow().shutdown_calls, 1);
}

#[test]
fn history_actions_allocate_exact_generations_and_truncate_forward_entries() {
    let (port, _handle) = FakePort::pair();
    let mut session = BrowserSession::new(port, limits(8, 3, 8)).unwrap();
    let (_, tab) = initial_ids();
    let first = queued(session.navigate_new(tab, "https://a.invalid/").unwrap());
    let second = queued(session.navigate_new(tab, "https://b.invalid/").unwrap());
    let third = queued(session.navigate_new(tab, "https://c.invalid/").unwrap());
    assert_eq!(first.generation().get(), 1);
    assert_eq!(second.generation().get(), 2);
    assert_eq!(third.generation().get(), 3);

    let back = queued(
        session
            .dispatch(wild_buzzard_ui::BrowserCommand::Back { tab })
            .unwrap(),
    );
    assert_eq!(back.generation().get(), 4);
    let replacement = queued(session.navigate_new(tab, "https://d.invalid/🦅").unwrap());
    assert_eq!(replacement.generation().get(), 5);
    assert_eq!(
        session.history_addresses(tab).unwrap(),
        vec![
            "https://a.invalid/",
            "https://b.invalid/",
            "https://d.invalid/🦅"
        ]
    );
    assert_eq!(session.tab_snapshot(tab).unwrap().history_index, Some(2));
}

#[test]
fn history_and_address_limits_reject_before_navigation_admission() {
    let (port, handle) = FakePort::pair();
    let limits = SessionLimits::new(2, 4, 4, 4, 2, 12, 64, 8, 4).unwrap();
    let mut session = BrowserSession::new(port, limits).unwrap();
    let (_, tab) = initial_ids();
    session.navigate_new(tab, "12345678").unwrap();
    assert_eq!(
        session.navigate_new(tab, "12345"),
        Err(SessionError::HistoryByteLimit { maximum: 12 })
    );
    assert_eq!(handle.0.borrow().navigations.len(), 1);
    assert!(matches!(
        session.navigate_new(tab, "🦅🦅x"),
        Err(SessionError::Address(_))
    ));
    assert_eq!(handle.0.borrow().navigations.len(), 1);
    assert_eq!(session.history_addresses(tab).unwrap(), vec!["12345678"]);
}

#[test]
fn navigation_ledger_saturation_rejects_transactionally_before_engine_admission() {
    let (port, handle) = FakePort::pair();
    let limits = SessionLimits::new(2, 4, 64, 4, 4, 64 * 1024, 64 * 1024, 16 * 1024, 4).unwrap();
    let mut session = BrowserSession::new(port, limits).unwrap();
    let (_, tab) = initial_ids();
    for _ in 0..4_096 {
        let _ = session.navigate_new(tab, "x").unwrap();
    }
    let admitted = handle.0.borrow().navigations.len();
    assert_eq!(admitted, 4_096);
    assert_eq!(session.navigation_ledger_entries(), 4_096);
    assert_eq!(
        session.tab_snapshot(tab).unwrap().navigation_ledger_len,
        4_096
    );
    assert_eq!(
        session.navigate_new(tab, "y"),
        Err(SessionError::NavigationLedgerLimit { maximum: 4_096 })
    );
    assert_eq!(handle.0.borrow().navigations.len(), admitted);
    assert_eq!(session.navigation_ledger_entries(), 4_096);
}

#[test]
fn terminal_entries_prune_session_wide_and_any_late_event_fails_closed() {
    let (port, handle) = FakePort::pair();
    let mut session = BrowserSession::new(port, limits(8, 50, 8)).unwrap();
    let (window, first) = initial_ids();
    let second = match session.open_tab(window).unwrap() {
        BrowserCommandOutcome::TabOpened { tab, .. } => tab,
        other => panic!("unexpected {other:?}"),
    };
    let first_navigation = queued(session.navigate_new(first, "a").unwrap());
    let second_navigation = queued(session.navigate_new(second, "b").unwrap());
    for navigation in [first_navigation, second_navigation] {
        handle.push(EnginePortEventKind::NavigationStarted { navigation });
        handle.push(EnginePortEventKind::NavigationFailed {
            navigation,
            failure: ExecutionFailure::new(ExecutionFailureKind::Network, NavigationStage::Fetch),
        });
    }
    for _ in 0..4 {
        assert_eq!(
            session.poll_engine_once().unwrap(),
            EnginePumpOutcome::Applied
        );
    }
    assert_eq!(session.navigation_ledger_entries(), 2);

    let replacement = queued(session.navigate_new(first, "c").unwrap());
    assert_eq!(session.navigation_ledger_entries(), 1);
    assert_eq!(
        session.tab_snapshot(first).unwrap().navigation_ledger_len,
        1
    );
    assert_eq!(
        session.tab_snapshot(second).unwrap().navigation_ledger_len,
        0
    );
    assert_eq!(
        session.tab_snapshot(first).unwrap().latest_navigation,
        Some(replacement)
    );

    handle.push(EnginePortEventKind::NavigationStarted {
        navigation: second_navigation,
    });
    assert!(matches!(
        session.poll_engine_once(),
        Err(SessionError::Terminal(
            SessionFailure::EngineContract { .. }
        ))
    ));
    assert_eq!(session.tab_count(), 0);
}

#[test]
fn close_window_preflights_tombstone_saturation_without_engine_side_effects() {
    let (port, handle) = FakePort::pair();
    let mut session = BrowserSession::new(port, limits(1, 50, 8)).unwrap();
    let (window, first) = initial_ids();
    let second = match session.open_tab(window).unwrap() {
        BrowserCommandOutcome::TabOpened { tab, .. } => tab,
        other => panic!("unexpected {other:?}"),
    };
    session.navigate_new(first, "https://a.invalid/").unwrap();
    session.navigate_new(second, "https://b.invalid/").unwrap();

    assert_eq!(
        session.close_window(window),
        Err(SessionError::ClosingContextLimit { maximum: 1 })
    );
    assert_eq!(session.tab_count(), 2);
    assert!(handle.0.borrow().close_calls.is_empty());
}

#[test]
fn partial_window_close_failure_is_terminal_and_releases_all_product_state() {
    let (port, handle) = FakePort::pair();
    let mut session = BrowserSession::new(port, limits(8, 50, 8)).unwrap();
    let (window, first) = initial_ids();
    let second = match session.open_tab(window).unwrap() {
        BrowserCommandOutcome::TabOpened { tab, .. } => tab,
        other => panic!("unexpected {other:?}"),
    };
    session.navigate_new(first, "https://a.invalid/").unwrap();
    session.navigate_new(second, "https://b.invalid/").unwrap();
    handle.0.borrow_mut().close_failure_on = Some(2);

    assert!(matches!(
        session.close_window(window),
        Err(SessionError::Terminal(
            SessionFailure::EngineContract { .. }
        ))
    ));
    assert!(matches!(
        session.lifecycle(),
        SessionLifecycle::Failed { .. }
    ));
    assert_eq!(session.window_count(), 0);
    assert_eq!(session.tab_count(), 0);
    assert_eq!(handle.0.borrow().shutdown_calls, 1);
}

#[test]
fn bounded_pump_reports_exact_count_and_possible_remaining_work() {
    let (port, handle) = FakePort::pair();
    let mut session = BrowserSession::new(port, limits(8, 50, 2)).unwrap();
    let (_, tab) = initial_ids();
    let navigation = queued(session.navigate_new(tab, "https://a.invalid/").unwrap());
    let next_navigation = queued(session.navigate_new(tab, "https://b.invalid/").unwrap());
    handle.push(EnginePortEventKind::NavigationStarted { navigation });
    handle.push(EnginePortEventKind::NavigationFailed {
        navigation,
        failure: ExecutionFailure::new(ExecutionFailureKind::Network, NavigationStage::Fetch),
    });
    handle.push(EnginePortEventKind::NavigationStarted {
        navigation: next_navigation,
    });

    assert_eq!(
        session.pump_engine(20).unwrap(),
        EnginePumpOutcome::Batch {
            processed: 2,
            more_may_remain: true,
        }
    );
    assert_eq!(
        session.pump_engine(20).unwrap(),
        EnginePumpOutcome::Batch {
            processed: 1,
            more_may_remain: false,
        }
    );
}

#[test]
fn exact_navigation_phase_paths_are_enforced() {
    // Requested -> Cancelled is valid.
    let (port, handle) = FakePort::pair();
    let mut session = BrowserSession::new(port, limits(8, 50, 8)).unwrap();
    let (_, tab) = initial_ids();
    let cancelled = queued(session.navigate_new(tab, "cancel").unwrap());
    handle.push(EnginePortEventKind::NavigationCancelled {
        navigation: cancelled,
    });
    assert_eq!(
        session.poll_engine_once().unwrap(),
        EnginePumpOutcome::Applied
    );
    assert_eq!(
        session.tab_snapshot(tab).unwrap().latest_navigation_phase,
        Some(NavigationPhase::Cancelled)
    );

    // Requested -> Started -> Failed is valid.
    let failed = queued(session.navigate_new(tab, "fail").unwrap());
    handle.push(EnginePortEventKind::NavigationStarted { navigation: failed });
    handle.push(EnginePortEventKind::NavigationFailed {
        navigation: failed,
        failure: ExecutionFailure::new(ExecutionFailureKind::Network, NavigationStage::Fetch),
    });
    assert_eq!(
        session.poll_engine_once().unwrap(),
        EnginePumpOutcome::Applied
    );
    assert_eq!(
        session.poll_engine_once().unwrap(),
        EnginePumpOutcome::Applied
    );
    assert_eq!(
        session.tab_snapshot(tab).unwrap().latest_navigation_phase,
        Some(NavigationPhase::Failed)
    );

    // Requested -> Started -> Committed -> Ready is valid.
    let ready = queued(session.navigate_new(tab, "ready").unwrap());
    let (lease, descriptor, document_version) = handle.register_frame(ready, 80, [8, 0, 0, 255]);
    push_started_committed(&handle, ready);
    handle.push(EnginePortEventKind::FrameReady {
        navigation: ready,
        lease,
        descriptor,
        document_version: Some(document_version),
    });
    for _ in 0..3 {
        assert_eq!(
            session.poll_engine_once().unwrap(),
            EnginePumpOutcome::Applied
        );
    }
    assert_eq!(
        session.tab_snapshot(tab).unwrap().latest_navigation_phase,
        Some(NavigationPhase::Ready)
    );
}

#[test]
fn older_committed_frame_promotes_while_a_newer_navigation_is_pending() {
    // B may promote over A even though newer C is already committed. C has
    // not published yet, so B is the next visible page in event order.
    let (port, handle) = FakePort::pair();
    let mut session = BrowserSession::new(port, limits(8, 50, 8)).unwrap();
    let (_, tab) = initial_ids();
    let navigation_a = navigate_and_publish_ready(
        &mut session,
        &handle,
        tab,
        "monotonic-a",
        180,
        [18, 0, 0, 255],
    );
    let navigation_b = queued(session.navigate_new(tab, "monotonic-b").unwrap());
    let navigation_c = queued(session.navigate_new(tab, "monotonic-c").unwrap());
    let (lease_b, descriptor_b, document_b) =
        handle.register_frame(navigation_b, 181, [18, 1, 0, 255]);
    let (lease_c, descriptor_c, document_c) =
        handle.register_frame(navigation_c, 182, [18, 2, 0, 255]);
    push_started_committed(&handle, navigation_b);
    push_started_committed(&handle, navigation_c);
    handle.push(EnginePortEventKind::FrameReady {
        navigation: navigation_b,
        lease: lease_b,
        descriptor: descriptor_b,
        document_version: Some(document_b),
    });
    handle.push(EnginePortEventKind::FrameReady {
        navigation: navigation_c,
        lease: lease_c,
        descriptor: descriptor_c,
        document_version: Some(document_c),
    });

    for _ in 0..4 {
        assert_eq!(
            session.poll_engine_once().unwrap(),
            EnginePumpOutcome::Applied
        );
    }
    let pending = session.tab_snapshot(tab).unwrap();
    assert_eq!(pending.live_navigation, Some(navigation_a));
    assert_eq!(pending.latest_navigation, Some(navigation_c));
    assert_eq!(
        pending.latest_navigation_phase,
        Some(NavigationPhase::Committed)
    );
    assert_eq!(pending.navigation_ledger_len, 3);

    assert_eq!(
        session.poll_engine_once().unwrap(),
        EnginePumpOutcome::Applied
    );
    let published_b = session.tab_snapshot(tab).unwrap();
    assert_eq!(published_b.live_navigation, Some(navigation_b));
    assert_eq!(published_b.navigation_ledger_len, 2);
    assert_eq!(
        session.frame(tab).unwrap().unwrap().navigation(),
        navigation_b
    );

    assert_eq!(
        session.poll_engine_once().unwrap(),
        EnginePumpOutcome::Applied
    );
    let published_c = session.tab_snapshot(tab).unwrap();
    assert_eq!(published_c.live_navigation, Some(navigation_c));
    assert_eq!(published_c.navigation_ledger_len, 1);
    assert_eq!(
        session.frame(tab).unwrap().unwrap().navigation(),
        navigation_c
    );
}

#[test]
fn older_committed_frame_cannot_roll_back_a_newer_ready_navigation() {
    // In the hostile order, C publishes before B. B remains Committed in the
    // phase ledger but can no longer roll the retained live page backward.
    let (port, handle) = FakePort::pair();
    let mut session = BrowserSession::new(port, limits(8, 50, 8)).unwrap();
    let (_, tab) = initial_ids();
    let _navigation_a = navigate_and_publish_ready(
        &mut session,
        &handle,
        tab,
        "hostile-a",
        190,
        [19, 0, 0, 255],
    );
    let navigation_b = queued(session.navigate_new(tab, "hostile-b").unwrap());
    let navigation_c = queued(session.navigate_new(tab, "hostile-c").unwrap());
    let (lease_b, descriptor_b, document_b) =
        handle.register_frame(navigation_b, 191, [19, 1, 0, 255]);
    let (lease_c, descriptor_c, document_c) =
        handle.register_frame(navigation_c, 192, [19, 2, 0, 255]);
    push_started_committed(&handle, navigation_b);
    push_started_committed(&handle, navigation_c);
    handle.push(EnginePortEventKind::FrameReady {
        navigation: navigation_c,
        lease: lease_c,
        descriptor: descriptor_c,
        document_version: Some(document_c),
    });
    handle.push(EnginePortEventKind::FrameReady {
        navigation: navigation_b,
        lease: lease_b,
        descriptor: descriptor_b,
        document_version: Some(document_b),
    });

    for _ in 0..5 {
        assert_eq!(
            session.poll_engine_once().unwrap(),
            EnginePumpOutcome::Applied
        );
    }
    let before_rollback = session.tab_snapshot(tab).unwrap();
    assert_eq!(before_rollback.live_navigation, Some(navigation_c));
    assert_eq!(before_rollback.navigation_ledger_len, 2);
    assert_eq!(
        before_rollback.engine_document_navigation,
        Some(navigation_c)
    );
    assert_eq!(
        session.frame(tab).unwrap().unwrap().navigation(),
        navigation_c
    );

    assert!(matches!(
        session.poll_engine_once(),
        Err(SessionError::Terminal(
            SessionFailure::EngineContract { .. }
        ))
    ));
    assert_eq!(session.tab_count(), 0);
    assert_eq!(session.window_count(), 0);
    let state = handle.0.borrow();
    assert!(!state.frame_transfers.contains(&(navigation_b, lease_b)));
    assert_eq!(state.shutdown_calls, 1);
}

#[test]
fn duplicate_out_of_order_and_after_terminal_navigation_events_fail_closed() {
    fn assert_terminal_after(build: impl FnOnce(NavigationId) -> Vec<EnginePortEventKind>) {
        let (port, handle) = FakePort::pair();
        let mut session = BrowserSession::new(port, limits(8, 50, 8)).unwrap();
        let (_, tab) = initial_ids();
        let navigation = queued(session.navigate_new(tab, "phase").unwrap());
        for kind in build(navigation) {
            handle.push(kind);
        }
        loop {
            match session.poll_engine_once() {
                Ok(EnginePumpOutcome::Applied) => {}
                Err(SessionError::Terminal(SessionFailure::EngineContract { .. })) => break,
                other => panic!("expected exact phase contract failure, got {other:?}"),
            }
        }
        assert_eq!(session.tab_count(), 0);
    }

    assert_terminal_after(|navigation| {
        vec![EnginePortEventKind::NavigationCommitted {
            navigation,
            http_status: 200,
        }]
    });
    assert_terminal_after(|navigation| {
        vec![EnginePortEventKind::NavigationFailed {
            navigation,
            failure: ExecutionFailure::new(ExecutionFailureKind::Network, NavigationStage::Fetch),
        }]
    });
    assert_terminal_after(|navigation| {
        vec![
            EnginePortEventKind::NavigationStarted { navigation },
            EnginePortEventKind::NavigationStarted { navigation },
        ]
    });
    assert_terminal_after(|navigation| {
        vec![
            EnginePortEventKind::NavigationStarted { navigation },
            EnginePortEventKind::NavigationCommitted {
                navigation,
                http_status: 200,
            },
            EnginePortEventKind::NavigationCancelled { navigation },
        ]
    });
    assert_terminal_after(|navigation| {
        vec![
            EnginePortEventKind::NavigationStarted { navigation },
            EnginePortEventKind::NavigationCommitted {
                navigation,
                http_status: 200,
            },
            EnginePortEventKind::NavigationFailed {
                navigation,
                failure: ExecutionFailure::new(
                    ExecutionFailureKind::Network,
                    NavigationStage::Fetch,
                ),
            },
        ]
    });
}

#[test]
fn frame_is_accepted_only_from_committed_and_ready_is_terminal() {
    let (port, handle) = FakePort::pair();
    let mut session = BrowserSession::new(port, limits(8, 50, 8)).unwrap();
    let (_, tab) = initial_ids();
    let navigation = queued(session.navigate_new(tab, "frame-too-early").unwrap());
    let (lease, descriptor, document_version) =
        handle.register_frame(navigation, 81, [8, 1, 0, 255]);
    handle.push(EnginePortEventKind::NavigationStarted { navigation });
    handle.push(EnginePortEventKind::FrameReady {
        navigation,
        lease,
        descriptor,
        document_version: Some(document_version),
    });
    assert_eq!(
        session.poll_engine_once().unwrap(),
        EnginePumpOutcome::Applied
    );
    assert!(matches!(
        session.poll_engine_once(),
        Err(SessionError::Terminal(
            SessionFailure::EngineContract { .. }
        ))
    ));

    let (port, handle) = FakePort::pair();
    let mut session = BrowserSession::new(port, limits(8, 50, 8)).unwrap();
    let (_, tab) = initial_ids();
    let navigation = queued(session.navigate_new(tab, "after-ready").unwrap());
    let (lease, descriptor, document_version) =
        handle.register_frame(navigation, 82, [8, 2, 0, 255]);
    push_started_committed(&handle, navigation);
    handle.push(EnginePortEventKind::FrameReady {
        navigation,
        lease,
        descriptor,
        document_version: Some(document_version),
    });
    handle.push(EnginePortEventKind::NavigationCancelled { navigation });
    for _ in 0..3 {
        assert_eq!(
            session.poll_engine_once().unwrap(),
            EnginePumpOutcome::Applied
        );
    }
    assert!(matches!(
        session.poll_engine_once(),
        Err(SessionError::Terminal(
            SessionFailure::EngineContract { .. }
        ))
    ));
}

#[test]
fn stale_frame_drain_cannot_consume_the_newer_exact_lease() {
    let (port, handle) = FakePort::pair();
    let mut session = BrowserSession::new(port, limits(8, 50, 8)).unwrap();
    let (_, tab) = initial_ids();
    let stale = queued(session.navigate_new(tab, "https://old.invalid/").unwrap());
    let current = queued(session.navigate_new(tab, "https://new.invalid/").unwrap());
    let (stale_lease, stale_descriptor, stale_version) =
        handle.register_frame(stale, 1, [1, 2, 3, 4]);
    let (current_lease, current_descriptor, current_version) =
        handle.register_frame(current, 2, [9, 8, 7, 6]);
    handle.0.borrow_mut().stale_frames.insert(stale_lease);
    push_started_committed(&handle, stale);
    handle.push(EnginePortEventKind::FrameReady {
        navigation: stale,
        lease: stale_lease,
        descriptor: stale_descriptor,
        document_version: Some(stale_version),
    });
    push_started_committed(&handle, current);
    handle.push(EnginePortEventKind::FrameReady {
        navigation: current,
        lease: current_lease,
        descriptor: current_descriptor,
        document_version: Some(current_version),
    });

    assert_eq!(
        session.poll_engine_once().unwrap(),
        EnginePumpOutcome::Applied
    );
    assert_eq!(
        session.poll_engine_once().unwrap(),
        EnginePumpOutcome::Applied
    );
    assert_eq!(
        session.poll_engine_once().unwrap(),
        EnginePumpOutcome::StaleSuppressed
    );
    let stale_snapshot = session.tab_snapshot(tab).unwrap();
    assert_eq!(stale_snapshot.live_navigation, Some(stale));
    assert_eq!(stale_snapshot.engine_live_version, Some(stale_version));
    assert!(session.frame(tab).unwrap().is_none());
    assert_eq!(
        session.poll_engine_once().unwrap(),
        EnginePumpOutcome::Applied
    );
    assert_eq!(
        session.poll_engine_once().unwrap(),
        EnginePumpOutcome::Applied
    );
    assert_eq!(
        session.poll_engine_once().unwrap(),
        EnginePumpOutcome::Applied
    );
    assert_eq!(
        session.frame(tab).unwrap().unwrap().rgba8_pixels(),
        Some(&[9, 8, 7, 6][..])
    );
    assert_eq!(
        handle.0.borrow().frame_transfers,
        vec![(current, current_lease)]
    );
}

#[test]
fn stale_initial_frame_anchors_document_until_later_navigation_publication() {
    let (port, handle) = FakePort::pair();
    let mut session = BrowserSession::new(port, limits(8, 50, 8)).unwrap();
    let (_, tab) = initial_ids();
    let old_navigation = queued(
        session
            .navigate_new(tab, "https://rerender.invalid/")
            .unwrap(),
    );
    let new_navigation = queued(
        session
            .navigate_new(tab, "https://replacement.invalid/")
            .unwrap(),
    );
    let (old_lease, old_descriptor, old_version) =
        handle.register_frame(old_navigation, 1, [1, 1, 1, 255]);
    let (new_lease, new_descriptor, new_version) =
        handle.register_frame(new_navigation, 2, [2, 2, 2, 255]);
    handle.0.borrow_mut().stale_frames.insert(old_lease);
    push_started_committed(&handle, old_navigation);
    handle.push(EnginePortEventKind::FrameReady {
        navigation: old_navigation,
        lease: old_lease,
        descriptor: old_descriptor,
        document_version: Some(old_version),
    });
    push_started_committed(&handle, new_navigation);
    handle.push(EnginePortEventKind::FrameReady {
        navigation: new_navigation,
        lease: new_lease,
        descriptor: new_descriptor,
        document_version: Some(new_version),
    });

    assert_eq!(
        session.poll_engine_once().unwrap(),
        EnginePumpOutcome::Applied
    );
    assert_eq!(
        session.poll_engine_once().unwrap(),
        EnginePumpOutcome::Applied
    );
    assert_eq!(
        session.poll_engine_once().unwrap(),
        EnginePumpOutcome::StaleSuppressed
    );
    assert!(session.frame(tab).unwrap().is_none());
    assert_eq!(
        session.tab_snapshot(tab).unwrap().engine_live_version,
        Some(old_version)
    );
    assert_eq!(
        session.poll_engine_once().unwrap(),
        EnginePumpOutcome::Applied
    );
    assert_eq!(
        session.poll_engine_once().unwrap(),
        EnginePumpOutcome::Applied
    );
    assert_eq!(
        session.poll_engine_once().unwrap(),
        EnginePumpOutcome::Applied
    );
    assert_eq!(
        session.frame(tab).unwrap().unwrap().rgba8_pixels(),
        Some(&[2, 2, 2, 255][..])
    );
    assert!(matches!(session.lifecycle(), SessionLifecycle::Running));
}

#[test]
fn current_frame_over_session_budget_is_drained_exactly_without_replacing_content() {
    let (port, handle) = FakePort::pair();
    let tiny = SessionLimits::new(2, 4, 4, 4, 4, 1_024, 3, 1_024, 4).unwrap();
    let mut session = BrowserSession::new(port, tiny).unwrap();
    let (_, tab) = initial_ids();
    let navigation = queued(session.navigate_new(tab, "https://large.invalid/").unwrap());
    let (lease, descriptor, document_version) = handle.register_frame(navigation, 1, [1, 2, 3, 4]);
    push_started_committed(&handle, navigation);
    handle.push(EnginePortEventKind::FrameReady {
        navigation,
        lease,
        descriptor,
        document_version: Some(document_version),
    });

    assert_eq!(
        session.poll_engine_once().unwrap(),
        EnginePumpOutcome::Applied
    );
    assert_eq!(
        session.poll_engine_once().unwrap(),
        EnginePumpOutcome::Applied
    );
    assert_eq!(
        session.poll_engine_once().unwrap(),
        EnginePumpOutcome::FrameSuppressedByResourceLimit { navigation }
    );
    assert!(session.frame(tab).unwrap().is_none());
    assert_eq!(session.retained_frame_bytes(), 0);
    assert_eq!(handle.0.borrow().frame_transfers, vec![(navigation, lease)]);
}

#[test]
fn initial_frame_document_identity_is_nonzero_present_and_exactly_transferred() {
    for announced in [
        None,
        Some(EngineDocumentVersion::new(0, 0)),
        Some(EngineDocumentVersion::new(99_999, 0)),
    ] {
        let (port, handle) = FakePort::pair();
        let mut session = BrowserSession::new(port, limits(8, 50, 8)).unwrap();
        let (_, tab) = initial_ids();
        let navigation = queued(session.navigate_new(tab, "document-contract").unwrap());
        let (lease, descriptor, _) = handle.register_frame(navigation, 91, [9, 1, 0, 255]);
        push_started_committed(&handle, navigation);
        handle.push(EnginePortEventKind::FrameReady {
            navigation,
            lease,
            descriptor,
            document_version: announced,
        });
        assert_eq!(
            session.poll_engine_once().unwrap(),
            EnginePumpOutcome::Applied
        );
        assert_eq!(
            session.poll_engine_once().unwrap(),
            EnginePumpOutcome::Applied
        );
        assert!(matches!(
            session.poll_engine_once(),
            Err(SessionError::Terminal(
                SessionFailure::EngineContract { .. }
            ))
        ));
        assert_eq!(session.tab_count(), 0);
    }

    let (port, handle) = FakePort::pair();
    let mut session = BrowserSession::new(port, limits(8, 50, 8)).unwrap();
    let (_, tab) = initial_ids();
    let navigation = queued(
        session
            .navigate_new(tab, "missing-transfer-version")
            .unwrap(),
    );
    let (lease, descriptor, announced) = handle.register_frame(navigation, 92, [9, 2, 0, 255]);
    handle
        .0
        .borrow_mut()
        .frames
        .get_mut(&lease)
        .unwrap()
        .document_version = None;
    push_started_committed(&handle, navigation);
    handle.push(EnginePortEventKind::FrameReady {
        navigation,
        lease,
        descriptor,
        document_version: Some(announced),
    });
    assert_eq!(
        session.poll_engine_once().unwrap(),
        EnginePumpOutcome::Applied
    );
    assert_eq!(
        session.poll_engine_once().unwrap(),
        EnginePumpOutcome::Applied
    );
    assert!(matches!(
        session.poll_engine_once(),
        Err(SessionError::Terminal(
            SessionFailure::EngineContract { .. }
        ))
    ));
}

#[test]
fn cancellation_and_stale_completion_remain_bound_to_exact_generations() {
    let (port, handle) = FakePort::pair();
    let mut session = BrowserSession::new(port, limits(8, 50, 8)).unwrap();
    let (_, tab) = initial_ids();
    let old = queued(session.navigate_new(tab, "https://old.invalid/").unwrap());
    let new = queued(session.navigate_new(tab, "https://new.invalid/").unwrap());
    handle.push(EnginePortEventKind::NavigationCancelled { navigation: old });
    assert_eq!(
        session.poll_engine_once().unwrap(),
        EnginePumpOutcome::Applied
    );
    assert_eq!(session.tab_snapshot(tab).unwrap().navigation_ledger_len, 2);
    assert!(session.tab_snapshot(tab).unwrap().loading);
    assert!(matches!(
        session.stop(tab).unwrap(),
        BrowserCommandOutcome::StopRequested { navigation, .. } if navigation == new
    ));
    assert_eq!(handle.0.borrow().cancellations, vec![new]);
}

#[test]
fn live_context_close_is_terminal_but_retired_and_foreign_contexts_are_distinct() {
    let (port, handle) = FakePort::pair();
    let mut session = BrowserSession::new(port, limits(8, 50, 8)).unwrap();
    let (_, tab) = initial_ids();
    let live = queued(session.navigate_new(tab, "https://live.invalid/").unwrap());
    handle.push(EnginePortEventKind::ContextClosed { navigation: live });
    assert!(matches!(
        session.poll_engine_once(),
        Err(SessionError::Terminal(
            SessionFailure::EngineContract { .. }
        ))
    ));
    assert_eq!(session.tab_count(), 0);

    let (port, handle) = FakePort::pair();
    let mut session = BrowserSession::new(port, limits(8, 50, 8)).unwrap();
    let (_, tab) = initial_ids();
    let _second_window = match session.open_window().unwrap() {
        BrowserCommandOutcome::WindowOpened { window, .. } => window,
        other => panic!("unexpected {other:?}"),
    };
    let retired = queued(
        session
            .navigate_new(tab, "https://retired.invalid/")
            .unwrap(),
    );
    session.close_tab(tab).unwrap();
    handle.push(EnginePortEventKind::ContextClosed {
        navigation: retired,
    });
    assert!(matches!(
        session.poll_engine_once().unwrap(),
        EnginePumpOutcome::ContextCloseAcknowledged { .. }
    ));
    handle.push(EnginePortEventKind::ContextClosed {
        navigation: retired,
    });
    assert_eq!(
        session.poll_engine_once().unwrap(),
        EnginePumpOutcome::RetiredContextSuppressed {
            navigation: retired,
        }
    );
    let foreign = NavigationId::new(
        TopLevelContextId::new(999).unwrap(),
        NavigationGeneration::INITIAL,
    );
    handle.push(EnginePortEventKind::ContextClosed {
        navigation: foreign,
    });
    assert!(matches!(
        session.poll_engine_once(),
        Err(SessionError::Terminal(
            SessionFailure::EngineContract { .. }
        ))
    ));
    assert_eq!(session.window_count(), 0);
}

#[test]
fn context_close_acknowledgement_must_match_the_exact_tombstone_generation() {
    let (port, handle) = FakePort::pair();
    let mut session = BrowserSession::new(port, limits(8, 50, 8)).unwrap();
    let (_, tab) = initial_ids();
    session.open_window().unwrap();
    let navigation = queued(
        session
            .navigate_new(tab, "https://closing.invalid/")
            .unwrap(),
    );
    session.close_tab(tab).unwrap();
    let wrong_generation = NavigationId::new(
        navigation.context(),
        navigation.generation().checked_next().unwrap(),
    );
    handle.push(EnginePortEventKind::ContextClosed {
        navigation: wrong_generation,
    });

    assert!(matches!(
        session.poll_engine_once(),
        Err(SessionError::Terminal(
            SessionFailure::EngineContract { .. }
        ))
    ));
    assert_eq!(session.tab_count(), 0);
    assert_eq!(handle.0.borrow().shutdown_calls, 1);
}

#[test]
fn reordered_engine_sequence_and_hostile_navigation_identity_fail_closed() {
    let (port, handle) = FakePort::pair();
    let mut session = BrowserSession::new(port, limits(8, 50, 8)).unwrap();
    let (_, tab) = initial_ids();
    let navigation = queued(session.navigate_new(tab, "https://a.invalid/").unwrap());
    handle.push_at(2, EnginePortEventKind::NavigationStarted { navigation });
    assert_eq!(
        session.poll_engine_once(),
        Err(SessionError::Terminal(
            SessionFailure::EngineEventSequence {
                expected: 1,
                received: 2,
            }
        ))
    );
    assert!(matches!(
        session.lifecycle(),
        SessionLifecycle::Failed { .. }
    ));

    let (port, handle) = FakePort::pair();
    let mut session = BrowserSession::new(port, limits(8, 50, 8)).unwrap();
    let (_, tab) = initial_ids();
    handle.0.borrow_mut().navigation_override = Some(NavigationId::new(
        TopLevelContextId::new(88).unwrap(),
        NavigationGeneration::new(7).unwrap(),
    ));
    assert!(matches!(
        session.navigate_new(tab, "https://hostile.invalid/"),
        Err(SessionError::Terminal(
            SessionFailure::EngineContract { .. }
        ))
    ));
    assert_eq!(session.tab_count(), 0);
}

#[test]
fn engine_disconnect_and_panic_are_caught_and_cleanup_is_deterministic() {
    let (port, handle) = FakePort::pair();
    let mut session = BrowserSession::new(port, limits(8, 50, 8)).unwrap();
    handle.0.borrow_mut().receiver_closed = Some(clean_shutdown());
    assert!(matches!(
        session.poll_engine_once(),
        Err(SessionError::Terminal(
            SessionFailure::EngineDisconnected { .. }
        ))
    ));
    assert_eq!(handle.0.borrow().shutdown_calls, 1);
    let status = session.shutdown();
    assert_eq!(status, clean_shutdown());
    assert_eq!(handle.0.borrow().shutdown_calls, 1);

    let (port, handle) = FakePort::pair();
    let mut session = BrowserSession::new(port, limits(8, 50, 8)).unwrap();
    handle.0.borrow_mut().panic_on_poll = true;
    assert!(matches!(
        session.poll_engine_once(),
        Err(SessionError::Terminal(SessionFailure::EnginePanicked {
            operation: "poll event",
        }))
    ));
    assert_eq!(handle.0.borrow().shutdown_calls, 1);
}

#[test]
fn sequenced_shutdown_complete_is_terminal_and_releases_all_product_state() {
    let (port, handle) = FakePort::pair();
    let mut session = BrowserSession::new(port, limits(8, 50, 8)).unwrap();
    handle.push(EnginePortEventKind::ShutdownComplete {
        status: clean_shutdown(),
    });
    assert_eq!(
        session.poll_engine_once(),
        Err(SessionError::Terminal(SessionFailure::EngineStopped {
            status: clean_shutdown(),
        }))
    );
    assert_eq!(session.window_count(), 0);
    assert_eq!(session.tab_count(), 0);
    assert_eq!(session.retained_frame_bytes(), 0);
    assert_eq!(session.navigation_ledger_entries(), 0);
    assert_eq!(handle.0.borrow().shutdown_calls, 1);
}

#[test]
fn generic_shutdown_contains_shutdown_and_owner_drop_panics_exactly_once() {
    let shutdowns = Rc::new(Cell::new(0));
    let drops = Rc::new(Cell::new(0));
    let port = PanicShutdownDropPort {
        shutdowns: Rc::clone(&shutdowns),
        drops: Rc::clone(&drops),
    };
    let mut session = BrowserSession::new(port, limits(8, 50, 8)).unwrap();
    let status = session.shutdown();
    assert_eq!(status.reason(), EnginePortStopReason::PortPanicked);
    assert_eq!(status.executor(), EnginePortExecutorShutdown::Panicked);
    assert_eq!(shutdowns.get(), 1);
    assert_eq!(drops.get(), 1);
    assert_eq!(session.window_count(), 0);
    assert_eq!(session.tab_count(), 0);
    assert_eq!(session.shutdown(), status);
    assert_eq!(shutdowns.get(), 1);
    assert_eq!(drops.get(), 1);
}

fn ready_surface() -> (SurfaceId, LinuxWindowEvent) {
    let mut allocator = SurfaceIdAllocator::new(SurfaceNamespace::new(42).unwrap());
    let surface = allocator.allocate().unwrap();
    let descriptor = SurfaceDescriptor {
        id: surface,
        size: PhysicalSize::new(800, 600).unwrap(),
        scale: ScaleFactor::new(1.0).unwrap(),
        format: PixelFormat::Rgba8Srgb,
        role: SurfaceRole::Window,
    };
    (
        surface,
        LinuxWindowEvent::Ready {
            backend: LinuxBackend::Wayland,
            desired_surface: descriptor,
        },
    )
}

#[test]
fn native_surface_registry_rejects_cross_window_collision_and_duplicate_ready() {
    let (port, _handle) = FakePort::pair();
    let mut session = BrowserSession::new(port, limits(8, 50, 8)).unwrap();
    let (first_window, _) = initial_ids();
    let second_window = match session.open_window().unwrap() {
        BrowserCommandOutcome::WindowOpened { window, .. } => window,
        other => panic!("unexpected {other:?}"),
    };
    let (_, first_ready) = ready_surface();
    let (_, colliding_ready) = ready_surface();
    session
        .handle_linux_event(first_window, first_ready)
        .unwrap();
    assert!(matches!(
        session.handle_linux_event(second_window, colliding_ready),
        Err(SessionError::Terminal(
            SessionFailure::LinuxEventOrder { .. }
        ))
    ));

    let (port, _handle) = FakePort::pair();
    let mut session = BrowserSession::new(port, limits(8, 50, 8)).unwrap();
    let (window, _) = initial_ids();
    let (_, ready) = ready_surface();
    let (_, duplicate) = ready_surface();
    session.handle_linux_event(window, ready).unwrap();
    assert!(matches!(
        session.handle_linux_event(window, duplicate),
        Err(SessionError::Terminal(
            SessionFailure::LinuxEventOrder { .. }
        ))
    ));
}

#[test]
fn destroyed_surface_rejects_duplicate_and_every_later_nonterminal_event() {
    let (port, _handle) = FakePort::pair();
    let mut session = BrowserSession::new(port, limits(8, 50, 8)).unwrap();
    let (window, _) = initial_ids();
    let (surface, ready) = ready_surface();
    session.handle_linux_event(window, ready).unwrap();
    assert_eq!(
        session
            .handle_linux_event(window, LinuxWindowEvent::Destroyed { surface })
            .unwrap(),
        LinuxEventOutcome::NativeStateChanged
    );
    assert!(matches!(
        session.handle_linux_event(window, LinuxWindowEvent::Resumed),
        Err(SessionError::Terminal(
            SessionFailure::LinuxEventOrder { .. }
        ))
    ));

    let (port, _handle) = FakePort::pair();
    let mut session = BrowserSession::new(port, limits(8, 50, 8)).unwrap();
    let (window, _) = initial_ids();
    let (surface, ready) = ready_surface();
    session.handle_linux_event(window, ready).unwrap();
    session
        .handle_linux_event(window, LinuxWindowEvent::Destroyed { surface })
        .unwrap();
    assert!(matches!(
        session.handle_linux_event(window, LinuxWindowEvent::Destroyed { surface }),
        Err(SessionError::Terminal(
            SessionFailure::LinuxEventOrder { .. }
        ))
    ));
}

fn key_event(surface: SurfaceId, sequence: u64, physical: u32) -> InputEvent {
    InputEvent::Key(KeyEvent {
        metadata: InputMetadata {
            sequence: EventSequence::new(sequence).unwrap(),
            timestamp: EventTimestampMicros(sequence),
            seat: SeatId::new(1).unwrap(),
            device: InputDeviceId::new(1).unwrap(),
            surface,
            modifiers: Modifiers::default(),
        },
        physical_key: PhysicalKeyCode(physical),
        state: KeyState::Down,
        location: KeyLocation::Standard,
        repeat: false,
    })
}

fn pointer_move(surface: SurfaceId, sequence: u64, x: f64) -> InputEvent {
    InputEvent::Pointer(PointerEvent {
        metadata: InputMetadata {
            sequence: EventSequence::new(sequence).unwrap(),
            timestamp: EventTimestampMicros(sequence),
            seat: SeatId::new(1).unwrap(),
            device: InputDeviceId::new(2).unwrap(),
            surface,
            modifiers: Modifiers::default(),
        },
        pointer: PointerId::new(1).unwrap(),
        kind: PointerKind::Mouse,
        phase: PointerPhase::Move,
        position: LogicalPoint::new(x, 4.0).unwrap(),
        buttons: 0,
        pressure: None,
    })
}

#[test]
fn unrouted_linux_input_preserves_exact_event_target_and_origin() {
    let (port, _handle) = FakePort::pair();
    let mut session = BrowserSession::new(port, limits(8, 50, 8)).unwrap();
    let (window, tab) = initial_ids();
    let (surface, ready) = ready_surface();
    session.handle_linux_event(window, ready).unwrap();
    session.focus_content(tab).unwrap();
    let event = key_event(surface, 1, 9_999);

    assert_eq!(
        session
            .handle_linux_event(
                window,
                LinuxWindowEvent::Input {
                    event: event.clone(),
                    origin: InputOrigin::Synthetic,
                },
            )
            .unwrap(),
        LinuxEventOutcome::ContentInputUnrouted {
            window,
            tab,
            origin: InputOrigin::Synthetic,
            event,
        }
    );
}

#[test]
fn coalesced_pointer_moves_allow_initial_offset_and_sequence_gaps_but_reject_replay() {
    let (port, _handle) = FakePort::pair();
    let mut session = BrowserSession::new(port, limits(8, 50, 8)).unwrap();
    let (window, tab) = initial_ids();
    let (surface, ready) = ready_surface();
    session.handle_linux_event(window, ready).unwrap();
    session.focus_content(tab).unwrap();

    for (sequence, x) in [(4, 1.0), (9, 8.0)] {
        let event = pointer_move(surface, sequence, x);
        assert_eq!(
            session
                .handle_linux_event(
                    window,
                    LinuxWindowEvent::Input {
                        event: event.clone(),
                        origin: InputOrigin::Native,
                    },
                )
                .unwrap(),
            LinuxEventOutcome::ContentInputUnrouted {
                window,
                tab,
                origin: InputOrigin::Native,
                event,
            }
        );
    }

    assert_eq!(
        session.handle_linux_event(
            window,
            LinuxWindowEvent::Input {
                event: pointer_move(surface, 9, 9.0),
                origin: InputOrigin::Native,
            },
        ),
        Err(SessionError::Terminal(SessionFailure::LinuxInputSequence {
            window,
            previous: 9,
            received: 9,
        }))
    );
    assert_eq!(session.tab_count(), 0);
}
