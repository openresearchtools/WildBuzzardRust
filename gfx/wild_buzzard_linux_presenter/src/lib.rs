//! Direct Linux EGL window-surface ownership for Wild Buzzard.
//!
//! The crate consumes a winit window and keeps it behind a bounded presenter
//! API. Native display/window handles and unrestricted GL access never leave
//! this owner. A renderer runs only through callback-scoped capabilities, and
//! EGL is verified non-current before its Rust native-owner wrappers release.

#![cfg_attr(
    not(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu")),
    allow(unused)
)]
#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(not(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu")))]
compile_error!("wild_buzzard_linux_presenter supports only x86_64-unknown-linux-gnu");

mod contract;
mod egl_window;

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
