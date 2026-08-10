//! Direct Linux EGL window-surface ownership for Wild Buzzard.
//!
//! The crate consumes a winit window and keeps it behind a bounded presenter
//! API. Native display/window handles and unrestricted GL access never leave
//! this owner. Diagnostic rendering runs through callback-scoped capabilities;
//! normal scene presentation nests one hardware `WebRender` renderer in the
//! same thread-affine owner. EGL is verified non-current before its Rust
//! native-owner wrappers release.

#![cfg_attr(
    not(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu")),
    allow(unused)
)]
#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(not(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu")))]
compile_error!("wild_buzzard_linux_presenter supports only x86_64-unknown-linux-gnu");

mod browser_compositor;
mod contract;
mod egl_window;
mod webrender_window;
mod window_contract;
mod window_notifier;

pub use browser_compositor::{
    BrowserAddressSelection, BrowserChromeFocus, BrowserChromeGeometry, BrowserChromeRevision,
    BrowserChromeScene, BrowserChromeState, BrowserChromeTab, BrowserFrameReceipt,
    BrowserFrameRequest, BrowserHitTarget, BrowserHitTestResult, BrowserNavigationIdentity,
    BrowserPageIdentity, BrowserPageScene, BrowserPageSceneRevision, BrowserPageSnapshot,
    BrowserPageUpdate, BrowserPhysicalRect, BrowserTabIdentity,
    MAX_BROWSER_CHROME_DISPLAY_LIST_BYTES, MAX_BROWSER_CHROME_GLYPHS, MAX_BROWSER_CHROME_RUNS,
    MAX_BROWSER_CHROME_TABS, MAX_BROWSER_CHROME_TEXT_BYTES, MAX_BROWSER_CHROME_TEXTS,
    MAX_BROWSER_ROOT_DISPLAY_LIST_BYTES,
};
pub use contract::{
    DirectFrameRequest, DirectRenderError, DirectRenderer, LinuxPresentationBackend,
    MAX_PRESENTATION_DIMENSION, MAX_PRESENTATION_FRAMES, MAX_PRESENTATION_PIXEL_BYTES,
    MAX_PRESENTATION_PIXELS, PresentationError, PresentationErrorKind, PresentationFailureStage,
    PresentationLimits, PresentationRetentionReport, PresentationShutdownReport,
    PresentationStartupFailure, PresentationState, PresentationTeardownOutcome, SolidColor,
    SolidColorFrame, SwapSubmissionReceipt,
};
pub use egl_window::{
    DirectFrameTarget, LinuxPresentedWindow, LinuxPresenterCreationError, LinuxWindowPreparation,
    prepare_and_attach,
};
pub use webrender_window::WebRenderPresentedWindow;
pub use window_contract::{
    MAX_WINDOW_DISPLAY_LIST_BYTES, MAX_WINDOW_PENDING_TEXT_RUNS, MAX_WINDOW_SCENE_ITEMS,
    WINDOW_FRAME_TIMEOUT, WINDOW_SHUTDOWN_TIMEOUT, WebRenderSurfaceRevision,
    WebRenderSurfaceSnapshot, WebRenderTeardownEvidence, WebRenderWindowError,
    WebRenderWindowErrorKind, WebRenderWindowFailureStage, WebRenderWindowFrameReceipt,
    WebRenderWindowFrameRequest, WebRenderWindowLimits, WebRenderWindowResizeRequest,
    WebRenderWindowShutdownFailure, WebRenderWindowShutdownReport, WebRenderWindowStartupFailure,
    WebRenderWindowState,
};
