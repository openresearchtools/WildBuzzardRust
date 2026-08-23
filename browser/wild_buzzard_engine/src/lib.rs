//! The deepest currently composable Wild Buzzard page pipeline.
//!
//! This crate fetches a document through an explicitly separate numeric-loopback
//! or general HTTP/authenticated-HTTPS capability, parses it into the Rust DOM,
//! computes author styles through imported Stylo, performs Rust
//! layout using exact shaped text metrics, compiles a real `WebRender` display
//! list, resolves every finalized text fragment, and reads one composed RGBA8
//! frame from the Linux headless renderer. Its synchronous dynamic seam retains
//! one live DOM and can fully recompute a bounded exact-version mutation. The
//! opt-in `contained_inline_classic` gate additionally proves that one retained
//! Rust JavaScript owner can publish a post-script DOM into both headless and
//! native-presentation rendering; general-web product admission remains off.

#![forbid(unsafe_code)]

mod document_policy;
mod dynamic;
mod error;
mod navigation;
mod pipeline;
mod style_fetch;
mod style_policy;
mod style_resources;

pub use document_policy::{
    CapturedDocumentResponseMetadata, ContentTypeInput, CspFieldValue, DocumentPolicyError,
    DocumentPolicyField, DocumentPolicyLimit, MAX_CONTENT_TYPE_CHARSETS,
    MAX_CONTENT_TYPE_FIELD_BYTES, MAX_CONTENT_TYPE_FIELDS, MAX_CSP_BYTES, MAX_CSP_FIELD_BYTES,
    MAX_DOCUMENT_POLICY_INPUT_BYTES, MAX_ENFORCING_CSP_FIELDS,
    MAX_RECOGNIZED_REFERRER_POLICY_INPUTS, MAX_REFERRER_POLICY_FIELD_BYTES,
    MAX_REFERRER_POLICY_FIELDS, MAX_REFERRER_POLICY_TOKENS, MAX_REPORT_ONLY_CSP_FIELDS,
    MAX_SET_COOKIE_BYTES, MAX_SET_COOKIE_FIELDS, MalformedContentType, ParsedContentType,
    ReferrerPolicyInput, ReferrerPolicyMetadata, SetCookieMetadata,
};
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
#[cfg(feature = "contained_inline_classic")]
pub use pipeline::{RenderedContainedScriptPage, RenderedContainedScriptPresentationPage};
pub use style_fetch::{
    MAX_STYLE_FETCH_AGGREGATE_BODY_BYTES, MAX_STYLE_FETCH_AGGREGATE_HEADER_BYTES,
    MAX_STYLE_FETCH_CHUNK_LINE_BYTES, MAX_STYLE_FETCH_CONTENT_TYPE_BYTES,
    MAX_STYLE_FETCH_DIAGNOSTICS, MAX_STYLE_FETCH_DURATION, MAX_STYLE_FETCH_HTTP_EXCHANGES,
    MAX_STYLE_FETCH_REDIRECTS, MAX_STYLE_FETCH_RESPONSE_BODY_BYTES, MAX_STYLE_FETCH_RESPONSES,
    NonProductStyleFetchAuthority, NonProductStyleFetchOwner, StyleFetchAuthority,
    StyleFetchDiagnostic, StyleFetchDiagnosticKind, StyleFetchError, StyleFetchFailure,
    StyleFetchLimit, StyleFetchLimits, StyleFetchMime, StyleFetchNetworkFailure,
    StyleFetchOriginCleanliness, StyleFetchOwner, StyleFetchOwnerError, StyleFetchRejection,
    StyleFetchResponse, StyleFetchResponseHeaders, StyleFetchSet, StyleFetchTransportPolicy,
};
pub use style_policy::{
    MAX_STYLE_CSP_DIRECTIVES_PER_POLICY, MAX_STYLE_CSP_NONCE_BYTES, MAX_STYLE_CSP_POLICY_BYTES,
    MAX_STYLE_CSP_POLICY_MEMBERS, MAX_STYLE_CSP_POLICY_WORK, MAX_STYLE_CSP_SOURCE_EXPRESSIONS,
    MAX_STYLE_CSP_SOURCE_TOKEN_BYTES, StylePolicyAllocation, StylePolicyDecision, StylePolicyError,
    StylePolicyInput, StylePolicyLimit, StylePolicyResource, StylePolicySet,
    UnsupportedStyleSource, UnsupportedStyleSourceKind,
};
pub use style_resources::{
    MAX_STYLE_RESOURCE_ATTRIBUTE_BYTES, MAX_STYLE_RESOURCE_CANDIDATES,
    MAX_STYLE_RESOURCE_DIAGNOSTICS, MAX_STYLE_RESOURCE_URL_BYTES, StyleBaseCandidateEvidence,
    StyleBaseCandidateStatus, StyleResourceAllocation, StyleResourceAttribute,
    StyleResourceCandidateRecord, StyleResourceCandidateStatus, StyleResourceCounter,
    StyleResourceDiagnostic, StyleResourceDiagnosticKind, StyleResourceDiagnosticSubject,
    StyleResourceLimit, StyleResourcePlan, StyleResourcePlanError, StyleResourceRequestIdentity,
};
pub use wild_buzzard_dom::DocumentVersion;
pub use wild_buzzard_net::{
    CancellationSource, CancellationToken, CommittedResponseAuthority, GeneralWebConfig,
    IpAddressSpace, LocalNetworkAccessPermissions, LocalNetworkPermission, TrustStore,
};
#[cfg(feature = "contained_inline_classic")]
pub use wild_buzzard_script::{
    PRODUCT_SCRIPT_ADMISSION_ENABLED, ScriptBoundaryEvidence, ScriptDisposition,
    ScriptLoopCancellationSource, ScriptLoopCancellationToken, ScriptLoopError, ScriptLoopReport,
    ScriptedDocument, SkippedScriptReason,
};
pub use wild_buzzard_text::FontSourcePolicy;
