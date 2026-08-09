//! Bounded Linux text selection, Unicode analysis, and shaping for Wild Buzzard.

#![forbid(unsafe_code)]

#[cfg(not(all(
    target_arch = "x86_64",
    target_os = "linux",
    target_env = "gnu",
    target_vendor = "unknown",
    target_pointer_width = "64",
    target_abi = ""
)))]
compile_error!("wild_buzzard_text supports only x86_64-unknown-linux-gnu");

mod contract;
mod error;
mod limits;
mod system;

pub use contract::{
    CacheStatistics, FontFace, FontFaceId, FontFamily, FontFeature, FontStretch, FontStyle,
    FontSynthesis, FontVariation, FontWeight, GenericFamily, GlyphCluster, LineHeight,
    LineHeightProvenance, PositionedGlyph, RunDirection, RunMetrics, ScriptTag, ShapedRun,
    ShapedText, TextDirection, TextMetrics, TextRequest,
};
pub use error::{InvalidTextField, TextError, TextResource};
pub use limits::TextLimits;
pub use system::{FontSourcePolicy, TextShutdownReport, TextSystem};
