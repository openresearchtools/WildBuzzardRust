//! Bounded browser-product state above the Wild Buzzard engine and Linux shell.
//!
//! The controller owns product identities and state. It communicates with the
//! page engine only through [`EnginePort`] and consumes Linux input only through
//! first-party value types. It deliberately has no renderer or native-window
//! authority.

#![forbid(unsafe_code)]

mod address;
mod engine;
mod input;
mod primary_ui;
mod session;

pub use address::{
    AddressEditError, AddressEditState, AddressPreedit, AddressSelection, CursorMove,
};
pub use engine::{
    EngineDocumentVersion, EngineFrameDescriptor, EngineFrameLease, EngineMutationResultLease,
    EnginePort, EnginePortError, EnginePortEvent, EnginePortEventKind, EnginePortExecutorShutdown,
    EnginePortFrameLeaseId, EnginePortMutationLeaseId, EnginePortSequence,
    EnginePortShutdownStatus, EnginePortStopReason, EnginePresentationDescriptor,
    EnginePresentationIdentity, EnginePresentationLease, EngineRgba8Descriptor,
    NavigationEnginePort, NavigationEnginePortStartError,
};
pub use input::{
    LinuxInputAction, LinuxShortcut, PrimaryUiInputContext, map_linux_input,
    map_linux_primary_input,
};
pub use primary_ui::{
    MAX_PRIMARY_UI_LABEL_BYTES, MAX_PRIMARY_UI_PANEL_ROWS, MAX_PRIMARY_UI_SCROLL_ROWS,
    PrimaryReloadStopMode, PrimarySiteIdentityKind, PrimaryUiAction, PrimaryUiActionBinding,
    PrimaryUiActionOutcome, PrimaryUiAvailability, PrimaryUiControl, PrimaryUiControlSet,
    PrimaryUiControlSnapshot, PrimaryUiDirection, PrimaryUiElementId, PrimaryUiFocus,
    PrimaryUiInteraction, PrimaryUiLayout, PrimaryUiLayoutError, PrimaryUiMoveDirection,
    PrimaryUiPanel, PrimaryUiPanelItemAction, PrimaryUiPanelItemId, PrimaryUiPanelItemSnapshot,
    PrimaryUiPanelSnapshot, PrimaryUiRevision, PrimaryUiRole, PrimaryUiSemanticNode,
    PrimaryUiSnapshot, PrimaryUiTabSnapshot,
};
pub use session::{
    BrowserCommand, BrowserCommandOutcome, BrowserSession, BrowserTabId, BrowserWindowId,
    EnginePumpOutcome, HistoryEntryState, LinuxEventOutcome, NativeWindowState, NavigationPhase,
    PresentationRerenderTerminal, SessionError, SessionFailure, SessionLifecycle, SessionLimits,
    SessionLimitsError, SessionPresentationError, TabSnapshot, WindowSnapshot,
};

pub use wild_buzzard_engine::{
    CommandErrorKind, DocumentOperationFailure, ExecutionFailure, ExecutionFailureKind,
    NavigationGeneration, NavigationId, NavigationRequestError, NavigationStage, TopLevelContextId,
};
