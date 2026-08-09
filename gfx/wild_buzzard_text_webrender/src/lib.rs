//! Renderer-side adapter from immutable shaped text to imported `WebRender`.
//!
//! This crate does not select fonts or shape text. It consumes the exact
//! [`wild_buzzard_text::ShapedText`] result shared with layout, registers its
//! exact font blobs in one renderer namespace, and emits the supplied glyph IDs
//! and positions. It deliberately has no dependency on DOM or layout.

#![forbid(unsafe_code)]

#[cfg(not(all(
    target_arch = "x86_64",
    target_os = "linux",
    target_env = "gnu",
    target_vendor = "unknown",
    target_pointer_width = "64",
    target_abi = ""
)))]
compile_error!("wild_buzzard_text_webrender supports only x86_64-unknown-linux-gnu");

mod contract;
mod error;
mod registry;

pub use contract::{
    RegistryRelease, ShapedTextFrame, TextColor, TextOrigin, TextPipelineKey,
    TextRegistryStatistics, TextRenderLimits, TextViewport,
};
pub use error::{InvalidRenderField, TextRenderError, TextRenderResource};
pub use registry::{PreparedTextFrame, TextFontRegistry};
