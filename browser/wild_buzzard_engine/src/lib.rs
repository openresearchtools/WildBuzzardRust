//! The deepest currently composable Wild Buzzard static-page pipeline.
//!
//! This crate fetches a numeric-loopback HTTP document, parses it into the
//! Rust DOM, computes author styles through imported Stylo, performs Rust
//! layout using shaped text metrics, compiles a real `WebRender` display list,
//! and reads an RGBA8 frame from the Linux headless renderer.
//!
//! The current renderer contracts deliberately keep shaped glyphs separate
//! from page decorations. [`RenderedStaticPage`] therefore returns the real
//! page-decoration frame and, when text exists, a separate real `WebRender`
//! glyph-proof frame. [`CompositionStatus`] makes that gap impossible to
//! mistake for a complete page render.

#![forbid(unsafe_code)]

mod error;
mod navigation;
mod pipeline;

pub use error::{PipelineError, PipelineStage};
pub use navigation::{
    CommandError, CommandErrorKind, CommandReceipt, EngineCommand, EngineEvent, EngineEventKind,
    EngineEventReceiver, EngineFrame, EngineFrameError, EngineLimits, EngineLimitsError,
    EngineShutdownStatus, EngineStartError, EventReceiveError, EventSequence, ExecutionFailure,
    ExecutionFailureKind, ExecutorOutput, ExecutorShutdownStatus, FrameComposition, FrameLease,
    FrameLeaseError, FrameLeaseId, FrameMetadata, MAX_NAVIGATION_URL_BYTES, NavigationEngine,
    NavigationExecutor, NavigationGeneration, NavigationId, NavigationRequest,
    NavigationRequestError, NavigationStage, PixelSize, Rgba8Metadata, TopLevelContextId,
    WorkerStopReason,
};
pub use pipeline::{
    CompositionStatus, EngineShutdownReport, PipelineEvidence, RenderedStaticPage,
    StaticPageConfig, StaticPageEngine, TextEvidence,
};
pub use wild_buzzard_net::{CancellationSource, CancellationToken};
pub use wild_buzzard_text::FontSourcePolicy;
