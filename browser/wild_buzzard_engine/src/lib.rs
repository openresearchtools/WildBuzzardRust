//! The deepest currently composable Wild Buzzard page pipeline.
//!
//! This crate fetches a document through an explicitly separate numeric-loopback
//! or general HTTP/authenticated-HTTPS capability, parses it into the Rust DOM,
//! computes author styles through imported Stylo, performs Rust
//! layout using exact shaped text metrics, compiles a real `WebRender` display
//! list, resolves every finalized text fragment, and reads one composed RGBA8
//! frame from the Linux headless renderer. Its synchronous dynamic seam retains
//! one live DOM and can fully recompute a bounded exact-version mutation; it is
//! not a JavaScript or event-loop integration.

#![forbid(unsafe_code)]

mod dynamic;
mod error;
mod navigation;
mod pipeline;

pub use dynamic::{
    DocumentMutationCommit, DocumentUpdateError, DocumentUpdateRejection, DynamicRenderEvidence,
    LiveDocumentPage, RenderedDocumentUpdate, RenderedLiveDocument,
};
pub use error::{PipelineError, PipelineStage, RedirectLocationFailure};
pub use navigation::{
    CommandError, CommandErrorKind, CommandReceipt, DocumentLoadProof, DocumentOperationFailure,
    DocumentOperationId, EngineCommand, EngineEvent, EngineEventKind, EngineEventReceiver,
    EngineFrame, EngineFrameError, EngineLimits, EngineLimitsError, EngineShutdownStatus,
    EngineStartError, EventReceiveError, EventSequence, ExecutionFailure, ExecutionFailureKind,
    ExecutorDocumentMutation, ExecutorDocumentRerender, ExecutorOutput, ExecutorShutdownStatus,
    FrameLease, FrameLeaseError, FrameLeaseId, FrameMetadata, FrameOutputMetadata,
    MAX_NAVIGATION_URL_BYTES, MutationResultLease, MutationResultLeaseError, MutationResultLeaseId,
    NavigationAlpn, NavigationCommit, NavigationCommitError, NavigationCommitMetadata,
    NavigationCommitValidationError, NavigationConnectionSecurity, NavigationEngine,
    NavigationExecutor, NavigationGeneration, NavigationId, NavigationNetworkCapability,
    NavigationRequest, NavigationRequestError, NavigationStage, NavigationTlsVersion, PixelSize,
    Rgba8Metadata, TopLevelContextId, WorkerStopReason,
};
pub use pipeline::{
    EngineShutdownReport, MAX_TOP_LEVEL_REDIRECTS, PipelineEvidence, PresentationScene,
    PresentationSceneMetadata, PresentationSceneRevision, RenderedPresentationPage,
    RenderedStaticPage, StaticPageConfig, StaticPageEngine, TextEvidence,
};
pub use wild_buzzard_dom::DocumentVersion;
pub use wild_buzzard_net::{CancellationSource, CancellationToken, GeneralWebConfig, TrustStore};
pub use wild_buzzard_text::FontSourcePolicy;
