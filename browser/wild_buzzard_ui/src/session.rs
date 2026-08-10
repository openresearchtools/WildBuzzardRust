use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::num::NonZeroU64;
use std::panic::{AssertUnwindSafe, catch_unwind};

use wild_buzzard_engine::{
    CommandErrorKind, ExecutionFailure, FrameLeaseError, MAX_NAVIGATION_URL_BYTES,
    MutationResultLeaseError, NavigationGeneration, NavigationId, NavigationRequest,
    NavigationRequestError, TopLevelContextId,
};
use wild_buzzard_linux::{
    BrowserNavigationIdentity, BrowserPageScene, InputOrigin, LinuxStopReason, LinuxWindowEvent,
    WebRenderWindowError,
};
use wild_buzzard_platform::{InputEvent, SurfaceId};

use crate::address::{AddressEditError, AddressEditState, AddressSelection};
use crate::engine::{
    EngineDocumentVersion, EngineFrameDescriptor, EngineFrameLease, EngineMutationResultLease,
    EnginePort, EnginePortError, EnginePortEvent, EnginePortEventKind, EnginePortFrameLeaseId,
    EnginePortMutationLeaseId, EnginePortShutdownStatus, EnginePortStopReason,
};
use crate::input::{LinuxInputAction, LinuxShortcut, map_linux_input};

const MAX_WINDOWS: usize = 64;
const MAX_TABS_PER_WINDOW: usize = 1_024;
const MAX_TOTAL_TABS: usize = 4_096;
const MAX_HISTORY_ENTRIES: usize = 4_096;
const MAX_TOTAL_HISTORY_BYTES: usize = 256 * 1024 * 1024;
const MAX_TOTAL_FRAME_BYTES: usize = 1024 * 1024 * 1024;
const MAX_ENGINE_EVENTS_PER_PUMP: usize = 4_096;
const MAX_NAVIGATION_LEDGER_ENTRIES: usize = 4_096;

macro_rules! browser_id {
    ($name:ident, $description:literal) => {
        #[doc = $description]
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(NonZeroU64);

        impl $name {
            /// Creates an identity for diagnostics and hostile contract tests.
            #[must_use]
            pub const fn new(raw: u64) -> Option<Self> {
                match NonZeroU64::new(raw) {
                    Some(raw) => Some(Self(raw)),
                    None => None,
                }
            }

            /// Returns the numeric representation.
            #[must_use]
            pub const fn get(self) -> u64 {
                self.0.get()
            }
        }
    };
}

browser_id!(
    BrowserWindowId,
    "Process-local, never-reused browser window identity."
);
browser_id!(
    BrowserTabId,
    "Process-local, never-reused browser tab identity."
);

/// Immutable resource policy for one browser session controller.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(clippy::struct_field_names)]
pub struct SessionLimits {
    max_windows: usize,
    max_tabs_per_window: usize,
    max_total_tabs: usize,
    max_closing_contexts: usize,
    max_history_entries: usize,
    max_total_history_bytes: usize,
    max_total_frame_bytes: usize,
    max_address_bytes: usize,
    max_engine_events_per_pump: usize,
}

impl Default for SessionLimits {
    fn default() -> Self {
        Self {
            max_windows: 16,
            max_tabs_per_window: 256,
            max_total_tabs: 1_024,
            max_closing_contexts: 1_024,
            max_history_entries: 50,
            max_total_history_bytes: 64 * 1024 * 1024,
            max_total_frame_bytes: 256 * 1024 * 1024,
            max_address_bytes: MAX_NAVIGATION_URL_BYTES,
            max_engine_events_per_pump: 256,
        }
    }
}

impl SessionLimits {
    /// Creates an explicitly bounded product policy.
    ///
    /// # Errors
    ///
    /// Returns [`SessionLimitsError`] when a limit is zero, exceeds its hard
    /// ceiling, or conflicts with an aggregate limit.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        max_windows: usize,
        max_tabs_per_window: usize,
        max_total_tabs: usize,
        max_closing_contexts: usize,
        max_history_entries: usize,
        max_total_history_bytes: usize,
        max_total_frame_bytes: usize,
        max_address_bytes: usize,
        max_engine_events_per_pump: usize,
    ) -> Result<Self, SessionLimitsError> {
        check_limit("max_windows", max_windows, MAX_WINDOWS)?;
        check_limit(
            "max_tabs_per_window",
            max_tabs_per_window,
            MAX_TABS_PER_WINDOW,
        )?;
        check_limit("max_total_tabs", max_total_tabs, MAX_TOTAL_TABS)?;
        check_limit("max_closing_contexts", max_closing_contexts, MAX_TOTAL_TABS)?;
        check_limit(
            "max_history_entries",
            max_history_entries,
            MAX_HISTORY_ENTRIES,
        )?;
        check_limit(
            "max_total_history_bytes",
            max_total_history_bytes,
            MAX_TOTAL_HISTORY_BYTES,
        )?;
        check_limit(
            "max_total_frame_bytes",
            max_total_frame_bytes,
            MAX_TOTAL_FRAME_BYTES,
        )?;
        check_limit(
            "max_address_bytes",
            max_address_bytes,
            MAX_NAVIGATION_URL_BYTES,
        )?;
        check_limit(
            "max_engine_events_per_pump",
            max_engine_events_per_pump,
            MAX_ENGINE_EVENTS_PER_PUMP,
        )?;
        if max_tabs_per_window > max_total_tabs {
            return Err(SessionLimitsError::Inconsistent {
                detail: "max_tabs_per_window exceeds max_total_tabs",
            });
        }
        if max_closing_contexts > max_total_tabs {
            return Err(SessionLimitsError::Inconsistent {
                detail: "max_closing_contexts exceeds max_total_tabs",
            });
        }
        Ok(Self {
            max_windows,
            max_tabs_per_window,
            max_total_tabs,
            max_closing_contexts,
            max_history_entries,
            max_total_history_bytes,
            max_total_frame_bytes,
            max_address_bytes,
            max_engine_events_per_pump,
        })
    }

    #[must_use]
    pub const fn max_windows(self) -> usize {
        self.max_windows
    }

    #[must_use]
    pub const fn max_tabs_per_window(self) -> usize {
        self.max_tabs_per_window
    }

    #[must_use]
    pub const fn max_total_tabs(self) -> usize {
        self.max_total_tabs
    }

    #[must_use]
    pub const fn max_closing_contexts(self) -> usize {
        self.max_closing_contexts
    }

    #[must_use]
    pub const fn max_history_entries(self) -> usize {
        self.max_history_entries
    }

    #[must_use]
    pub const fn max_total_history_bytes(self) -> usize {
        self.max_total_history_bytes
    }

    #[must_use]
    pub const fn max_total_frame_bytes(self) -> usize {
        self.max_total_frame_bytes
    }

    #[must_use]
    pub const fn max_address_bytes(self) -> usize {
        self.max_address_bytes
    }

    #[must_use]
    pub const fn max_engine_events_per_pump(self) -> usize {
        self.max_engine_events_per_pump
    }
}

fn check_limit(
    field: &'static str,
    value: usize,
    maximum: usize,
) -> Result<(), SessionLimitsError> {
    if value == 0 {
        Err(SessionLimitsError::Zero { field })
    } else if value > maximum {
        Err(SessionLimitsError::TooLarge {
            field,
            actual: value,
            maximum,
        })
    } else {
        Ok(())
    }
}

/// Invalid session resource policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionLimitsError {
    Zero {
        field: &'static str,
    },
    TooLarge {
        field: &'static str,
        actual: usize,
        maximum: usize,
    },
    Inconsistent {
        detail: &'static str,
    },
}

impl fmt::Display for SessionLimitsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "invalid browser session limits: {self:?}")
    }
}

impl std::error::Error for SessionLimitsError {}

/// Retained state of one session-history entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HistoryEntryState {
    Requested,
    Loading,
    Committed { http_status: u16 },
    Cancelled,
    Failed(ExecutionFailure),
}

/// Exact lifecycle of one navigation generation admitted for a tab.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NavigationPhase {
    Requested,
    Started,
    Committed,
    Ready,
    Cancelled,
    Failed,
}

impl NavigationPhase {
    const fn is_terminal(self) -> bool {
        matches!(self, Self::Ready | Self::Cancelled | Self::Failed)
    }
}

struct HistoryEntry {
    address: Box<str>,
    navigation: NavigationId,
    state: HistoryEntryState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TabFocus {
    Content,
    Address,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct EngineDocumentState {
    navigation: NavigationId,
    live_version: EngineDocumentVersion,
    frame_version: EngineDocumentVersion,
}

struct TabState {
    id: BrowserTabId,
    window: BrowserWindowId,
    context: TopLevelContextId,
    history: Vec<HistoryEntry>,
    history_index: Option<usize>,
    address: AddressEditState,
    focus: TabFocus,
    latest_navigation: Option<NavigationId>,
    navigation_phases: BTreeMap<NavigationId, NavigationPhase>,
    live_navigation: Option<NavigationId>,
    loading: Option<NavigationId>,
    stop_requested: bool,
    frame: Option<EngineFrameLease>,
    mutation_result: Option<EngineMutationResultLease>,
    engine_document: Option<EngineDocumentState>,
    last_document_failure: Option<wild_buzzard_engine::DocumentOperationFailure>,
    last_presentation_rerender: Option<PresentationRerenderTerminal>,
}

/// Window-shell lifecycle remembered by the product controller.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeWindowState {
    Starting,
    Running,
    Suspended,
    Destroyed,
}

struct WindowState {
    id: BrowserWindowId,
    tabs: Vec<BrowserTabId>,
    active: BrowserTabId,
    native_state: NativeWindowState,
    surface: Option<SurfaceId>,
    native_focused: bool,
    last_input_sequence: Option<u64>,
}

#[derive(Clone, Copy)]
struct ClosingContext {
    navigation: NavigationId,
    tab: BrowserTabId,
    window: BrowserWindowId,
}

/// Terminal browser-controller failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionFailure {
    EnginePanicked {
        operation: &'static str,
    },
    EngineDisconnected {
        status: EnginePortShutdownStatus,
    },
    EngineStopped {
        status: EnginePortShutdownStatus,
    },
    EngineEventSequence {
        expected: u64,
        received: u64,
    },
    EngineContract {
        detail: &'static str,
    },
    LinuxInputSequence {
        window: BrowserWindowId,
        previous: u64,
        received: u64,
    },
    LinuxEventOrder {
        window: BrowserWindowId,
        detail: &'static str,
    },
    LinuxSurfaceMismatch {
        window: BrowserWindowId,
    },
    LinuxStopped {
        window: BrowserWindowId,
        reason: LinuxStopReason,
    },
}

/// One-way session lifecycle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionLifecycle {
    Running,
    Closed {
        status: EnginePortShutdownStatus,
    },
    Failed {
        failure: SessionFailure,
        status: EnginePortShutdownStatus,
    },
}

impl SessionLifecycle {
    const fn is_running(self) -> bool {
        matches!(self, Self::Running)
    }

    const fn shutdown_status(self) -> Option<EnginePortShutdownStatus> {
        match self {
            Self::Running => None,
            Self::Closed { status } | Self::Failed { status, .. } => Some(status),
        }
    }
}

/// Product command independent of native UI widgets.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BrowserCommand {
    NewWindow,
    NewTab {
        window: BrowserWindowId,
    },
    CloseWindow {
        window: BrowserWindowId,
    },
    CloseTab {
        tab: BrowserTabId,
    },
    ActivateTab {
        tab: BrowserTabId,
    },
    Navigate {
        tab: BrowserTabId,
        address: Box<str>,
    },
    SubmitAddress {
        tab: BrowserTabId,
    },
    Back {
        tab: BrowserTabId,
    },
    Forward {
        tab: BrowserTabId,
    },
    Reload {
        tab: BrowserTabId,
    },
    Stop {
        tab: BrowserTabId,
    },
    FocusAddress {
        window: BrowserWindowId,
    },
    FocusContent {
        tab: BrowserTabId,
    },
}

/// Observable result of one admitted product command.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BrowserCommandOutcome {
    WindowOpened {
        window: BrowserWindowId,
        tab: BrowserTabId,
    },
    TabOpened {
        window: BrowserWindowId,
        tab: BrowserTabId,
    },
    TabActivated {
        window: BrowserWindowId,
        tab: BrowserTabId,
    },
    NavigationQueued {
        tab: BrowserTabId,
        navigation: NavigationId,
    },
    StopRequested {
        tab: BrowserTabId,
        navigation: NavigationId,
    },
    TabClosed {
        tab: BrowserTabId,
        window_closed: bool,
    },
    WindowClosed {
        window: BrowserWindowId,
    },
    AddressFocused {
        window: BrowserWindowId,
        tab: BrowserTabId,
    },
    ContentFocused {
        window: BrowserWindowId,
        tab: BrowserTabId,
    },
    NoChange,
    SessionClosed {
        status: EnginePortShutdownStatus,
    },
}

/// Result of polling the engine boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnginePumpOutcome {
    Empty,
    Applied,
    StaleSuppressed,
    RetiredContextSuppressed {
        navigation: NavigationId,
    },
    ContextCloseAcknowledged {
        tab: BrowserTabId,
        window: BrowserWindowId,
    },
    Batch {
        processed: usize,
        more_may_remain: bool,
    },
    FrameSuppressedByResourceLimit {
        navigation: NavigationId,
    },
    MutationAppliedFrameSuppressed {
        navigation: NavigationId,
        operation: wild_buzzard_engine::DocumentOperationId,
    },
}

/// Typed result of routing one Linux shell event.
#[derive(Debug, PartialEq)]
pub enum LinuxEventOutcome {
    Ignored,
    NativeStateChanged,
    AddressEdited {
        tab: BrowserTabId,
    },
    Command(BrowserCommandOutcome),
    EnginePumped {
        processed: usize,
        more: bool,
    },
    RedrawRequested {
        window: BrowserWindowId,
        surface: SurfaceId,
    },
    ContentInputUnrouted {
        window: BrowserWindowId,
        tab: BrowserTabId,
        origin: InputOrigin,
        event: InputEvent,
    },
    ChromeInputUnmapped {
        window: BrowserWindowId,
        tab: BrowserTabId,
        origin: InputOrigin,
        event: InputEvent,
    },
}

/// Exact terminal observation for the latest admitted presentation rerender.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PresentationRerenderTerminal {
    operation: wild_buzzard_engine::DocumentOperationId,
    failure: Option<wild_buzzard_engine::DocumentOperationFailure>,
}

impl PresentationRerenderTerminal {
    const fn new(
        operation: wild_buzzard_engine::DocumentOperationId,
        failure: Option<wild_buzzard_engine::DocumentOperationFailure>,
    ) -> Self {
        Self { operation, failure }
    }

    /// Exact operation identity returned when the rerender was admitted.
    #[must_use]
    pub const fn operation(self) -> wild_buzzard_engine::DocumentOperationId {
        self.operation
    }

    /// Terminal failure, or `None` for a completed rerender (including a
    /// semantically stale no-frame completion).
    #[must_use]
    pub const fn failure(self) -> Option<wild_buzzard_engine::DocumentOperationFailure> {
        self.failure
    }
}

/// Compact, owned inspection of one tab without exposing controller internals.
#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(clippy::struct_excessive_bools)]
pub struct TabSnapshot {
    pub id: BrowserTabId,
    pub window: BrowserWindowId,
    pub context: TopLevelContextId,
    pub address: Box<str>,
    pub address_selection: AddressSelection,
    pub address_dirty: bool,
    pub address_focused: bool,
    pub history_len: usize,
    pub history_index: Option<usize>,
    pub latest_navigation: Option<NavigationId>,
    /// Phase of `latest_navigation`, including its terminal outcome.
    pub latest_navigation_phase: Option<NavigationPhase>,
    /// Bounded admitted-navigation entries still checked for exact ordering.
    pub navigation_ledger_len: usize,
    /// Last navigation whose page was successfully published by the engine.
    pub live_navigation: Option<NavigationId>,
    pub loading: bool,
    pub stop_requested: bool,
    pub frame: Option<EnginePortFrameLeaseId>,
    pub mutation_result: Option<EnginePortMutationLeaseId>,
    /// Navigation owning the engine document-version state, when available.
    pub engine_document_navigation: Option<NavigationId>,
    /// Latest engine-announced live DOM revision, independent of UI frame retention.
    pub engine_live_version: Option<EngineDocumentVersion>,
    /// Latest engine-announced rendered revision, independent of UI frame retention.
    pub engine_frame_version: Option<EngineDocumentVersion>,
    pub last_document_failure: Option<wild_buzzard_engine::DocumentOperationFailure>,
    /// Exact terminal result of the most recently completed presentation
    /// rerender. A newly admitted rerender clears the prior observation.
    pub last_presentation_rerender: Option<PresentationRerenderTerminal>,
}

/// Compact, owned inspection of one browser window.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowSnapshot {
    pub id: BrowserWindowId,
    pub tabs: Box<[BrowserTabId]>,
    pub active: BrowserTabId,
    pub native_state: NativeWindowState,
    pub surface: Option<SurfaceId>,
    pub native_focused: bool,
}

/// Failure which either rejects one command transactionally or reports a
/// terminal lifecycle transition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionError {
    NotRunning,
    UnknownWindow(BrowserWindowId),
    UnknownTab(BrowserTabId),
    WindowLimit { maximum: usize },
    TabLimit { maximum: usize },
    ClosingContextLimit { maximum: usize },
    NavigationLedgerLimit { maximum: usize },
    IdentityExhausted { kind: &'static str },
    HistoryUnavailable,
    HistoryByteLimit { maximum: usize },
    Address(AddressEditError),
    NavigationRequest(NavigationRequestError),
    Engine(EnginePortError),
    Terminal(SessionFailure),
}

impl fmt::Display for SessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "browser session operation failed: {self:?}")
    }
}

impl std::error::Error for SessionError {}

/// Failure while atomically transferring one exact session candidate into its
/// final graphics-owned page package.
#[derive(Debug)]
pub enum SessionPresentationError {
    Session(SessionError),
    NotPresentationOutput,
    CandidateIdentityMismatch,
    Graphics(WebRenderWindowError),
}

impl fmt::Display for SessionPresentationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Session(error) => write!(formatter, "{error}"),
            Self::NotPresentationOutput => {
                formatter.write_str("the retained engine candidate is not a presentation scene")
            }
            Self::CandidateIdentityMismatch => formatter.write_str(
                "the retained presentation candidate disagrees with the tab's exact live labels",
            ),
            Self::Graphics(error) => write!(formatter, "page-scene validation failed: {error}"),
        }
    }
}

impl std::error::Error for SessionPresentationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Session(error) => Some(error),
            Self::Graphics(error) => Some(error),
            Self::NotPresentationOutput | Self::CandidateIdentityMismatch => None,
        }
    }
}

impl From<SessionError> for SessionPresentationError {
    fn from(error: SessionError) -> Self {
        Self::Session(error)
    }
}

/// Bounded multi-window browser-product controller.
pub struct BrowserSession<E: EnginePort> {
    engine: Option<E>,
    limits: SessionLimits,
    lifecycle: SessionLifecycle,
    windows: BTreeMap<BrowserWindowId, WindowState>,
    tabs: BTreeMap<BrowserTabId, TabState>,
    contexts: BTreeMap<TopLevelContextId, BrowserTabId>,
    closing_contexts: BTreeMap<TopLevelContextId, ClosingContext>,
    surfaces: HashMap<SurfaceId, BrowserWindowId>,
    next_window: Option<u64>,
    next_tab: Option<u64>,
    next_context: Option<u64>,
    history_bytes: usize,
    retained_frame_bytes: usize,
    navigation_ledger_entries: usize,
    last_engine_sequence: Option<u64>,
}

impl<E: EnginePort> BrowserSession<E> {
    /// Creates one initial blank window and address-focused tab.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError`] if the initial bounded identities or product
    /// state cannot be admitted.
    pub fn new(engine: E, limits: SessionLimits) -> Result<Self, SessionError> {
        let mut session = Self {
            engine: Some(engine),
            limits,
            lifecycle: SessionLifecycle::Running,
            windows: BTreeMap::new(),
            tabs: BTreeMap::new(),
            contexts: BTreeMap::new(),
            closing_contexts: BTreeMap::new(),
            surfaces: HashMap::new(),
            next_window: Some(1),
            next_tab: Some(1),
            next_context: Some(1),
            history_bytes: 0,
            retained_frame_bytes: 0,
            navigation_ledger_entries: 0,
            last_engine_sequence: None,
        };
        session.open_window_internal()?;
        Ok(session)
    }

    /// Current one-way lifecycle.
    #[must_use]
    pub const fn lifecycle(&self) -> SessionLifecycle {
        self.lifecycle
    }

    /// Resource policy fixed at construction.
    #[must_use]
    pub const fn limits(&self) -> SessionLimits {
        self.limits
    }

    #[cfg(test)]
    pub(crate) fn engine_mut_for_tests(&mut self) -> &mut E {
        self.engine
            .as_mut()
            .expect("running test session owns its engine")
    }

    /// Number of live product windows.
    #[must_use]
    pub fn window_count(&self) -> usize {
        self.windows.len()
    }

    /// Number of live tabs; closing engine tombstones are excluded.
    #[must_use]
    pub fn tab_count(&self) -> usize {
        self.tabs.len()
    }

    /// Number of bounded engine-close tombstones.
    #[must_use]
    pub fn closing_context_count(&self) -> usize {
        self.closing_contexts.len()
    }

    /// Aggregate retained session-history address bytes.
    #[must_use]
    pub const fn retained_history_bytes(&self) -> usize {
        self.history_bytes
    }

    /// Aggregate pixels currently owned by tab frame leases.
    #[must_use]
    pub const fn retained_frame_bytes(&self) -> usize {
        self.retained_frame_bytes
    }

    /// Aggregate fixed-size navigation phase entries retained for ordering.
    #[must_use]
    pub const fn navigation_ledger_entries(&self) -> usize {
        self.navigation_ledger_entries
    }

    /// Snapshot of one live window.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::UnknownWindow`] when `window` is not live.
    pub fn window_snapshot(&self, window: BrowserWindowId) -> Result<WindowSnapshot, SessionError> {
        let window = self
            .windows
            .get(&window)
            .ok_or(SessionError::UnknownWindow(window))?;
        Ok(WindowSnapshot {
            id: window.id,
            tabs: window.tabs.clone().into_boxed_slice(),
            active: window.active,
            native_state: window.native_state,
            surface: window.surface,
            native_focused: window.native_focused,
        })
    }

    /// Snapshot of one live tab.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::UnknownTab`] when `tab` is not live.
    pub fn tab_snapshot(&self, tab: BrowserTabId) -> Result<TabSnapshot, SessionError> {
        let tab = self.tabs.get(&tab).ok_or(SessionError::UnknownTab(tab))?;
        Ok(TabSnapshot {
            id: tab.id,
            window: tab.window,
            context: tab.context,
            address: tab.address.text().into(),
            address_selection: tab.address.selection(),
            address_dirty: tab.address.is_dirty(),
            address_focused: tab.focus == TabFocus::Address,
            history_len: tab.history.len(),
            history_index: tab.history_index,
            latest_navigation: tab.latest_navigation,
            latest_navigation_phase: tab
                .latest_navigation
                .and_then(|navigation| tab.navigation_phases.get(&navigation).copied()),
            navigation_ledger_len: tab.navigation_phases.len(),
            live_navigation: tab.live_navigation,
            loading: tab.loading.is_some(),
            stop_requested: tab.stop_requested,
            frame: tab.frame.as_ref().map(EngineFrameLease::lease_id),
            mutation_result: tab
                .mutation_result
                .as_ref()
                .map(EngineMutationResultLease::lease_id),
            engine_document_navigation: tab.engine_document.map(|state| state.navigation),
            engine_live_version: tab.engine_document.map(|state| state.live_version),
            engine_frame_version: tab.engine_document.map(|state| state.frame_version),
            last_document_failure: tab.last_document_failure,
            last_presentation_rerender: tab.last_presentation_rerender,
        })
    }

    /// Ordered retained history addresses for focused tests and chrome views.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::UnknownTab`] when `tab` is not live.
    pub fn history_addresses(&self, tab: BrowserTabId) -> Result<Vec<&str>, SessionError> {
        let tab = self.tabs.get(&tab).ok_or(SessionError::UnknownTab(tab))?;
        Ok(tab
            .history
            .iter()
            .map(|entry| entry.address.as_ref())
            .collect())
    }

    /// State of one retained history entry.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::UnknownTab`] when `tab` is not live. An absent
    /// entry is returned as `Ok(None)`.
    pub fn history_entry_state(
        &self,
        tab: BrowserTabId,
        index: usize,
    ) -> Result<Option<HistoryEntryState>, SessionError> {
        let tab = self.tabs.get(&tab).ok_or(SessionError::UnknownTab(tab))?;
        Ok(tab.history.get(index).map(|entry| entry.state))
    }

    /// Current frame, retained until replacement, explicit take, tab close, or shutdown.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::UnknownTab`] when `tab` is not live.
    pub fn frame(&self, tab: BrowserTabId) -> Result<Option<&EngineFrameLease>, SessionError> {
        Ok(self
            .tabs
            .get(&tab)
            .ok_or(SessionError::UnknownTab(tab))?
            .frame
            .as_ref())
    }

    /// Transfers the tab's current frame to the chrome presenter owner.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError`] when `tab` is unknown or retained-byte
    /// accounting is inconsistent.
    pub fn take_frame(
        &mut self,
        tab: BrowserTabId,
    ) -> Result<Option<EngineFrameLease>, SessionError> {
        let frame_bytes = self
            .tabs
            .get(&tab)
            .ok_or(SessionError::UnknownTab(tab))?
            .frame
            .as_ref()
            .map_or(0, |frame| frame.descriptor().retained_charge_bytes());
        let Some(retained_after) = self.retained_frame_bytes.checked_sub(frame_bytes) else {
            return self.fail(SessionFailure::EngineContract {
                detail: "frame byte accounting underflow during transfer",
            });
        };
        let frame = self
            .tabs
            .get_mut(&tab)
            .ok_or(SessionError::UnknownTab(tab))?
            .frame
            .take();
        self.retained_frame_bytes = retained_after;
        Ok(frame)
    }

    /// Revalidates and consumes the tab's exact presentation candidate into a
    /// final graphics-owned page package.
    ///
    /// Validation and removal occur in this one method: a stale, cross-tab,
    /// headless, or document-mismatched candidate is rejected without being
    /// consumed. The graphics scene revision is derived from the engine scene
    /// and cannot be supplied by the caller.
    ///
    /// # Errors
    ///
    /// Returns [`SessionPresentationError`] for unknown tabs, accounting drift,
    /// a non-presentation candidate, identity mismatch, or graphics validation.
    pub fn take_presentation_scene(
        &mut self,
        tab: BrowserTabId,
        expected_navigation: NavigationId,
        expected_document: EngineDocumentVersion,
        expected_scene_revision: u64,
        browser_navigation: BrowserNavigationIdentity,
    ) -> Result<Option<BrowserPageScene>, SessionPresentationError> {
        let state = self.tabs.get(&tab).ok_or(SessionError::UnknownTab(tab))?;
        let Some(frame) = state.frame.as_ref() else {
            return Ok(None);
        };
        let descriptor = frame
            .descriptor()
            .presentation_scene()
            .ok_or(SessionPresentationError::NotPresentationOutput)?;
        let navigation = state
            .live_navigation
            .ok_or(SessionPresentationError::CandidateIdentityMismatch)?;
        let document = state
            .engine_document
            .ok_or(SessionPresentationError::CandidateIdentityMismatch)?;
        if document.navigation != navigation
            || navigation != expected_navigation
            || document.frame_version != expected_document
            || frame.navigation() != navigation
            || frame.document_version() != Some(document.frame_version)
            || descriptor.document_version() != document.frame_version
            || descriptor.scene_revision() != expected_scene_revision
        {
            return Err(SessionPresentationError::CandidateIdentityMismatch);
        }
        let Some(frame) = self.take_frame(tab)? else {
            return Err(SessionPresentationError::CandidateIdentityMismatch);
        };
        let lease = frame
            .into_presentation()
            .map_err(|_| SessionPresentationError::NotPresentationOutput)?;
        lease
            .into_browser_page_scene(browser_navigation)
            .map(Some)
            .map_err(SessionPresentationError::Graphics)
    }

    /// Requests a fresh one-shot presentation candidate for the exact retained
    /// live navigation/document of `tab`.
    ///
    /// This is used after a compositor consumes a scene, including tab
    /// reactivation or a failed install. It performs no fetch, history change,
    /// DOM mutation, or navigation relabeling.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError`] when the tab has no exact live document, the
    /// labels disagree, the engine rejects the rerender, or the session is
    /// terminal.
    pub fn request_presentation_rerender(
        &mut self,
        tab: BrowserTabId,
    ) -> Result<wild_buzzard_engine::DocumentOperationId, SessionError> {
        self.ensure_running()?;
        let (navigation, version) = {
            let state = self.tabs.get(&tab).ok_or(SessionError::UnknownTab(tab))?;
            let navigation = state
                .live_navigation
                .ok_or(SessionError::HistoryUnavailable)?;
            let document = state
                .engine_document
                .ok_or(SessionError::HistoryUnavailable)?;
            if document.navigation != navigation {
                return self.fail(SessionFailure::EngineContract {
                    detail: "presentation rerender labels disagreed with the retained live page",
                });
            }
            (navigation, document.live_version)
        };
        let operation = self.call_engine("request presentation rerender", |engine| {
            engine.request_rerender(navigation, version)
        })?;
        self.tabs
            .get_mut(&tab)
            .ok_or(SessionError::UnknownTab(tab))?
            .last_presentation_rerender = None;
        Ok(operation)
    }

    /// Transfers the tab's current mutation result to its future document-task owner.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::UnknownTab`] when `tab` is not live.
    pub fn take_mutation_result(
        &mut self,
        tab: BrowserTabId,
    ) -> Result<Option<EngineMutationResultLease>, SessionError> {
        Ok(self
            .tabs
            .get_mut(&tab)
            .ok_or(SessionError::UnknownTab(tab))?
            .mutation_result
            .take())
    }

    /// Mutable per-tab address editor; all methods remain byte bounded.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError`] when the session is terminal or `tab` is not
    /// live.
    pub fn address_mut(
        &mut self,
        tab: BrowserTabId,
    ) -> Result<&mut AddressEditState, SessionError> {
        self.ensure_running()?;
        Ok(&mut self
            .tabs
            .get_mut(&tab)
            .ok_or(SessionError::UnknownTab(tab))?
            .address)
    }

    /// Dispatches one typed product command.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError`] when command preflight, engine admission, or a
    /// fail-closed lifecycle transition rejects the command.
    pub fn dispatch(
        &mut self,
        command: BrowserCommand,
    ) -> Result<BrowserCommandOutcome, SessionError> {
        match command {
            BrowserCommand::NewWindow => self.open_window(),
            BrowserCommand::NewTab { window } => self.open_tab(window),
            BrowserCommand::CloseWindow { window } => self.close_window(window),
            BrowserCommand::CloseTab { tab } => self.close_tab(tab),
            BrowserCommand::ActivateTab { tab } => self.activate_tab(tab),
            BrowserCommand::Navigate { tab, address } => self.navigate_new(tab, &address),
            BrowserCommand::SubmitAddress { tab } => self.submit_address(tab),
            BrowserCommand::Back { tab } => self.go_history(tab, -1),
            BrowserCommand::Forward { tab } => self.go_history(tab, 1),
            BrowserCommand::Reload { tab } => self.reload(tab),
            BrowserCommand::Stop { tab } => self.stop(tab),
            BrowserCommand::FocusAddress { window } => self.focus_address(window),
            BrowserCommand::FocusContent { tab } => self.focus_content(tab),
        }
    }

    /// Opens another bounded browser window with one blank tab.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError`] when the session is terminal, a resource bound
    /// is reached, or an identity is exhausted.
    pub fn open_window(&mut self) -> Result<BrowserCommandOutcome, SessionError> {
        self.ensure_running()?;
        let (window, tab) = self.open_window_internal()?;
        Ok(BrowserCommandOutcome::WindowOpened { window, tab })
    }

    fn open_window_internal(&mut self) -> Result<(BrowserWindowId, BrowserTabId), SessionError> {
        if self.windows.len() >= self.limits.max_windows {
            return Err(SessionError::WindowLimit {
                maximum: self.limits.max_windows,
            });
        }
        self.ensure_tab_capacity(None)?;
        let window = self.peek_window_id()?;
        let tab = self.peek_tab_id()?;
        let context = self.peek_context_id()?;
        self.commit_window_id();
        self.commit_tab_id();
        self.commit_context_id();
        self.tabs.insert(
            tab,
            TabState {
                id: tab,
                window,
                context,
                history: Vec::new(),
                history_index: None,
                address: AddressEditState::empty(self.limits.max_address_bytes),
                focus: TabFocus::Address,
                latest_navigation: None,
                navigation_phases: BTreeMap::new(),
                live_navigation: None,
                loading: None,
                stop_requested: false,
                frame: None,
                mutation_result: None,
                engine_document: None,
                last_document_failure: None,
                last_presentation_rerender: None,
            },
        );
        self.contexts.insert(context, tab);
        self.windows.insert(
            window,
            WindowState {
                id: window,
                tabs: vec![tab],
                active: tab,
                native_state: NativeWindowState::Starting,
                surface: None,
                native_focused: false,
                last_input_sequence: None,
            },
        );
        Ok((window, tab))
    }

    /// Opens and activates one blank tab without inventing a page load.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError`] when the window is unknown, the session is
    /// terminal, a tab bound is reached, or an identity is exhausted.
    pub fn open_tab(
        &mut self,
        window: BrowserWindowId,
    ) -> Result<BrowserCommandOutcome, SessionError> {
        self.ensure_running()?;
        self.ensure_tab_capacity(Some(window))?;
        let tab = self.peek_tab_id()?;
        let context = self.peek_context_id()?;
        self.commit_tab_id();
        self.commit_context_id();
        let window_state = self
            .windows
            .get_mut(&window)
            .ok_or(SessionError::UnknownWindow(window))?;
        window_state.tabs.push(tab);
        window_state.active = tab;
        self.tabs.insert(
            tab,
            TabState {
                id: tab,
                window,
                context,
                history: Vec::new(),
                history_index: None,
                address: AddressEditState::empty(self.limits.max_address_bytes),
                focus: TabFocus::Address,
                latest_navigation: None,
                navigation_phases: BTreeMap::new(),
                live_navigation: None,
                loading: None,
                stop_requested: false,
                frame: None,
                mutation_result: None,
                engine_document: None,
                last_document_failure: None,
                last_presentation_rerender: None,
            },
        );
        self.contexts.insert(context, tab);
        Ok(BrowserCommandOutcome::TabOpened { window, tab })
    }

    /// Activates one exact live tab, preserving every tab's address focus state.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError`] when the session is terminal or `tab` is not
    /// live.
    pub fn activate_tab(
        &mut self,
        tab: BrowserTabId,
    ) -> Result<BrowserCommandOutcome, SessionError> {
        self.ensure_running()?;
        let window = self
            .tabs
            .get(&tab)
            .ok_or(SessionError::UnknownTab(tab))?
            .window;
        let window_state = self
            .windows
            .get_mut(&window)
            .ok_or(SessionError::UnknownWindow(window))?;
        if window_state.active == tab {
            return Ok(BrowserCommandOutcome::NoChange);
        }
        window_state.active = tab;
        Ok(BrowserCommandOutcome::TabActivated { window, tab })
    }

    /// Navigates to a new bounded history entry transactionally.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError`] when the address or history exceeds policy,
    /// the tab is unavailable, engine admission fails, or a contract fault
    /// makes the session terminal.
    pub fn navigate_new(
        &mut self,
        tab: BrowserTabId,
        address: &str,
    ) -> Result<BrowserCommandOutcome, SessionError> {
        self.ensure_running()?;
        if address.len() > self.limits.max_address_bytes {
            return Err(SessionError::Address(AddressEditError::TooLong {
                actual: address.len(),
                maximum: self.limits.max_address_bytes,
            }));
        }
        let request = NavigationRequest::new(address).map_err(SessionError::NavigationRequest)?;
        self.preflight_history_append(tab, address.len())?;
        let retained_address: Box<str> = address.into();
        let navigation = self.send_navigation(tab, request)?;
        self.append_history_after_admission(tab, retained_address, navigation)?;
        Ok(BrowserCommandOutcome::NavigationQueued { tab, navigation })
    }

    /// Submits the exact current address draft.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError`] under the same conditions as
    /// [`Self::navigate_new`], or when `tab` is not live.
    pub fn submit_address(
        &mut self,
        tab: BrowserTabId,
    ) -> Result<BrowserCommandOutcome, SessionError> {
        self.ensure_running()?;
        let address = self
            .tabs
            .get(&tab)
            .ok_or(SessionError::UnknownTab(tab))?
            .address
            .text()
            .to_owned();
        let outcome = self.navigate_new(tab, &address)?;
        let Some(state) = self.tabs.get_mut(&tab) else {
            return self.fail(SessionFailure::EngineContract {
                detail: "admitted address submission lost its tab",
            });
        };
        state.focus = TabFocus::Content;
        state.address.clear_preedit();
        Ok(outcome)
    }

    fn go_history(
        &mut self,
        tab: BrowserTabId,
        delta: isize,
    ) -> Result<BrowserCommandOutcome, SessionError> {
        self.ensure_running()?;
        let (target, address) = {
            let state = self.tabs.get(&tab).ok_or(SessionError::UnknownTab(tab))?;
            let Some(index) = state.history_index else {
                return Ok(BrowserCommandOutcome::NoChange);
            };
            let Some(target) = index.checked_add_signed(delta) else {
                return Ok(BrowserCommandOutcome::NoChange);
            };
            let Some(entry) = state.history.get(target) else {
                return Ok(BrowserCommandOutcome::NoChange);
            };
            (target, entry.address.clone())
        };
        let request = NavigationRequest::new(&address).map_err(SessionError::NavigationRequest)?;
        let navigation = self.send_navigation(tab, request)?;
        let Some(state) = self.tabs.get_mut(&tab) else {
            return self.fail(SessionFailure::EngineContract {
                detail: "admitted history navigation lost its tab",
            });
        };
        state.history_index = Some(target);
        let Some(entry) = state.history.get_mut(target) else {
            return self.fail(SessionFailure::EngineContract {
                detail: "admitted history navigation lost its target entry",
            });
        };
        entry.navigation = navigation;
        entry.state = HistoryEntryState::Requested;
        state.address.accept_navigation_value(&address);
        Ok(BrowserCommandOutcome::NavigationQueued { tab, navigation })
    }

    /// Reloads the current history slot with a new exact navigation generation.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError`] when the tab or current history state is
    /// unavailable, engine admission fails, or the session is terminal.
    pub fn reload(&mut self, tab: BrowserTabId) -> Result<BrowserCommandOutcome, SessionError> {
        self.ensure_running()?;
        let (index, address) = {
            let state = self.tabs.get(&tab).ok_or(SessionError::UnknownTab(tab))?;
            let Some(index) = state.history_index else {
                return Ok(BrowserCommandOutcome::NoChange);
            };
            let entry = state
                .history
                .get(index)
                .ok_or(SessionError::HistoryUnavailable)?;
            (index, entry.address.clone())
        };
        let request = NavigationRequest::new(&address).map_err(SessionError::NavigationRequest)?;
        let navigation = self.send_navigation(tab, request)?;
        let Some(state) = self.tabs.get_mut(&tab) else {
            return self.fail(SessionFailure::EngineContract {
                detail: "admitted reload lost its tab",
            });
        };
        let Some(entry) = state.history.get_mut(index) else {
            return self.fail(SessionFailure::EngineContract {
                detail: "admitted reload lost its history entry",
            });
        };
        entry.navigation = navigation;
        entry.state = HistoryEntryState::Requested;
        state.address.accept_navigation_value(&address);
        Ok(BrowserCommandOutcome::NavigationQueued { tab, navigation })
    }

    /// Cancels only the exact navigation the tab still considers active.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError`] when the tab is unavailable, cancellation
    /// fails, or the session is terminal.
    pub fn stop(&mut self, tab: BrowserTabId) -> Result<BrowserCommandOutcome, SessionError> {
        self.ensure_running()?;
        let navigation = self
            .tabs
            .get(&tab)
            .ok_or(SessionError::UnknownTab(tab))?
            .loading;
        let Some(navigation) = navigation else {
            return Ok(BrowserCommandOutcome::NoChange);
        };
        match self.call_engine("cancel navigation", |engine| {
            engine.cancel_navigation(navigation)
        }) {
            Ok(()) => {
                self.tabs
                    .get_mut(&tab)
                    .ok_or(SessionError::UnknownTab(tab))?
                    .stop_requested = true;
                Ok(BrowserCommandOutcome::StopRequested { tab, navigation })
            }
            Err(SessionError::Engine(EnginePortError::Command(
                CommandErrorKind::NoActiveNavigation,
            ))) => Ok(BrowserCommandOutcome::NoChange),
            Err(error) => Err(error),
        }
    }

    /// Focuses and selects the active tab's complete address buffer.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError`] when the window or active tab is unavailable,
    /// or the session is terminal.
    pub fn focus_address(
        &mut self,
        window: BrowserWindowId,
    ) -> Result<BrowserCommandOutcome, SessionError> {
        self.ensure_running()?;
        let tab = self
            .windows
            .get(&window)
            .ok_or(SessionError::UnknownWindow(window))?
            .active;
        let state = self
            .tabs
            .get_mut(&tab)
            .ok_or(SessionError::UnknownTab(tab))?;
        state.focus = TabFocus::Address;
        state.address.select_all();
        Ok(BrowserCommandOutcome::AddressFocused { window, tab })
    }

    /// Moves keyboard focus to the exact live tab's content owner.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError`] when `tab` is unavailable or the session is
    /// terminal.
    pub fn focus_content(
        &mut self,
        tab: BrowserTabId,
    ) -> Result<BrowserCommandOutcome, SessionError> {
        self.ensure_running()?;
        let state = self
            .tabs
            .get_mut(&tab)
            .ok_or(SessionError::UnknownTab(tab))?;
        state.focus = TabFocus::Content;
        state.address.clear_preedit();
        Ok(BrowserCommandOutcome::ContentFocused {
            window: state.window,
            tab,
        })
    }

    /// Closes one tab after exact-context close admission. The active successor
    /// is the next tab in strip order, otherwise the previous tab.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError`] when close preflight or engine admission fails,
    /// or when a contract fault makes the session terminal.
    #[allow(clippy::too_many_lines)]
    pub fn close_tab(&mut self, tab: BrowserTabId) -> Result<BrowserCommandOutcome, SessionError> {
        self.ensure_running()?;
        let (window, navigation, context, history_bytes, frame_bytes, ledger_entries) = {
            let state = self.tabs.get(&tab).ok_or(SessionError::UnknownTab(tab))?;
            (
                state.window,
                state.latest_navigation,
                state.context,
                state
                    .history
                    .iter()
                    .map(|entry| entry.address.len())
                    .sum::<usize>(),
                state
                    .frame
                    .as_ref()
                    .map_or(0, |frame| frame.descriptor().retained_charge_bytes()),
                state.navigation_phases.len(),
            )
        };
        let membership_valid = self
            .windows
            .get(&window)
            .is_some_and(|state| state.tabs.contains(&tab));
        if !membership_valid {
            return self.fail(SessionFailure::EngineContract {
                detail: "tab is missing from its owning window",
            });
        }
        if self.contexts.get(&context).copied() != Some(tab)
            || self.closing_contexts.contains_key(&context)
        {
            return self.fail(SessionFailure::EngineContract {
                detail: "live tab context ownership is inconsistent",
            });
        }
        let Some(retained_history_after) = self.history_bytes.checked_sub(history_bytes) else {
            return self.fail(SessionFailure::EngineContract {
                detail: "history byte accounting underflow",
            });
        };
        let Some(retained_frames_after) = self.retained_frame_bytes.checked_sub(frame_bytes) else {
            return self.fail(SessionFailure::EngineContract {
                detail: "frame byte accounting underflow during tab close",
            });
        };
        let Some(ledger_after) = self.navigation_ledger_entries.checked_sub(ledger_entries) else {
            return self.fail(SessionFailure::EngineContract {
                detail: "navigation phase-ledger accounting underflow during tab close",
            });
        };
        if navigation.is_some() && self.closing_contexts.len() >= self.limits.max_closing_contexts {
            return Err(SessionError::ClosingContextLimit {
                maximum: self.limits.max_closing_contexts,
            });
        }
        if let Some(navigation) = navigation {
            self.call_engine("close context", |engine| engine.close_context(navigation))?;
        }

        let Some(_removed_tab) = self.tabs.remove(&tab) else {
            return self.fail(SessionFailure::EngineContract {
                detail: "admitted tab close lost its tab state",
            });
        };
        if self.contexts.remove(&context) != Some(tab) {
            return self.fail(SessionFailure::EngineContract {
                detail: "admitted tab close lost its context ownership",
            });
        }
        self.history_bytes = retained_history_after;
        self.retained_frame_bytes = retained_frames_after;
        self.navigation_ledger_entries = ledger_after;

        if let Some(navigation) = navigation {
            let replaced = self.closing_contexts.insert(
                context,
                ClosingContext {
                    navigation,
                    tab,
                    window,
                },
            );
            if replaced.is_some() {
                return self.fail(SessionFailure::EngineContract {
                    detail: "admitted tab close replaced a context tombstone",
                });
            }
        }

        let window_closed = {
            let Some(window_state) = self.windows.get_mut(&window) else {
                return self.fail(SessionFailure::EngineContract {
                    detail: "admitted tab close lost its owning window",
                });
            };
            let Some(index) = window_state
                .tabs
                .iter()
                .position(|candidate| *candidate == tab)
            else {
                return self.fail(SessionFailure::EngineContract {
                    detail: "admitted tab close lost window membership",
                });
            };
            window_state.tabs.remove(index);
            if window_state.tabs.is_empty() {
                true
            } else {
                if window_state.active == tab {
                    let successor = index.min(window_state.tabs.len() - 1);
                    window_state.active = window_state.tabs[successor];
                }
                false
            }
        };
        if window_closed {
            let removed = self.windows.remove(&window).ok_or(SessionError::Engine(
                EnginePortError::ContractViolation(
                    "closed window disappeared before surface retirement",
                ),
            ))?;
            if let Some(surface) = removed.surface
                && removed.native_state != NativeWindowState::Destroyed
                && self.surfaces.remove(&surface) != Some(window)
            {
                return self.fail(SessionFailure::EngineContract {
                    detail: "closed window lost its live surface registry entry",
                });
            }
        }
        if self.windows.is_empty() {
            let status = self.shutdown();
            return Ok(BrowserCommandOutcome::SessionClosed { status });
        }
        Ok(BrowserCommandOutcome::TabClosed { tab, window_closed })
    }

    /// Closes all tabs in one window. Any partial engine-close failure becomes
    /// a terminal session shutdown rather than a half-owned hidden window.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError`] when preflight or close admission fails. A
    /// failure after partial admission transitions the session terminally.
    pub fn close_window(
        &mut self,
        window: BrowserWindowId,
    ) -> Result<BrowserCommandOutcome, SessionError> {
        self.ensure_running()?;
        let tabs = self
            .windows
            .get(&window)
            .ok_or(SessionError::UnknownWindow(window))?
            .tabs
            .clone();
        let required_tombstones = tabs
            .iter()
            .filter(|tab| {
                self.tabs
                    .get(tab)
                    .is_some_and(|state| state.latest_navigation.is_some())
            })
            .count();
        let projected_tombstones = self
            .closing_contexts
            .len()
            .checked_add(required_tombstones)
            .ok_or(SessionError::ClosingContextLimit {
                maximum: self.limits.max_closing_contexts,
            })?;
        if projected_tombstones > self.limits.max_closing_contexts {
            return Err(SessionError::ClosingContextLimit {
                maximum: self.limits.max_closing_contexts,
            });
        }
        for (closed, tab) in tabs.into_iter().enumerate() {
            let outcome = match self.close_tab(tab) {
                Ok(outcome) => outcome,
                Err(error) if closed == 0 || !self.lifecycle.is_running() => return Err(error),
                Err(_) => {
                    return self.fail(SessionFailure::EngineContract {
                        detail: "engine rejected a window close after partial admission",
                    });
                }
            };
            if matches!(outcome, BrowserCommandOutcome::SessionClosed { .. }) {
                return Ok(outcome);
            }
        }
        Ok(BrowserCommandOutcome::WindowClosed { window })
    }

    /// Receives and applies at most one engine event.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError`] for a terminal session, an engine fault, or an
    /// invalid event/lease contract.
    pub fn poll_engine_once(&mut self) -> Result<EnginePumpOutcome, SessionError> {
        self.ensure_running()?;
        let event = self.call_engine("poll event", EnginePort::poll_event)?;
        let Some(event) = event else {
            return Ok(EnginePumpOutcome::Empty);
        };
        self.validate_engine_sequence(event)?;
        let applied = catch_unwind(AssertUnwindSafe(|| self.apply_engine_event(event)));
        match applied {
            Err(_) => self.fail(SessionFailure::EnginePanicked {
                operation: "apply engine event",
            }),
            Ok(Ok(outcome)) => Ok(outcome),
            Ok(Err(error)) if !self.lifecycle.is_running() => Err(error),
            Ok(Err(_)) => self.fail(SessionFailure::EngineContract {
                detail: "dequeued engine event could not be applied exactly",
            }),
        }
    }

    /// Drains a bounded number of events. A hostile producer cannot monopolize
    /// the caller; the batch result says when another wake may be required.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError`] when any polled engine event fails validation
    /// or the session is terminal.
    pub fn pump_engine(&mut self, maximum: usize) -> Result<EnginePumpOutcome, SessionError> {
        self.ensure_running()?;
        let maximum = maximum.min(self.limits.max_engine_events_per_pump);
        if maximum == 0 {
            return Ok(EnginePumpOutcome::Batch {
                processed: 0,
                more_may_remain: true,
            });
        }
        let mut processed = 0usize;
        while processed < maximum {
            match self.poll_engine_once()? {
                EnginePumpOutcome::Empty => {
                    return Ok(EnginePumpOutcome::Batch {
                        processed,
                        more_may_remain: false,
                    });
                }
                EnginePumpOutcome::Batch { .. } => {
                    return self.fail(SessionFailure::EngineContract {
                        detail: "single-event poll returned a batch result",
                    });
                }
                _ => processed += 1,
            }
        }
        Ok(EnginePumpOutcome::Batch {
            processed,
            more_may_remain: true,
        })
    }

    /// Routes one event from the shell instance assigned to `window`.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError`] when the window/surface contract, input order,
    /// mapped command, engine pump, or session lifecycle fails validation.
    #[allow(clippy::too_many_lines)]
    pub fn handle_linux_event(
        &mut self,
        window: BrowserWindowId,
        event: LinuxWindowEvent,
    ) -> Result<LinuxEventOutcome, SessionError> {
        self.ensure_running()?;
        if !self.windows.contains_key(&window) {
            if self.window_identity_was_allocated(window) {
                return Ok(LinuxEventOutcome::Ignored);
            }
            return Err(SessionError::UnknownWindow(window));
        }
        if self
            .windows
            .get(&window)
            .is_some_and(|state| state.native_state == NativeWindowState::Destroyed)
            && !matches!(&event, LinuxWindowEvent::Stopped(_))
        {
            return self.fail(SessionFailure::LinuxEventOrder {
                window,
                detail: "nonterminal native event followed Destroyed",
            });
        }
        match event {
            LinuxWindowEvent::Ready {
                desired_surface, ..
            } => {
                let ready_allowed = self.windows.get(&window).is_some_and(|state| {
                    state.surface.is_none()
                        && matches!(
                            state.native_state,
                            NativeWindowState::Starting | NativeWindowState::Running
                        )
                });
                if !ready_allowed {
                    return self.fail(SessionFailure::LinuxEventOrder {
                        window,
                        detail: "duplicate or out-of-order Ready event",
                    });
                }
                if self.surfaces.contains_key(&desired_surface.id) {
                    return self.fail(SessionFailure::LinuxEventOrder {
                        window,
                        detail: "Ready reused a live surface identity",
                    });
                }
                self.surfaces.insert(desired_surface.id, window);
                let state = self
                    .windows
                    .get_mut(&window)
                    .ok_or(SessionError::UnknownWindow(window))?;
                state.surface = Some(desired_surface.id);
                state.native_state = NativeWindowState::Running;
                Ok(LinuxEventOutcome::NativeStateChanged)
            }
            LinuxWindowEvent::Resumed => {
                let state = self
                    .windows
                    .get_mut(&window)
                    .ok_or(SessionError::UnknownWindow(window))?;
                if !matches!(
                    state.native_state,
                    NativeWindowState::Starting | NativeWindowState::Suspended
                ) {
                    return self.fail(SessionFailure::LinuxEventOrder {
                        window,
                        detail: "duplicate or out-of-order Resumed event",
                    });
                }
                state.native_state = NativeWindowState::Running;
                Ok(LinuxEventOutcome::NativeStateChanged)
            }
            LinuxWindowEvent::Suspended => {
                let state = self
                    .windows
                    .get_mut(&window)
                    .ok_or(SessionError::UnknownWindow(window))?;
                if state.native_state != NativeWindowState::Running {
                    return self.fail(SessionFailure::LinuxEventOrder {
                        window,
                        detail: "duplicate or out-of-order Suspended event",
                    });
                }
                state.native_state = NativeWindowState::Suspended;
                Ok(LinuxEventOutcome::NativeStateChanged)
            }
            LinuxWindowEvent::Resized { surface, .. }
            | LinuxWindowEvent::ScaleFactorChanged { surface, .. }
            | LinuxWindowEvent::ImeEnabled { surface } => {
                self.validate_surface(window, surface)?;
                Ok(LinuxEventOutcome::NativeStateChanged)
            }
            LinuxWindowEvent::FocusChanged { surface, focused } => {
                self.validate_surface(window, surface)?;
                self.windows
                    .get_mut(&window)
                    .ok_or(SessionError::UnknownWindow(window))?
                    .native_focused = focused;
                Ok(LinuxEventOutcome::NativeStateChanged)
            }
            LinuxWindowEvent::Input { event, origin } => self.handle_input(window, event, origin),
            LinuxWindowEvent::ImePreedit { surface, preedit } => {
                self.validate_surface(window, surface)?;
                let tab = self.active_tab(window)?;
                if self
                    .tabs
                    .get(&tab)
                    .ok_or(SessionError::UnknownTab(tab))?
                    .focus
                    != TabFocus::Address
                {
                    return Ok(LinuxEventOutcome::Ignored);
                }
                self.tabs
                    .get_mut(&tab)
                    .ok_or(SessionError::UnknownTab(tab))?
                    .address
                    .set_preedit(preedit.text(), preedit.selection())
                    .map_err(SessionError::Address)?;
                Ok(LinuxEventOutcome::AddressEdited { tab })
            }
            LinuxWindowEvent::ImeDisabled { surface } => {
                self.validate_surface(window, surface)?;
                let tab = self.active_tab(window)?;
                self.tabs
                    .get_mut(&tab)
                    .ok_or(SessionError::UnknownTab(tab))?
                    .address
                    .clear_preedit();
                Ok(LinuxEventOutcome::NativeStateChanged)
            }
            LinuxWindowEvent::RedrawRequested { surface } => {
                self.validate_surface(window, surface)?;
                Ok(LinuxEventOutcome::RedrawRequested { window, surface })
            }
            LinuxWindowEvent::WakeRequested => {
                let maximum = self.limits.max_engine_events_per_pump;
                match self.pump_engine(maximum)? {
                    EnginePumpOutcome::Batch {
                        processed,
                        more_may_remain,
                    } => Ok(LinuxEventOutcome::EnginePumped {
                        processed,
                        more: more_may_remain,
                    }),
                    EnginePumpOutcome::Empty => self.fail(SessionFailure::EngineContract {
                        detail: "bounded engine pump returned a single-event empty result",
                    }),
                    EnginePumpOutcome::Applied
                    | EnginePumpOutcome::StaleSuppressed
                    | EnginePumpOutcome::RetiredContextSuppressed { .. }
                    | EnginePumpOutcome::ContextCloseAcknowledged { .. }
                    | EnginePumpOutcome::FrameSuppressedByResourceLimit { .. }
                    | EnginePumpOutcome::MutationAppliedFrameSuppressed { .. } => {
                        self.fail(SessionFailure::EngineContract {
                            detail: "bounded engine pump returned a single-event result",
                        })
                    }
                }
            }
            LinuxWindowEvent::CloseRequested { surface } => {
                self.validate_surface(window, surface)?;
                self.close_window(window).map(LinuxEventOutcome::Command)
            }
            LinuxWindowEvent::Destroyed { surface } => {
                self.validate_surface(window, surface)?;
                if self.surfaces.remove(&surface) != Some(window) {
                    return self.fail(SessionFailure::LinuxEventOrder {
                        window,
                        detail: "Destroyed could not retire its exact live surface",
                    });
                }
                let state = self
                    .windows
                    .get_mut(&window)
                    .ok_or(SessionError::UnknownWindow(window))?;
                state.native_state = NativeWindowState::Destroyed;
                Ok(LinuxEventOutcome::NativeStateChanged)
            }
            LinuxWindowEvent::Stopped(report) => self.fail(SessionFailure::LinuxStopped {
                window,
                reason: report.reason,
            }),
        }
    }

    /// Idempotently shuts down the engine and drops every product lease/state.
    /// This boundary imposes no join deadline: a non-cooperating executor can
    /// block shutdown indefinitely even though receiver-owned shared queues,
    /// leases, document metadata, and accounting are released before the
    /// concrete engine enters its join. An executor-owned live page remains on
    /// its worker until executor finalization during that join.
    #[must_use]
    pub fn shutdown(&mut self) -> EnginePortShutdownStatus {
        if let Some(status) = self.lifecycle.shutdown_status() {
            return status;
        }
        let status = self.safe_engine_shutdown();
        self.clear_product_state();
        self.lifecycle = if status.reason() == EnginePortStopReason::PortPanicked {
            SessionLifecycle::Failed {
                failure: SessionFailure::EnginePanicked {
                    operation: "shutdown",
                },
                status,
            }
        } else {
            SessionLifecycle::Closed { status }
        };
        status
    }

    fn ensure_tab_capacity(&self, window: Option<BrowserWindowId>) -> Result<(), SessionError> {
        let owned = self
            .tabs
            .len()
            .checked_add(self.closing_contexts.len())
            .ok_or(SessionError::TabLimit {
                maximum: self.limits.max_total_tabs,
            })?;
        if owned >= self.limits.max_total_tabs {
            return Err(SessionError::TabLimit {
                maximum: self.limits.max_total_tabs,
            });
        }
        if let Some(window) = window {
            let count = self
                .windows
                .get(&window)
                .ok_or(SessionError::UnknownWindow(window))?
                .tabs
                .len();
            if count >= self.limits.max_tabs_per_window {
                return Err(SessionError::TabLimit {
                    maximum: self.limits.max_tabs_per_window,
                });
            }
        }
        Ok(())
    }

    fn send_navigation(
        &mut self,
        tab: BrowserTabId,
        request: NavigationRequest,
    ) -> Result<NavigationId, SessionError> {
        let (context, expected_generation) = {
            let state = self.tabs.get(&tab).ok_or(SessionError::UnknownTab(tab))?;
            let expected = match state.latest_navigation {
                Some(navigation) => navigation.generation().checked_next().ok_or(
                    SessionError::IdentityExhausted {
                        kind: "navigation generation",
                    },
                )?,
                None => NavigationGeneration::INITIAL,
            };
            (state.context, expected)
        };
        let prunable = self
            .tabs
            .values()
            .map(|state| {
                state
                    .navigation_phases
                    .iter()
                    .filter(|(navigation, phase)| {
                        Some(**navigation) != state.live_navigation && (**phase).is_terminal()
                    })
                    .count()
            })
            .sum::<usize>();
        let projected_ledger = self
            .navigation_ledger_entries
            .checked_sub(prunable)
            .and_then(|entries| entries.checked_add(1))
            .ok_or(SessionError::NavigationLedgerLimit {
                maximum: MAX_NAVIGATION_LEDGER_ENTRIES,
            })?;
        if projected_ledger > MAX_NAVIGATION_LEDGER_ENTRIES {
            return Err(SessionError::NavigationLedgerLimit {
                maximum: MAX_NAVIGATION_LEDGER_ENTRIES,
            });
        }
        let navigation =
            self.call_engine("navigate", |engine| engine.navigate(context, request))?;
        let expected = NavigationId::new(context, expected_generation);
        if navigation != expected {
            return self.fail(SessionFailure::EngineContract {
                detail: "engine returned a reused, skipped, or foreign navigation identity",
            });
        }
        for state in self.tabs.values_mut() {
            let live_navigation = state.live_navigation;
            state.navigation_phases.retain(|navigation, phase| {
                Some(*navigation) == live_navigation || !phase.is_terminal()
            });
        }
        let replaced = {
            let state = self
                .tabs
                .get_mut(&tab)
                .ok_or(SessionError::UnknownTab(tab))?;
            let replaced = state
                .navigation_phases
                .insert(navigation, NavigationPhase::Requested)
                .is_some();
            state.latest_navigation = Some(navigation);
            state.loading = Some(navigation);
            state.stop_requested = false;
            replaced
        };
        if replaced {
            return self.fail(SessionFailure::EngineContract {
                detail: "admitted navigation replaced a phase-ledger entry",
            });
        }
        self.navigation_ledger_entries = projected_ledger;
        // The concrete engine keeps the prior live page until this replacement
        // publishes successfully. Its frame, document state, and mutation
        // result remain bound to `live_navigation`; admission alone cannot
        // retire or relabel those capabilities.
        Ok(navigation)
    }

    fn preflight_history_append(
        &self,
        tab: BrowserTabId,
        address_bytes: usize,
    ) -> Result<(), SessionError> {
        let state = self.tabs.get(&tab).ok_or(SessionError::UnknownTab(tab))?;
        let keep = state.history_index.map_or(0, |index| index + 1);
        let truncated_bytes = state.history[keep..]
            .iter()
            .map(|entry| entry.address.len())
            .sum::<usize>();
        let retained_before_new = self.history_bytes.checked_sub(truncated_bytes).ok_or(
            SessionError::HistoryByteLimit {
                maximum: self.limits.max_total_history_bytes,
            },
        )?;
        let mut projected = retained_before_new.checked_add(address_bytes).ok_or(
            SessionError::HistoryByteLimit {
                maximum: self.limits.max_total_history_bytes,
            },
        )?;
        let projected_entries = keep
            .checked_add(1)
            .ok_or(SessionError::HistoryUnavailable)?;
        if projected_entries > self.limits.max_history_entries {
            let oldest = state.history.first().map_or(0, |entry| entry.address.len());
            projected = projected
                .checked_sub(oldest)
                .ok_or(SessionError::HistoryByteLimit {
                    maximum: self.limits.max_total_history_bytes,
                })?;
        }
        if projected > self.limits.max_total_history_bytes {
            return Err(SessionError::HistoryByteLimit {
                maximum: self.limits.max_total_history_bytes,
            });
        }
        Ok(())
    }

    fn append_history_after_admission(
        &mut self,
        tab: BrowserTabId,
        address: Box<str>,
        navigation: NavigationId,
    ) -> Result<(), SessionError> {
        let (keep, removed, oldest, resulting_entries) = {
            let Some(state) = self.tabs.get(&tab) else {
                return self.fail(SessionFailure::EngineContract {
                    detail: "admitted navigation lost its tab",
                });
            };
            let keep = state.history_index.map_or(0, |index| index + 1);
            let removed = state.history[keep..]
                .iter()
                .map(|entry| entry.address.len())
                .sum::<usize>();
            let Some(resulting_entries) = keep.checked_add(1) else {
                return self.fail(SessionFailure::EngineContract {
                    detail: "history entry count overflowed after admission",
                });
            };
            let oldest = if resulting_entries > self.limits.max_history_entries {
                state.history.first().map_or(0, |entry| entry.address.len())
            } else {
                0
            };
            (keep, removed, oldest, resulting_entries)
        };
        let Some(resulting_bytes) = self
            .history_bytes
            .checked_sub(removed)
            .and_then(|bytes| bytes.checked_add(address.len()))
            .and_then(|bytes| bytes.checked_sub(oldest))
        else {
            return self.fail(SessionFailure::EngineContract {
                detail: "history byte accounting failed after admission",
            });
        };
        if resulting_entries > self.limits.max_history_entries + 1
            || resulting_bytes > self.limits.max_total_history_bytes
        {
            return self.fail(SessionFailure::EngineContract {
                detail: "preflighted history append exceeded its resource policy",
            });
        }
        let Some(state) = self.tabs.get_mut(&tab) else {
            return self.fail(SessionFailure::EngineContract {
                detail: "admitted navigation lost its tab before history commit",
            });
        };
        state.history.truncate(keep);
        state.history.push(HistoryEntry {
            address,
            navigation,
            state: HistoryEntryState::Requested,
        });
        if state.history.len() > self.limits.max_history_entries {
            let _removed = state.history.remove(0);
        }
        state.history_index = state.history.len().checked_sub(1);
        let accepted_address = state
            .history_index
            .and_then(|index| state.history.get(index))
            .map(|entry| entry.address.clone());
        let Some(accepted_address) = accepted_address else {
            return self.fail(SessionFailure::EngineContract {
                detail: "history append did not retain its current address",
            });
        };
        state.address.accept_navigation_value(&accepted_address);
        self.history_bytes = resulting_bytes;
        Ok(())
    }

    fn validate_engine_sequence(&mut self, event: EnginePortEvent) -> Result<(), SessionError> {
        let received = event.sequence().get();
        let expected = match self.last_engine_sequence {
            Some(previous) => {
                let Some(expected) = previous.checked_add(1) else {
                    return self.fail(SessionFailure::EngineContract {
                        detail: "engine event sequence exhausted without shutdown",
                    });
                };
                expected
            }
            None => 1,
        };
        if received != expected {
            return self.fail(SessionFailure::EngineEventSequence { expected, received });
        }
        self.last_engine_sequence = Some(received);
        Ok(())
    }

    fn validate_document_transition(
        &mut self,
        tab: BrowserTabId,
        navigation: NavigationId,
        live_version: EngineDocumentVersion,
        frame_version: EngineDocumentVersion,
        detail: &'static str,
    ) -> Result<(), SessionError> {
        let actual = self
            .tabs
            .get(&tab)
            .ok_or(SessionError::UnknownTab(tab))?
            .engine_document;
        let expected = Some(EngineDocumentState {
            navigation,
            live_version,
            frame_version,
        });
        if actual != expected || !Self::document_pair_is_valid(live_version, frame_version) {
            return self.fail(SessionFailure::EngineContract { detail });
        }
        Ok(())
    }

    fn validate_mutation_advance(
        &mut self,
        previous: EngineDocumentVersion,
        live: EngineDocumentVersion,
        detail: &'static str,
    ) -> Result<(), SessionError> {
        let next_revision = previous.revision().checked_add(1);
        if previous.document() == 0
            || live.document() != previous.document()
            || next_revision != Some(live.revision())
        {
            return self.fail(SessionFailure::EngineContract { detail });
        }
        Ok(())
    }

    const fn document_pair_is_valid(
        live: EngineDocumentVersion,
        frame: EngineDocumentVersion,
    ) -> bool {
        live.document() != 0
            && frame.document() == live.document()
            && frame.revision() <= live.revision()
    }

    fn validate_initial_document_version(
        &mut self,
        version: Option<EngineDocumentVersion>,
    ) -> Result<EngineDocumentVersion, SessionError> {
        let Some(version) = version else {
            return self.fail(SessionFailure::EngineContract {
                detail: "initial frame omitted its document identity",
            });
        };
        if version.document() == 0 {
            return self.fail(SessionFailure::EngineContract {
                detail: "initial frame used a zero document identity",
            });
        }
        Ok(version)
    }

    fn validate_unchanged_document_state(
        &mut self,
        tab: BrowserTabId,
        navigation: NavigationId,
        live_version: Option<EngineDocumentVersion>,
        frame_version: Option<EngineDocumentVersion>,
        detail: &'static str,
    ) -> Result<(), SessionError> {
        let announced = match (live_version, frame_version) {
            (Some(live_version), Some(frame_version))
                if Self::document_pair_is_valid(live_version, frame_version) =>
            {
                Some(EngineDocumentState {
                    navigation,
                    live_version,
                    frame_version,
                })
            }
            _ => {
                return self.fail(SessionFailure::EngineContract { detail });
            }
        };
        let actual = self
            .tabs
            .get(&tab)
            .ok_or(SessionError::UnknownTab(tab))?
            .engine_document;
        if actual != announced {
            return self.fail(SessionFailure::EngineContract { detail });
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn validate_mutation_result(
        &mut self,
        result: &EngineMutationResultLease,
        navigation: NavigationId,
        lease: EnginePortMutationLeaseId,
        operation: wild_buzzard_engine::DocumentOperationId,
        live_version: EngineDocumentVersion,
        created_nodes: usize,
        detail: &'static str,
    ) -> Result<(), SessionError> {
        if result.navigation() != navigation
            || result.lease_id() != lease
            || result.operation() != operation
            || result.live_version() != live_version
            || result.created_nodes() != created_nodes
        {
            return self.fail(SessionFailure::EngineContract { detail });
        }
        Ok(())
    }

    fn validate_document_frame(
        &mut self,
        frame: &EngineFrameLease,
        live_version: EngineDocumentVersion,
        detail: &'static str,
    ) -> Result<(), SessionError> {
        if live_version.document() == 0 || frame.document_version() != Some(live_version) {
            return self.fail(SessionFailure::EngineContract { detail });
        }
        Ok(())
    }

    fn tab_for_known_navigation(
        &mut self,
        navigation: NavigationId,
        detail: &'static str,
    ) -> Result<Option<BrowserTabId>, SessionError> {
        let Some(tab) = self.contexts.get(&navigation.context()).copied() else {
            if self.closing_contexts.contains_key(&navigation.context())
                || self.context_identity_was_allocated(navigation.context())
            {
                return Ok(None);
            }
            return self.fail(SessionFailure::EngineContract { detail });
        };
        let state = self.tabs.get(&tab).ok_or(SessionError::UnknownTab(tab))?;
        if state.navigation_phases.contains_key(&navigation) {
            return Ok(Some(tab));
        }
        // Nonterminal entries are never pruned. An absent generation at or
        // below the latest admission is therefore a retired terminal identity
        // (or ledger drift), and any later event is an after-terminal fault.
        if state.latest_navigation.is_some() {
            return self.fail(SessionFailure::EngineContract { detail });
        }
        self.fail(SessionFailure::EngineContract { detail })
    }

    fn tab_for_live_document_event(
        &mut self,
        navigation: NavigationId,
    ) -> Result<Option<BrowserTabId>, SessionError> {
        let Some(tab) = self.tab_for_known_navigation(
            navigation,
            "document event used an unknown, retired, or future navigation",
        )?
        else {
            return Ok(None);
        };
        let state = self.tabs.get(&tab).ok_or(SessionError::UnknownTab(tab))?;
        if state.live_navigation != Some(navigation) {
            return self.fail(SessionFailure::EngineContract {
                detail: "document event did not belong to the retained live page",
            });
        }
        if state.navigation_phases.get(&navigation).copied() != Some(NavigationPhase::Ready) {
            return self.fail(SessionFailure::EngineContract {
                detail: "document event arrived before its navigation was ready",
            });
        }
        Ok(Some(tab))
    }

    fn require_navigation_phase(
        &mut self,
        tab: BrowserTabId,
        navigation: NavigationId,
        allowed: &[NavigationPhase],
        next: NavigationPhase,
        detail: &'static str,
    ) -> Result<(), SessionError> {
        let phase = self
            .tabs
            .get(&tab)
            .ok_or(SessionError::UnknownTab(tab))?
            .navigation_phases
            .get(&navigation)
            .copied()
            .ok_or(SessionError::Engine(EnginePortError::ContractViolation(
                detail,
            )))?;
        if !allowed.contains(&phase) {
            return self.fail(SessionFailure::EngineContract { detail });
        }
        self.tabs
            .get_mut(&tab)
            .ok_or(SessionError::UnknownTab(tab))?
            .navigation_phases
            .insert(navigation, next);
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn apply_engine_event(
        &mut self,
        event: EnginePortEvent,
    ) -> Result<EnginePumpOutcome, SessionError> {
        match event.kind() {
            EnginePortEventKind::NavigationStarted { navigation } => {
                let Some(tab) = self.tab_for_known_navigation(
                    navigation,
                    "navigation start used an unknown or retired identity",
                )?
                else {
                    return Ok(EnginePumpOutcome::StaleSuppressed);
                };
                self.require_navigation_phase(
                    tab,
                    navigation,
                    &[NavigationPhase::Requested],
                    NavigationPhase::Started,
                    "navigation start was duplicate or out of order",
                )?;
                let state = self
                    .tabs
                    .get_mut(&tab)
                    .ok_or(SessionError::UnknownTab(tab))?;
                Self::set_history_state(state, navigation, HistoryEntryState::Loading);
                Ok(EnginePumpOutcome::Applied)
            }
            EnginePortEventKind::NavigationCommitted {
                navigation,
                http_status,
            } => {
                let Some(tab) = self.tab_for_known_navigation(
                    navigation,
                    "navigation commit used an unknown or retired identity",
                )?
                else {
                    return Ok(EnginePumpOutcome::StaleSuppressed);
                };
                self.require_navigation_phase(
                    tab,
                    navigation,
                    &[NavigationPhase::Started],
                    NavigationPhase::Committed,
                    "navigation commit was duplicate or out of order",
                )?;
                let state = self
                    .tabs
                    .get_mut(&tab)
                    .ok_or(SessionError::UnknownTab(tab))?;
                Self::set_history_state(
                    state,
                    navigation,
                    HistoryEntryState::Committed { http_status },
                );
                Ok(EnginePumpOutcome::Applied)
            }
            EnginePortEventKind::FrameReady {
                navigation,
                lease,
                descriptor,
                document_version,
            } => self.apply_frame_event(navigation, lease, descriptor, document_version),
            EnginePortEventKind::NavigationCancelled { navigation } => {
                let Some(tab) = self.tab_for_known_navigation(
                    navigation,
                    "navigation cancellation used an unknown or retired identity",
                )?
                else {
                    return Ok(EnginePumpOutcome::StaleSuppressed);
                };
                self.require_navigation_phase(
                    tab,
                    navigation,
                    &[NavigationPhase::Requested, NavigationPhase::Started],
                    NavigationPhase::Cancelled,
                    "navigation cancellation was duplicate or out of order",
                )?;
                let state = self
                    .tabs
                    .get_mut(&tab)
                    .ok_or(SessionError::UnknownTab(tab))?;
                if state.loading == Some(navigation) {
                    state.loading = None;
                    state.stop_requested = false;
                }
                Self::set_history_state(state, navigation, HistoryEntryState::Cancelled);
                Ok(EnginePumpOutcome::Applied)
            }
            EnginePortEventKind::NavigationFailed {
                navigation,
                failure,
            } => {
                let Some(tab) = self.tab_for_known_navigation(
                    navigation,
                    "navigation failure used an unknown or retired identity",
                )?
                else {
                    return Ok(EnginePumpOutcome::StaleSuppressed);
                };
                self.require_navigation_phase(
                    tab,
                    navigation,
                    &[NavigationPhase::Started],
                    NavigationPhase::Failed,
                    "navigation failure was duplicate or out of order",
                )?;
                let state = self
                    .tabs
                    .get_mut(&tab)
                    .ok_or(SessionError::UnknownTab(tab))?;
                if state.loading == Some(navigation) {
                    state.loading = None;
                    state.stop_requested = false;
                }
                Self::set_history_state(state, navigation, HistoryEntryState::Failed(failure));
                Ok(EnginePumpOutcome::Applied)
            }
            EnginePortEventKind::DocumentMutationRendered {
                navigation,
                operation,
                previous_live_version,
                previous_frame_version,
                live_version,
                result: result_lease,
                created_nodes,
                frame: frame_lease,
                descriptor,
            } => {
                let Some(tab) = self.tab_for_live_document_event(navigation)? else {
                    self.discard_stale_result(navigation, result_lease)?;
                    self.discard_stale_frame(navigation, frame_lease)?;
                    return Ok(EnginePumpOutcome::StaleSuppressed);
                };
                self.validate_document_transition(
                    tab,
                    navigation,
                    previous_live_version,
                    previous_frame_version,
                    "rendered mutation did not continue the exact document versions",
                )?;
                self.validate_mutation_advance(
                    previous_live_version,
                    live_version,
                    "rendered mutation did not advance the exact document revision",
                )?;
                let Some(result) = self.take_checked_mutation_result(navigation, result_lease)?
                else {
                    // A later navigation or document invalidation may revoke
                    // this compound publication after its event was queued.
                    // Drain its independently keyed frame without applying a
                    // partial mutation outcome.
                    self.discard_stale_frame(navigation, frame_lease)?;
                    return Ok(EnginePumpOutcome::StaleSuppressed);
                };
                self.validate_mutation_result(
                    &result,
                    navigation,
                    result_lease,
                    operation,
                    live_version,
                    created_nodes,
                    "mutation result disagrees with its rendered event",
                )?;
                let frame = self.take_checked_frame(navigation, frame_lease, descriptor)?;
                if let Some(frame) = frame.as_ref() {
                    self.validate_document_frame(
                        frame,
                        live_version,
                        "mutation frame disagrees with its live document version",
                    )?;
                }
                let projected_frame_bytes = if frame.is_some() {
                    self.preflight_frame_install(tab, descriptor)?
                } else {
                    None
                };
                {
                    let state = self
                        .tabs
                        .get_mut(&tab)
                        .ok_or(SessionError::UnknownTab(tab))?;
                    state.mutation_result = Some(result);
                    state.engine_document = Some(EngineDocumentState {
                        navigation,
                        live_version,
                        frame_version: live_version,
                    });
                }
                match (frame, projected_frame_bytes) {
                    (Some(frame), None) => {
                        drop(frame);
                        self.suppress_retained_frame(tab)?;
                        self.tabs
                            .get_mut(&tab)
                            .ok_or(SessionError::UnknownTab(tab))?
                            .last_document_failure =
                            Some(wild_buzzard_engine::DocumentOperationFailure::ResourceLimit);
                        Ok(EnginePumpOutcome::MutationAppliedFrameSuppressed {
                            navigation,
                            operation,
                        })
                    }
                    (Some(frame), Some(projected_frame_bytes)) => {
                        let state = self
                            .tabs
                            .get_mut(&tab)
                            .ok_or(SessionError::UnknownTab(tab))?;
                        state.frame = Some(frame);
                        state.last_document_failure = None;
                        self.retained_frame_bytes = projected_frame_bytes;
                        Ok(EnginePumpOutcome::Applied)
                    }
                    (None, None) => {
                        self.suppress_retained_frame(tab)?;
                        self.tabs
                            .get_mut(&tab)
                            .ok_or(SessionError::UnknownTab(tab))?
                            .last_document_failure = None;
                        Ok(EnginePumpOutcome::StaleSuppressed)
                    }
                    (None, Some(_)) => self.fail(SessionFailure::EngineContract {
                        detail: "stale mutation frame acquired a retained-byte projection",
                    }),
                }
            }
            EnginePortEventKind::DocumentMutationCommittedWithoutFrame {
                navigation,
                operation,
                previous_live_version,
                live_version,
                frame_version,
                result: result_lease,
                created_nodes,
                failure,
            } => {
                let Some(tab) = self.tab_for_live_document_event(navigation)? else {
                    self.discard_stale_result(navigation, result_lease)?;
                    return Ok(EnginePumpOutcome::StaleSuppressed);
                };
                self.validate_document_transition(
                    tab,
                    navigation,
                    previous_live_version,
                    frame_version,
                    "no-frame mutation did not continue the exact document versions",
                )?;
                self.validate_mutation_advance(
                    previous_live_version,
                    live_version,
                    "no-frame mutation did not advance the exact document revision",
                )?;
                let Some(result) = self.take_checked_mutation_result(navigation, result_lease)?
                else {
                    return Ok(EnginePumpOutcome::StaleSuppressed);
                };
                self.validate_mutation_result(
                    &result,
                    navigation,
                    result_lease,
                    operation,
                    live_version,
                    created_nodes,
                    "mutation result disagrees with its no-frame event",
                )?;
                let state = self
                    .tabs
                    .get_mut(&tab)
                    .ok_or(SessionError::UnknownTab(tab))?;
                state.mutation_result = Some(result);
                state.engine_document = Some(EngineDocumentState {
                    navigation,
                    live_version,
                    frame_version,
                });
                state.last_document_failure = Some(failure);
                Ok(EnginePumpOutcome::Applied)
            }
            EnginePortEventKind::DocumentMutationRejected {
                navigation,
                live_version,
                frame_version,
                failure,
                ..
            } => {
                let Some(tab) = self.tab_for_live_document_event(navigation)? else {
                    return Ok(EnginePumpOutcome::StaleSuppressed);
                };
                self.validate_unchanged_document_state(
                    tab,
                    navigation,
                    live_version,
                    frame_version,
                    "rejected mutation disagrees with the current document versions",
                )?;
                self.tabs
                    .get_mut(&tab)
                    .ok_or(SessionError::UnknownTab(tab))?
                    .last_document_failure = Some(failure);
                Ok(EnginePumpOutcome::Applied)
            }
            EnginePortEventKind::DocumentRerendered {
                navigation,
                operation,
                live_version,
                previous_frame_version,
                frame,
                descriptor,
                ..
            } => self.apply_document_rerendered_event(
                navigation,
                operation,
                live_version,
                previous_frame_version,
                frame,
                descriptor,
            ),
            EnginePortEventKind::DocumentRerenderRejected {
                navigation,
                operation,
                live_version,
                frame_version,
                failure,
                ..
            } => {
                let Some(tab) = self.tab_for_live_document_event(navigation)? else {
                    return Ok(EnginePumpOutcome::StaleSuppressed);
                };
                self.validate_unchanged_document_state(
                    tab,
                    navigation,
                    live_version,
                    frame_version,
                    "rejected rerender disagrees with the current document versions",
                )?;
                let state = self
                    .tabs
                    .get_mut(&tab)
                    .ok_or(SessionError::UnknownTab(tab))?;
                state.last_document_failure = Some(failure);
                state.last_presentation_rerender =
                    Some(PresentationRerenderTerminal::new(operation, Some(failure)));
                Ok(EnginePumpOutcome::Applied)
            }
            EnginePortEventKind::ContextClosed { navigation } => {
                let context = navigation.context();
                if let Some(closing) = self.closing_contexts.get(&context).copied() {
                    if closing.navigation != navigation {
                        return self.fail(SessionFailure::EngineContract {
                            detail: "context close acknowledgement used the wrong generation",
                        });
                    }
                    self.closing_contexts.remove(&context);
                    return Ok(EnginePumpOutcome::ContextCloseAcknowledged {
                        tab: closing.tab,
                        window: closing.window,
                    });
                }
                if self.contexts.contains_key(&context) {
                    return self.fail(SessionFailure::EngineContract {
                        detail: "engine closed a context which still owns a live tab",
                    });
                }
                if self.context_identity_was_allocated(context) {
                    Ok(EnginePumpOutcome::RetiredContextSuppressed { navigation })
                } else {
                    self.fail(SessionFailure::EngineContract {
                        detail: "engine closed a context identity this session never allocated",
                    })
                }
            }
            EnginePortEventKind::ShutdownComplete { status } => {
                self.fail(SessionFailure::EngineStopped { status })
            }
        }
    }

    fn validate_frame_publication_candidate(
        &mut self,
        tab: BrowserTabId,
        navigation: NavigationId,
    ) -> Result<(), SessionError> {
        let (phase, live_navigation, live_phase) = {
            let state = self.tabs.get(&tab).ok_or(SessionError::UnknownTab(tab))?;
            let live_navigation = state.live_navigation;
            (
                state.navigation_phases.get(&navigation).copied(),
                live_navigation,
                live_navigation.and_then(|live| state.navigation_phases.get(&live).copied()),
            )
        };
        if phase != Some(NavigationPhase::Committed) {
            return self.fail(SessionFailure::EngineContract {
                detail: "frame publication was duplicate or out of order",
            });
        }
        if let Some(live_navigation) = live_navigation {
            if live_navigation.context() != navigation.context()
                || live_phase != Some(NavigationPhase::Ready)
            {
                return self.fail(SessionFailure::EngineContract {
                    detail: "retained live navigation lost its exact ready ledger entry",
                });
            }
            if navigation.generation() <= live_navigation.generation() {
                return self.fail(SessionFailure::EngineContract {
                    detail: "frame publication would roll back the retained live navigation",
                });
            }
        }
        Ok(())
    }

    fn apply_frame_event(
        &mut self,
        navigation: NavigationId,
        lease: EnginePortFrameLeaseId,
        descriptor: EngineFrameDescriptor,
        document_version: Option<EngineDocumentVersion>,
    ) -> Result<EnginePumpOutcome, SessionError> {
        let Some(tab) = self.tab_for_known_navigation(
            navigation,
            "frame publication used an unknown or retired navigation",
        )?
        else {
            self.discard_stale_frame(navigation, lease)?;
            return Ok(EnginePumpOutcome::StaleSuppressed);
        };
        self.validate_frame_publication_candidate(tab, navigation)?;
        let document_version = self.validate_initial_document_version(document_version)?;
        let frame = self.take_checked_frame(navigation, lease, descriptor)?;
        if let Some(frame) = frame.as_ref() {
            self.validate_document_frame(
                frame,
                document_version,
                "initial frame disagrees with its announced document version",
            )?;
        }
        let projected_frame_bytes = if frame.is_some() {
            self.preflight_frame_install(tab, descriptor)?
        } else {
            None
        };
        self.require_navigation_phase(
            tab,
            navigation,
            &[NavigationPhase::Committed],
            NavigationPhase::Ready,
            "frame publication did not complete the committed navigation",
        )?;
        let retired_prior = {
            let state = self
                .tabs
                .get_mut(&tab)
                .ok_or(SessionError::UnknownTab(tab))?;
            let prior_live = state.live_navigation.replace(navigation);
            let retired_prior = match prior_live.filter(|prior| *prior != navigation) {
                Some(prior) => {
                    if state.navigation_phases.remove(&prior) != Some(NavigationPhase::Ready) {
                        return Err(SessionError::Engine(EnginePortError::ContractViolation(
                            "published replacement could not retire the exact prior ready entry",
                        )));
                    }
                    true
                }
                None => false,
            };
            state.mutation_result = None;
            state.engine_document = Some(EngineDocumentState {
                navigation,
                live_version: document_version,
                frame_version: document_version,
            });
            state.last_document_failure = None;
            if state.loading == Some(navigation) {
                state.loading = None;
                state.stop_requested = false;
            }
            retired_prior
        };
        if retired_prior {
            self.navigation_ledger_entries =
                self.navigation_ledger_entries
                    .checked_sub(1)
                    .ok_or(SessionError::Engine(EnginePortError::ContractViolation(
                        "navigation phase-ledger accounting underflowed on publication",
                    )))?;
        }
        match (frame, projected_frame_bytes) {
            (Some(frame), Some(projected_frame_bytes)) => {
                self.tabs
                    .get_mut(&tab)
                    .ok_or(SessionError::UnknownTab(tab))?
                    .frame = Some(frame);
                self.retained_frame_bytes = projected_frame_bytes;
                Ok(EnginePumpOutcome::Applied)
            }
            (Some(frame), None) => {
                drop(frame);
                self.suppress_retained_frame(tab)?;
                Ok(EnginePumpOutcome::FrameSuppressedByResourceLimit { navigation })
            }
            (None, None) => {
                self.suppress_retained_frame(tab)?;
                Ok(EnginePumpOutcome::StaleSuppressed)
            }
            (None, Some(_)) => self.fail(SessionFailure::EngineContract {
                detail: "stale initial frame acquired a retained-byte projection",
            }),
        }
    }

    fn apply_document_rerendered_event(
        &mut self,
        navigation: NavigationId,
        operation: wild_buzzard_engine::DocumentOperationId,
        live_version: EngineDocumentVersion,
        previous_frame_version: EngineDocumentVersion,
        lease: EnginePortFrameLeaseId,
        descriptor: EngineFrameDescriptor,
    ) -> Result<EnginePumpOutcome, SessionError> {
        let Some(tab) = self.tab_for_live_document_event(navigation)? else {
            self.discard_stale_frame(navigation, lease)?;
            return Ok(EnginePumpOutcome::StaleSuppressed);
        };
        self.validate_document_transition(
            tab,
            navigation,
            live_version,
            previous_frame_version,
            "rerender did not continue the exact document versions",
        )?;
        let frame = self.take_checked_frame(navigation, lease, descriptor)?;
        if let Some(frame) = frame.as_ref() {
            self.validate_document_frame(
                frame,
                live_version,
                "rerendered frame disagrees with its live document version",
            )?;
        }
        let projected_frame_bytes = if frame.is_some() {
            self.preflight_frame_install(tab, descriptor)?
        } else {
            None
        };
        self.tabs
            .get_mut(&tab)
            .ok_or(SessionError::UnknownTab(tab))?
            .engine_document = Some(EngineDocumentState {
            navigation,
            live_version,
            frame_version: live_version,
        });
        match (frame, projected_frame_bytes) {
            (Some(frame), None) => {
                drop(frame);
                self.suppress_retained_frame(tab)?;
                self.tabs
                    .get_mut(&tab)
                    .ok_or(SessionError::UnknownTab(tab))?
                    .last_document_failure =
                    Some(wild_buzzard_engine::DocumentOperationFailure::ResourceLimit);
                self.tabs
                    .get_mut(&tab)
                    .ok_or(SessionError::UnknownTab(tab))?
                    .last_presentation_rerender = Some(PresentationRerenderTerminal::new(
                    operation,
                    Some(wild_buzzard_engine::DocumentOperationFailure::ResourceLimit),
                ));
                Ok(EnginePumpOutcome::FrameSuppressedByResourceLimit { navigation })
            }
            (Some(frame), Some(projected_frame_bytes)) => {
                let state = self
                    .tabs
                    .get_mut(&tab)
                    .ok_or(SessionError::UnknownTab(tab))?;
                state.frame = Some(frame);
                state.last_document_failure = None;
                state.last_presentation_rerender =
                    Some(PresentationRerenderTerminal::new(operation, None));
                self.retained_frame_bytes = projected_frame_bytes;
                Ok(EnginePumpOutcome::Applied)
            }
            (None, None) => {
                self.suppress_retained_frame(tab)?;
                let state = self
                    .tabs
                    .get_mut(&tab)
                    .ok_or(SessionError::UnknownTab(tab))?;
                state.last_document_failure = None;
                state.last_presentation_rerender =
                    Some(PresentationRerenderTerminal::new(operation, None));
                Ok(EnginePumpOutcome::StaleSuppressed)
            }
            (None, Some(_)) => self.fail(SessionFailure::EngineContract {
                detail: "stale rerender frame acquired a retained-byte projection",
            }),
        }
    }

    fn preflight_frame_install(
        &mut self,
        tab: BrowserTabId,
        descriptor: EngineFrameDescriptor,
    ) -> Result<Option<usize>, SessionError> {
        let old_bytes = self
            .tabs
            .get(&tab)
            .ok_or(SessionError::UnknownTab(tab))?
            .frame
            .as_ref()
            .map_or(0, |frame| frame.descriptor().retained_charge_bytes());
        let Some(projected) = self
            .retained_frame_bytes
            .checked_sub(old_bytes)
            .and_then(|bytes| bytes.checked_add(descriptor.retained_charge_bytes()))
        else {
            return self.fail(SessionFailure::EngineContract {
                detail: "frame byte accounting overflowed during event preflight",
            });
        };
        if projected > self.limits.max_total_frame_bytes {
            Ok(None)
        } else {
            Ok(Some(projected))
        }
    }

    fn take_checked_frame(
        &mut self,
        navigation: NavigationId,
        lease: EnginePortFrameLeaseId,
        descriptor: EngineFrameDescriptor,
    ) -> Result<Option<EngineFrameLease>, SessionError> {
        let Some(engine) = self.engine.as_mut() else {
            return self.fail(SessionFailure::EngineContract {
                detail: "frame transfer lost engine ownership",
            });
        };
        let transfer = catch_unwind(AssertUnwindSafe(|| engine.take_frame(navigation, lease)));
        let frame = match transfer {
            Err(_) => {
                return self.fail(SessionFailure::EnginePanicked {
                    operation: "take frame",
                });
            }
            Ok(Err(EnginePortError::FrameLease(FrameLeaseError::Stale))) => return Ok(None),
            Ok(Err(error)) => {
                return self
                    .resolve_engine_result::<EngineFrameLease>(Err(error))
                    .map(Some);
            }
            Ok(Ok(frame)) => frame,
        };
        if frame.navigation() != navigation
            || frame.lease_id() != lease
            || frame.descriptor() != descriptor
        {
            return self.fail(SessionFailure::EngineContract {
                detail: "transferred frame disagrees with its exact event",
            });
        }
        Ok(Some(frame))
    }

    fn take_checked_mutation_result(
        &mut self,
        navigation: NavigationId,
        lease: EnginePortMutationLeaseId,
    ) -> Result<Option<EngineMutationResultLease>, SessionError> {
        let Some(engine) = self.engine.as_mut() else {
            return self.fail(SessionFailure::EngineContract {
                detail: "mutation-result transfer lost engine ownership",
            });
        };
        let transfer = catch_unwind(AssertUnwindSafe(|| {
            engine.take_mutation_result(navigation, lease)
        }));
        match transfer {
            Err(_) => self.fail(SessionFailure::EnginePanicked {
                operation: "take mutation result",
            }),
            Ok(Err(EnginePortError::MutationLease(MutationResultLeaseError::Stale))) => Ok(None),
            Ok(Err(error)) => self
                .resolve_engine_result::<EngineMutationResultLease>(Err(error))
                .map(Some),
            Ok(Ok(result)) => Ok(Some(result)),
        }
    }

    fn suppress_retained_frame(&mut self, tab: BrowserTabId) -> Result<(), SessionError> {
        let retained = self
            .tabs
            .get(&tab)
            .ok_or(SessionError::UnknownTab(tab))?
            .frame
            .as_ref()
            .map_or(0, |frame| frame.descriptor().retained_charge_bytes());
        let Some(retained_after) = self.retained_frame_bytes.checked_sub(retained) else {
            return self.fail(SessionFailure::EngineContract {
                detail: "retained frame accounting underflowed during suppression",
            });
        };
        self.tabs
            .get_mut(&tab)
            .ok_or(SessionError::UnknownTab(tab))?
            .frame = None;
        self.retained_frame_bytes = retained_after;
        Ok(())
    }

    fn discard_stale_frame(
        &mut self,
        navigation: NavigationId,
        lease: EnginePortFrameLeaseId,
    ) -> Result<(), SessionError> {
        let Some(engine) = self.engine.as_mut() else {
            return self.fail(SessionFailure::EngineContract {
                detail: "stale-frame drain lost engine ownership",
            });
        };
        match catch_unwind(AssertUnwindSafe(|| engine.discard_frame(navigation, lease))) {
            Err(_) => self.fail(SessionFailure::EnginePanicked {
                operation: "discard stale frame",
            }),
            Ok(Ok(()) | Err(EnginePortError::FrameLease(FrameLeaseError::Stale))) => Ok(()),
            Ok(Err(error)) => self.resolve_engine_result(Err(error)),
        }
    }

    fn discard_stale_result(
        &mut self,
        navigation: NavigationId,
        lease: EnginePortMutationLeaseId,
    ) -> Result<(), SessionError> {
        let Some(engine) = self.engine.as_mut() else {
            return self.fail(SessionFailure::EngineContract {
                detail: "stale-result drain lost engine ownership",
            });
        };
        match catch_unwind(AssertUnwindSafe(|| {
            engine.discard_mutation_result(navigation, lease)
        })) {
            Err(_) => self.fail(SessionFailure::EnginePanicked {
                operation: "discard stale mutation result",
            }),
            Ok(Ok(()) | Err(EnginePortError::MutationLease(MutationResultLeaseError::Stale))) => {
                Ok(())
            }
            Ok(Err(error)) => self.resolve_engine_result(Err(error)),
        }
    }

    fn set_history_state(
        state: &mut TabState,
        navigation: NavigationId,
        history_state: HistoryEntryState,
    ) {
        if let Some(entry) = state
            .history
            .iter_mut()
            .find(|entry| entry.navigation == navigation)
        {
            entry.state = history_state;
        }
    }

    fn active_tab(&self, window: BrowserWindowId) -> Result<BrowserTabId, SessionError> {
        Ok(self
            .windows
            .get(&window)
            .ok_or(SessionError::UnknownWindow(window))?
            .active)
    }

    fn handle_input(
        &mut self,
        window: BrowserWindowId,
        event: InputEvent,
        origin: InputOrigin,
    ) -> Result<LinuxEventOutcome, SessionError> {
        let metadata = match &event {
            InputEvent::Pointer(event) => event.metadata,
            InputEvent::Scroll(event) => event.metadata,
            InputEvent::Key(event) => event.metadata,
            InputEvent::Text(event) => event.metadata,
        };
        self.validate_surface(window, metadata.surface)?;
        self.validate_input_sequence(window, metadata.sequence.get())?;
        let tab = self.active_tab(window)?;
        let address_focused = self
            .tabs
            .get(&tab)
            .ok_or(SessionError::UnknownTab(tab))?
            .focus
            == TabFocus::Address;
        let Some(action) = map_linux_input(&event, address_focused) else {
            return if address_focused {
                Ok(LinuxEventOutcome::ChromeInputUnmapped {
                    window,
                    tab,
                    origin,
                    event,
                })
            } else {
                Ok(LinuxEventOutcome::ContentInputUnrouted {
                    window,
                    tab,
                    origin,
                    event,
                })
            };
        };
        self.apply_input_action(window, tab, action)
    }

    fn apply_input_action(
        &mut self,
        window: BrowserWindowId,
        tab: BrowserTabId,
        action: LinuxInputAction,
    ) -> Result<LinuxEventOutcome, SessionError> {
        match action {
            LinuxInputAction::Shortcut(shortcut) => self
                .apply_shortcut(window, tab, shortcut)
                .map(LinuxEventOutcome::Command),
            LinuxInputAction::InsertText(text) => {
                self.tabs
                    .get_mut(&tab)
                    .ok_or(SessionError::UnknownTab(tab))?
                    .address
                    .insert(&text)
                    .map_err(SessionError::Address)?;
                Ok(LinuxEventOutcome::AddressEdited { tab })
            }
            LinuxInputAction::SelectAll => {
                self.tabs
                    .get_mut(&tab)
                    .ok_or(SessionError::UnknownTab(tab))?
                    .address
                    .select_all();
                Ok(LinuxEventOutcome::AddressEdited { tab })
            }
            LinuxInputAction::Backspace => {
                self.tabs
                    .get_mut(&tab)
                    .ok_or(SessionError::UnknownTab(tab))?
                    .address
                    .backspace();
                Ok(LinuxEventOutcome::AddressEdited { tab })
            }
            LinuxInputAction::DeleteForward => {
                self.tabs
                    .get_mut(&tab)
                    .ok_or(SessionError::UnknownTab(tab))?
                    .address
                    .delete_forward();
                Ok(LinuxEventOutcome::AddressEdited { tab })
            }
            LinuxInputAction::MoveCursor { movement, extend } => {
                self.tabs
                    .get_mut(&tab)
                    .ok_or(SessionError::UnknownTab(tab))?
                    .address
                    .move_cursor(movement, extend);
                Ok(LinuxEventOutcome::AddressEdited { tab })
            }
            LinuxInputAction::SubmitAddress => {
                self.submit_address(tab).map(LinuxEventOutcome::Command)
            }
            LinuxInputAction::Escape => {
                let should_revert = {
                    let state = self.tabs.get(&tab).ok_or(SessionError::UnknownTab(tab))?;
                    state.address.is_dirty() || state.address.preedit().is_some()
                };
                if should_revert {
                    let baseline = self.current_address(tab)?.to_owned();
                    self.tabs
                        .get_mut(&tab)
                        .ok_or(SessionError::UnknownTab(tab))?
                        .address
                        .revert_to(&baseline);
                    Ok(LinuxEventOutcome::AddressEdited { tab })
                } else {
                    self.stop(tab).map(LinuxEventOutcome::Command)
                }
            }
        }
    }

    fn apply_shortcut(
        &mut self,
        window: BrowserWindowId,
        tab: BrowserTabId,
        shortcut: LinuxShortcut,
    ) -> Result<BrowserCommandOutcome, SessionError> {
        match shortcut {
            LinuxShortcut::NewTab => self.open_tab(window),
            LinuxShortcut::CloseTab => self.close_tab(tab),
            LinuxShortcut::FocusAddress => self.focus_address(window),
            LinuxShortcut::Back => self.go_history(tab, -1),
            LinuxShortcut::Forward => self.go_history(tab, 1),
            LinuxShortcut::Reload => self.reload(tab),
            LinuxShortcut::Stop => self.stop(tab),
            LinuxShortcut::NextTab => self.activate_relative(window, 1),
            LinuxShortcut::PreviousTab => self.activate_relative(window, -1),
            LinuxShortcut::ActivatePosition { one_based } => {
                let target = {
                    let state = self
                        .windows
                        .get(&window)
                        .ok_or(SessionError::UnknownWindow(window))?;
                    if one_based == 9 {
                        state.tabs.last().copied()
                    } else {
                        state.tabs.get(usize::from(one_based - 1)).copied()
                    }
                };
                match target {
                    Some(target) => self.activate_tab(target),
                    None => Ok(BrowserCommandOutcome::NoChange),
                }
            }
        }
    }

    fn activate_relative(
        &mut self,
        window: BrowserWindowId,
        delta: isize,
    ) -> Result<BrowserCommandOutcome, SessionError> {
        let target = {
            let state = self
                .windows
                .get(&window)
                .ok_or(SessionError::UnknownWindow(window))?;
            let Some(index) = state.tabs.iter().position(|tab| *tab == state.active) else {
                return self.fail(SessionFailure::EngineContract {
                    detail: "active tab is missing from its window",
                });
            };
            let len = state.tabs.len();
            if len == 0 {
                return self.fail(SessionFailure::EngineContract {
                    detail: "live browser window has no tabs",
                });
            }
            let target = match delta {
                1 => (index + 1) % len,
                -1 => (index + len - 1) % len,
                _ => {
                    return self.fail(SessionFailure::EngineContract {
                        detail: "tab cycling received an unsupported delta",
                    });
                }
            };
            state.tabs[target]
        };
        self.activate_tab(target)
    }

    fn current_address(&self, tab: BrowserTabId) -> Result<&str, SessionError> {
        let state = self.tabs.get(&tab).ok_or(SessionError::UnknownTab(tab))?;
        Ok(state
            .history_index
            .and_then(|index| state.history.get(index))
            .map_or("", |entry| entry.address.as_ref()))
    }

    fn validate_surface(
        &mut self,
        window: BrowserWindowId,
        surface: SurfaceId,
    ) -> Result<(), SessionError> {
        let state = self
            .windows
            .get(&window)
            .ok_or(SessionError::UnknownWindow(window))?;
        if state.native_state == NativeWindowState::Destroyed {
            return self.fail(SessionFailure::LinuxEventOrder {
                window,
                detail: "surface event followed Destroyed",
            });
        }
        if state.surface != Some(surface) || self.surfaces.get(&surface).copied() != Some(window) {
            return self.fail(SessionFailure::LinuxSurfaceMismatch { window });
        }
        Ok(())
    }

    fn validate_input_sequence(
        &mut self,
        window: BrowserWindowId,
        received: u64,
    ) -> Result<(), SessionError> {
        let previous = self
            .windows
            .get(&window)
            .ok_or(SessionError::UnknownWindow(window))?
            .last_input_sequence;
        if let Some(previous) = previous
            && received <= previous
        {
            return self.fail(SessionFailure::LinuxInputSequence {
                window,
                previous,
                received,
            });
        }
        self.windows
            .get_mut(&window)
            .ok_or(SessionError::UnknownWindow(window))?
            .last_input_sequence = Some(received);
        Ok(())
    }

    fn call_engine<T>(
        &mut self,
        operation: &'static str,
        call: impl FnOnce(&mut E) -> Result<T, EnginePortError>,
    ) -> Result<T, SessionError> {
        let Some(engine) = self.engine.as_mut() else {
            return self.fail(SessionFailure::EngineContract {
                detail: "running session no longer owned its engine",
            });
        };
        let result = catch_unwind(AssertUnwindSafe(|| call(engine)));
        let Ok(result) = result else {
            return self.fail(SessionFailure::EnginePanicked { operation });
        };
        self.resolve_engine_result(result)
    }

    fn resolve_engine_result<T>(
        &mut self,
        result: Result<T, EnginePortError>,
    ) -> Result<T, SessionError> {
        match result {
            Ok(value) => Ok(value),
            Err(EnginePortError::ReceiverClosed(status)) => {
                self.fail(SessionFailure::EngineDisconnected { status })
            }
            Err(EnginePortError::ContractViolation(detail)) => {
                self.fail(SessionFailure::EngineContract { detail })
            }
            Err(
                error @ (EnginePortError::LeaseNavigationMismatch { .. }
                | EnginePortError::FrameLease(_)
                | EnginePortError::MutationLease(_)),
            ) => {
                let _ = error;
                self.fail(SessionFailure::EngineContract {
                    detail: "engine lease transfer violated its exact binding",
                })
            }
            Err(
                error @ EnginePortError::Command(
                    CommandErrorKind::EventReceiverDropped | CommandErrorKind::ShuttingDown,
                ),
            ) => {
                let _ = error;
                let status = self.safe_engine_shutdown();
                self.clear_product_state();
                let failure = SessionFailure::EngineDisconnected { status };
                self.lifecycle = SessionLifecycle::Failed { failure, status };
                Err(SessionError::Terminal(failure))
            }
            Err(error) => Err(SessionError::Engine(error)),
        }
    }

    fn fail<T>(&mut self, failure: SessionFailure) -> Result<T, SessionError> {
        if !self.lifecycle.is_running() {
            return Err(SessionError::NotRunning);
        }
        let status = self.safe_engine_shutdown();
        self.clear_product_state();
        self.lifecycle = SessionLifecycle::Failed { failure, status };
        Err(SessionError::Terminal(failure))
    }

    fn safe_engine_shutdown(&mut self) -> EnginePortShutdownStatus {
        let Some(mut engine) = self.engine.take() else {
            return self
                .lifecycle
                .shutdown_status()
                .unwrap_or_else(EnginePortShutdownStatus::port_panicked);
        };
        let shutdown = catch_unwind(AssertUnwindSafe(|| engine.shutdown()));
        let dropped = catch_unwind(AssertUnwindSafe(|| drop(engine)));
        match (shutdown, dropped) {
            (Ok(status), Ok(())) => status,
            (Err(_), Ok(()) | Err(_)) | (Ok(_), Err(_)) => {
                EnginePortShutdownStatus::port_panicked()
            }
        }
    }

    fn clear_product_state(&mut self) {
        self.windows.clear();
        self.tabs.clear();
        self.contexts.clear();
        self.closing_contexts.clear();
        self.surfaces.clear();
        self.history_bytes = 0;
        self.retained_frame_bytes = 0;
        self.navigation_ledger_entries = 0;
    }

    fn ensure_running(&self) -> Result<(), SessionError> {
        if self.lifecycle.is_running() {
            Ok(())
        } else {
            Err(SessionError::NotRunning)
        }
    }

    fn context_identity_was_allocated(&self, context: TopLevelContextId) -> bool {
        self.next_context.is_none_or(|next| context.get() < next)
    }

    fn window_identity_was_allocated(&self, window: BrowserWindowId) -> bool {
        self.next_window.is_none_or(|next| window.get() < next)
    }

    fn peek_window_id(&self) -> Result<BrowserWindowId, SessionError> {
        self.next_window
            .and_then(BrowserWindowId::new)
            .ok_or(SessionError::IdentityExhausted { kind: "window" })
    }

    fn peek_tab_id(&self) -> Result<BrowserTabId, SessionError> {
        self.next_tab
            .and_then(BrowserTabId::new)
            .ok_or(SessionError::IdentityExhausted { kind: "tab" })
    }

    fn peek_context_id(&self) -> Result<TopLevelContextId, SessionError> {
        self.next_context
            .and_then(TopLevelContextId::new)
            .ok_or(SessionError::IdentityExhausted {
                kind: "top-level context",
            })
    }

    fn commit_window_id(&mut self) {
        self.next_window = self.next_window.and_then(|value| value.checked_add(1));
    }

    fn commit_tab_id(&mut self) {
        self.next_tab = self.next_tab.and_then(|value| value.checked_add(1));
    }

    fn commit_context_id(&mut self) {
        self.next_context = self.next_context.and_then(|value| value.checked_add(1));
    }
}

impl<E: EnginePort> Drop for BrowserSession<E> {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}
