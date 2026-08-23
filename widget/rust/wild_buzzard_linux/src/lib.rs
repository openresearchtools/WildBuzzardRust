//! Rust-native Linux window ownership and event normalization.
//!
//! The crate deliberately exposes no winit types or native display/window
//! handles. It creates one top-level window, attaches the direct EGL presenter,
//! and maps events onto pointer-free Wild Buzzard contracts.

#![forbid(unsafe_code)]

#[cfg(not(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu")))]
compile_error!("wild_buzzard_linux supports only x86_64-unknown-linux-gnu");

mod config;
mod event;
mod lifecycle;
mod normalize;
mod queue;
mod shell;

pub use config::{
    ConfigError, LinuxBackendPreference, LinuxPresentationMode, LinuxShellConfig, LinuxShellLimits,
    MAX_APPLICATION_ID_BYTES, MAX_DEVICE_CAPACITY, MAX_EVENT_CAPACITY, MAX_IME_BYTES,
    MAX_TITLE_BYTES, MAX_TOUCH_CAPACITY,
};
pub use event::{
    BoundedImeText, ControlError, ImeTextError, InputOrigin, LinuxBackend,
    LinuxBrowserShutdownFailure, LinuxPresentationShutdown, LinuxShutdownReport, LinuxStopReason,
    LinuxWindowEvent,
};
pub use shell::{
    LinuxProfileWindowIdentityError, LinuxShellError, LinuxWakeHandle, LinuxWakeStatus,
    LinuxWindowControl, LinuxWindowHandler, LinuxWindowShell,
};

pub use wild_buzzard_linux_presenter::{
    BrowserAddressSelection, BrowserBgra8Crop, BrowserCaptureCopyError, BrowserChromeDirection,
    BrowserChromeElementIdentity, BrowserChromeFocus, BrowserChromeGeometry, BrowserChromeRevision,
    BrowserChromeScene, BrowserChromeState, BrowserChromeTab, BrowserElementAvailability,
    BrowserElementExpansion, BrowserElementInteraction, BrowserElementSelection,
    BrowserFrameCapture, BrowserFrameReceipt, BrowserFrameRequest, BrowserHitTarget,
    BrowserHitTestResult, BrowserNavigationIdentity, BrowserPageIdentity, BrowserPageScene,
    BrowserPageSceneRevision, BrowserPageSnapshot, BrowserPageUpdate, BrowserPrimaryActionKind,
    BrowserPrimaryChromeLayout, BrowserPrimaryChromeState, BrowserPrimaryControl,
    BrowserPrimaryControlKind, BrowserPrimaryControlPlacement, BrowserPrimaryLayoutPreview,
    BrowserPrimaryPopup, BrowserPrimaryPopupKind, BrowserPrimaryPopupRow,
    BrowserPrimaryPopupRowKind, BrowserPrimaryPreviewControl, BrowserReloadStopMode,
    BrowserResolvedPrimaryControl, BrowserResolvedPrimaryPopup, BrowserResolvedPrimaryPopupRow,
    BrowserSiteIdentityKind, BrowserTabIdentity, DirectFrameRequest, LinuxAccelerationClass,
    LinuxPresentationBackend, LinuxPresentationCapabilities, LinuxPresentationPolicy,
    LinuxResetProtection, MAX_BROWSER_CAPTURE_BYTES, MAX_BROWSER_CAPTURE_DIMENSION,
    MAX_BROWSER_CAPTURE_PIXELS, MAX_BROWSER_CHROME_DISPLAY_LIST_BYTES, MAX_BROWSER_CHROME_GLYPHS,
    MAX_BROWSER_CHROME_RUNS, MAX_BROWSER_CHROME_TABS, MAX_BROWSER_CHROME_TEXT_BYTES,
    MAX_BROWSER_CHROME_TEXTS, MAX_BROWSER_PRIMARY_CONTROLS, MAX_BROWSER_PRIMARY_POPUP_ROWS,
    MAX_LINUX_PRESENTATION_PROFILE_ATTEMPTS, MIN_BROWSER_CAPTURE_DIMENSION, PresentationError,
    PresentationErrorKind, PresentationFailureStage, PresentationRetentionReport,
    PresentationShutdownReport, PresentationStartupFailure, PresentationState,
    PresentationTeardownOutcome, SolidColor, SolidColorFrame, SwapSubmissionReceipt,
    WebRenderSurfaceSnapshot, WebRenderTeardownEvidence, WebRenderWindowError,
    WebRenderWindowErrorKind, WebRenderWindowFailureStage, WebRenderWindowResizeRequest,
    WebRenderWindowShutdownFailure, WebRenderWindowShutdownReport, WebRenderWindowStartupFailure,
};

pub use wild_buzzard_platform::{
    LogicalRect, PhysicalPoint, PhysicalSize, PixelFormat, ScaleFactor, SurfaceId, SurfaceNamespace,
};
