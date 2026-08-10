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
pub use input::{LinuxInputAction, LinuxShortcut, map_linux_input};
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
