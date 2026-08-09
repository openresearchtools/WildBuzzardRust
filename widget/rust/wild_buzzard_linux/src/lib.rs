//! Rust-native Linux window ownership and event normalization.
//!
//! The crate deliberately exposes no winit types, native display handles, or
//! renderer objects. It creates one top-level window and maps its events onto
//! the pointer-free contracts in `wild_buzzard_platform`.

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
    ConfigError, LinuxBackendPreference, LinuxShellConfig, LinuxShellLimits,
    MAX_APPLICATION_ID_BYTES, MAX_DEVICE_CAPACITY, MAX_EVENT_CAPACITY, MAX_IME_BYTES,
    MAX_TITLE_BYTES, MAX_TOUCH_CAPACITY,
};
pub use event::{
    BoundedImeText, ControlError, ImeTextError, InputOrigin, LinuxBackend, LinuxShutdownReport,
    LinuxStopReason, LinuxWindowEvent,
};
pub use shell::{
    LinuxShellError, LinuxWakeHandle, LinuxWakeStatus, LinuxWindowControl, LinuxWindowHandler,
    LinuxWindowShell,
};

pub use wild_buzzard_platform::{LogicalRect, PixelFormat, SurfaceNamespace};
