//! A bounded Linux `x86_64` boundary that submits validated Wild Buzzard scenes
//! to the imported `WebRender` renderer and reads an owned RGBA8 frame back from
//! an offscreen EGL pbuffer.
//!
//! The context implementation is intentionally Linux-only. It first asks EGL
//! for a device-backed display, which does not need a window system. An
//! X11-default EGL pbuffer is a diagnostic fallback when device enumeration is
//! unavailable. No Wrench, SWGL, or independent rasterizer is linked.

#![cfg_attr(not(all(target_os = "linux", target_arch = "x86_64")), allow(unused))]
#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
compile_error!("wild_buzzard_headless supports only x86_64-unknown-linux-gnu");

mod error;
mod frame;
mod headless;
mod linux_egl;
mod notifier;

pub use error::{
    ContextAttempt, ContextBackend, ContextStep, FrameStage, HeadlessError, ResourceKind,
};
pub use frame::{FrameRequest, FrameSize, HeadlessLimits, RgbaFrame};
pub use headless::{HeadlessRenderer, LinuxGlInfo, ShutdownReport};
pub use wild_buzzard_text_webrender::{
    ShapedTextFrame, TextColor, TextOrigin, TextPipelineKey, TextRegistryStatistics,
};
