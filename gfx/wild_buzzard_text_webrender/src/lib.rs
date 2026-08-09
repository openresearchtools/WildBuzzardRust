//! Renderer-side adapter from immutable shaped text to imported `WebRender`.
//!
//! This crate does not select fonts or shape text. It consumes an exact
//! [`wild_buzzard_text::ShapedText`] result, registers its font blobs in one
//! renderer namespace, and emits the supplied glyph IDs and positions. Each
//! allocation carries a DOM-owned [`wild_buzzard_dom::DocumentVersion`]
//! and canonical pending-text index for exact publication identity. Scene
//! placement remains owned and validated by the renderer crate; this adapter
//! never inspects DOM or layout objects.

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
    RegistryRelease, ShapedSceneText, ShapedTextFrame, TextColor, TextOrigin, TextPipelineKey,
    TextRegistryStatistics, TextRenderLimits, TextViewport,
};
pub use error::{InvalidRenderField, TextRenderError, TextRenderResource};
pub use registry::{
    PreparedSceneGlyphRun, PreparedSceneText, PreparedSceneTextEntry, PreparedTextFrame,
    TextFontRegistry,
};
