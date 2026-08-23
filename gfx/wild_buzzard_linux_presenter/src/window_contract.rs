#![forbid(unsafe_code)]

use std::error::Error;
use std::fmt;
use std::num::NonZeroU64;
use std::time::Duration;

use wild_buzzard_dom::{DocumentId, DocumentVersion};
use wild_buzzard_platform::{PhysicalSize, SurfaceDescriptor, SurfaceId};
use wild_buzzard_renderer::PipelineKey;

use crate::{
    LinuxPresentationCapabilities, PresentationError, PresentationErrorKind,
    PresentationFailureStage, PresentationLimits, PresentationShutdownReport,
    PresentationTeardownOutcome,
};

/// Maximum immutable scene items accepted by one window transaction.
pub const MAX_WINDOW_SCENE_ITEMS: usize = 1_000_000;
/// Maximum pending shaped-text records accepted by one window transaction.
pub const MAX_WINDOW_PENDING_TEXT_RUNS: usize = 100_000;
/// Maximum serialized display-list bytes accepted by one window transaction.
pub const MAX_WINDOW_DISPLAY_LIST_BYTES: usize = 128 << 20;
/// Total deadline checked at asynchronous and returned synchronous frame boundaries.
pub const WINDOW_FRAME_TIMEOUT: Duration = Duration::from_secs(10);
/// Deadline for the one asynchronous `WebRender` backend-shutdown acknowledgement.
pub const WINDOW_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

const INVALID_WEBRENDER_EPOCH: u32 = u32::MAX;
const MAX_ERROR_DETAIL_BYTES: usize = 2_048;

/// Fixed, caller-nonenlargeable resource and notification limits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WebRenderWindowLimits {
    max_scene_items: usize,
    max_pending_text_runs: usize,
    max_display_list_bytes: usize,
    frame_timeout: Duration,
    shutdown_timeout: Duration,
}

impl Default for WebRenderWindowLimits {
    fn default() -> Self {
        Self {
            max_scene_items: MAX_WINDOW_SCENE_ITEMS,
            max_pending_text_runs: MAX_WINDOW_PENDING_TEXT_RUNS,
            max_display_list_bytes: MAX_WINDOW_DISPLAY_LIST_BYTES,
            frame_timeout: WINDOW_FRAME_TIMEOUT,
            shutdown_timeout: WINDOW_SHUTDOWN_TIMEOUT,
        }
    }
}

impl WebRenderWindowLimits {
    /// Maximum validated scene-item count.
    #[must_use]
    pub const fn max_scene_items(self) -> usize {
        self.max_scene_items
    }

    /// Maximum pending shaped-text count.
    #[must_use]
    pub const fn max_pending_text_runs(self) -> usize {
        self.max_pending_text_runs
    }

    /// Maximum serialized display-list byte count.
    #[must_use]
    pub const fn max_display_list_bytes(self) -> usize {
        self.max_display_list_bytes
    }

    /// One total deadline checked throughout frame build, notification, render, and swap.
    #[must_use]
    pub const fn frame_timeout(self) -> Duration {
        self.frame_timeout
    }

    /// Backend-shutdown acknowledgement deadline.
    #[must_use]
    pub const fn shutdown_timeout(self) -> Duration {
        self.shutdown_timeout
    }

    #[cfg(test)]
    pub(crate) const fn for_test(
        max_scene_items: usize,
        max_pending_text_runs: usize,
        max_display_list_bytes: usize,
        frame_timeout: Duration,
        shutdown_timeout: Duration,
    ) -> Self {
        Self {
            max_scene_items,
            max_pending_text_runs,
            max_display_list_bytes,
            frame_timeout,
            shutdown_timeout,
        }
    }
}

/// Monotonic identity for one exact native surface configuration.
///
/// Values are created only by the owning presenter. Replaying an older value
/// after resize, suspension, resume, or scale change is rejected even when the
/// physical extent later returns to the same dimensions.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WebRenderSurfaceRevision(NonZeroU64);

impl WebRenderSurfaceRevision {
    const INITIAL: Self = Self(NonZeroU64::MIN);

    /// Numeric revision for diagnostics and typed transport.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }

    fn checked_next(self) -> Option<Self> {
        self.0
            .get()
            .checked_add(1)
            .and_then(NonZeroU64::new)
            .map(Self)
    }

    #[cfg(test)]
    const fn from_nonzero_for_test(value: NonZeroU64) -> Self {
        Self(value)
    }
}

/// Exact value-only native target identity supplied to frame and resize calls.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WebRenderSurfaceSnapshot {
    descriptor: SurfaceDescriptor,
    revision: WebRenderSurfaceRevision,
    capabilities: LinuxPresentationCapabilities,
}

impl WebRenderSurfaceSnapshot {
    #[cfg(test)]
    pub(crate) const fn initial(descriptor: SurfaceDescriptor) -> Self {
        Self::initial_with_capabilities(descriptor, LinuxPresentationCapabilities::STRICT_HARDWARE)
    }

    pub(crate) const fn initial_with_capabilities(
        descriptor: SurfaceDescriptor,
        capabilities: LinuxPresentationCapabilities,
    ) -> Self {
        Self {
            descriptor,
            revision: WebRenderSurfaceRevision::INITIAL,
            capabilities,
        }
    }

    /// Current generational native surface identity and metadata.
    #[must_use]
    pub const fn descriptor(self) -> SurfaceDescriptor {
        self.descriptor
    }

    /// Current non-reusing configuration revision.
    #[must_use]
    pub const fn revision(self) -> WebRenderSurfaceRevision {
        self.revision
    }

    /// Exact immutable EGL/GL profile selected before renderer startup.
    #[must_use]
    pub const fn capabilities(self) -> LinuxPresentationCapabilities {
        self.capabilities
    }

    /// Exact native surface identity.
    #[must_use]
    pub const fn surface(self) -> SurfaceId {
        self.descriptor.id
    }

    /// Exact current physical extent.
    #[must_use]
    pub const fn size(self) -> PhysicalSize {
        self.descriptor.size
    }
}

/// Exact immutable scene transaction requested for one native-window frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WebRenderWindowFrameRequest {
    surface: WebRenderSurfaceSnapshot,
    document_version: DocumentVersion,
    pipeline: PipelineKey,
    epoch: u32,
    sequence: u64,
}

impl WebRenderWindowFrameRequest {
    /// Binds an immutable scene identity to an exact native surface revision.
    #[must_use]
    pub const fn new(
        surface: WebRenderSurfaceSnapshot,
        document_version: DocumentVersion,
        pipeline: PipelineKey,
        epoch: u32,
        sequence: u64,
    ) -> Self {
        Self {
            surface,
            document_version,
            pipeline,
            epoch,
            sequence,
        }
    }

    /// Exact native target snapshot.
    #[must_use]
    pub const fn surface_snapshot(self) -> WebRenderSurfaceSnapshot {
        self.surface
    }

    /// Exact immutable document identity and revision.
    #[must_use]
    pub const fn document_version(self) -> DocumentVersion {
        self.document_version
    }

    /// Exact caller-owned pipeline identity.
    #[must_use]
    pub const fn pipeline(self) -> PipelineKey {
        self.pipeline
    }

    /// Monotonic `WebRender` epoch; `u32::MAX` is reserved and rejected.
    #[must_use]
    pub const fn epoch(self) -> u32 {
        self.epoch
    }

    /// Nonzero, monotonic native swap sequence.
    #[must_use]
    pub const fn sequence(self) -> u64 {
        self.sequence
    }
}

/// Exact stale-checked native surface transition.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WebRenderWindowResizeRequest {
    expected: WebRenderSurfaceSnapshot,
    size: PhysicalSize,
}

impl WebRenderWindowResizeRequest {
    /// Requests a transition from the exact current snapshot to `size`.
    #[must_use]
    pub const fn new(expected: WebRenderSurfaceSnapshot, size: PhysicalSize) -> Self {
        Self { expected, size }
    }

    /// Snapshot which must still be current.
    #[must_use]
    pub const fn expected(self) -> WebRenderSurfaceSnapshot {
        self.expected
    }

    /// Requested physical extent. A zero axis enters suspension.
    #[must_use]
    pub const fn size(self) -> PhysicalSize {
        self.size
    }
}

/// Stable stage at which the WebRender-to-window boundary failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WebRenderWindowFailureStage {
    /// Validate exact surface, document, pipeline, epoch, sequence, or resource bounds.
    ValidateRequest,
    /// Validate and compose the immutable scene/text transaction.
    ComposeScene,
    /// Create `WebRender` on the exact presenter's current context and surface.
    InitializeRenderer,
    /// Send one atomic resources/display-list/document-view/frame transaction.
    SubmitTransaction,
    /// Await the backend's `FrameBuilt` checkpoint.
    AwaitFrameBuilt,
    /// Await and validate the exact frame-ready notification.
    AwaitFrameReady,
    /// Make the presenter's exact context/surface current and validate GL state.
    PrepareNativeFrame,
    /// Ingest backend work on the renderer owner thread.
    UpdateRenderer,
    /// Verify the exact pipeline epoch published by the backend.
    VerifyEpoch,
    /// Render `WebRender`'s frame directly into the native default framebuffer.
    RenderFrame,
    /// Await the renderer's `FrameRendered` checkpoint.
    AwaitFrameRendered,
    /// Submit the rendered native back buffer through EGL.
    SwapBuffers,
    /// Resize or zero-size the checked EGL surface before publishing a new revision.
    ResizeSurface,
    /// Remove the EGL window surface while retaining renderer/context ownership.
    SuspendSurface,
    /// Recreate the exact EGL window surface before publishing a new revision.
    ResumeSurface,
    /// Release renderer-owned font/document/backend resources.
    ShutdownBackend,
    /// Delete renderer-owned GL resources while the exact context is current.
    DeinitializeRenderer,
    /// Release the nested presenter after `WebRender` ownership is gone.
    ShutdownPresenter,
    /// Resolve a physical point against the exact last successful browser composition.
    HitTest,
}

/// Stable failure class at the WebRender-to-window boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WebRenderWindowErrorKind {
    /// An unsupported or internally inconsistent contract was supplied.
    Contract,
    /// A fixed caller-nonenlargeable resource bound was exceeded.
    ResourceLimit,
    /// A foreign generational surface identity was supplied.
    SurfaceMismatch,
    /// A stale surface configuration revision was supplied.
    StaleSurfaceRevision,
    /// Physical extent or immutable scene viewport differed from the exact target.
    SizeMismatch,
    /// Scene/text identity differed from the exact requested document version.
    DocumentMismatch,
    /// A same-document revision moved backwards.
    RevisionRegressed,
    /// The consumed compiled scene carried a different pipeline identity.
    PipelineMismatch,
    /// The `WebRender` epoch was reserved, repeated, or nonmonotonic.
    Epoch,
    /// The native swap sequence was zero, repeated, exhausted, or nonmonotonic.
    FrameSequence,
    /// The zero-sized or explicitly suspended surface cannot accept a frame.
    Suspended,
    /// Immutable scene validation or composition rejected the transaction.
    Scene,
    /// Shaped-text validation or font-resource staging rejected the transaction.
    Text,
    /// `WebRender` dropped the transaction before a requested checkpoint.
    TransactionDropped,
    /// A fixed-capacity notification counter or channel overflowed.
    NotificationOverflow,
    /// Work did not complete under the one total frame deadline.
    Timeout,
    /// The `WebRender` backend channel disconnected or emitted an unauthorized event.
    Backend,
    /// The renderer rejected initialization, update, rendering, or exact epoch publication.
    Renderer,
    /// EGL or GL reported a context/device loss.
    DeviceLost,
    /// EGL, GL, or the native surface rejected an operation.
    Native,
    /// The native presenter contradicted a request already admitted by the outer contract.
    InternalDrift,
    /// A panic escaped an imported renderer/native call and was contained.
    Panic,
    /// A prior terminal fault permanently closed admission.
    TerminalState,
    /// Page/chrome pixels are not authoritative for the current surface or receipt.
    StaleComposition,
}

/// Bounded stable diagnostic from the WebRender-to-window boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WebRenderWindowError {
    stage: WebRenderWindowFailureStage,
    kind: WebRenderWindowErrorKind,
    detail: String,
}

impl WebRenderWindowError {
    pub(crate) fn new(
        stage: WebRenderWindowFailureStage,
        kind: WebRenderWindowErrorKind,
        detail: impl fmt::Display,
    ) -> Self {
        Self {
            stage,
            kind,
            detail: bounded_detail(&detail.to_string()),
        }
    }

    pub(crate) fn presentation(
        stage: WebRenderWindowFailureStage,
        error: &PresentationError,
    ) -> Self {
        let kind = match error.kind() {
            PresentationErrorKind::ContextLost => WebRenderWindowErrorKind::DeviceLost,
            PresentationErrorKind::Driver
            | PresentationErrorKind::OutOfMemory
            | PresentationErrorKind::DiagnosticMismatch => WebRenderWindowErrorKind::Native,
            PresentationErrorKind::SurfaceMismatch => WebRenderWindowErrorKind::SurfaceMismatch,
            PresentationErrorKind::SizeMismatch => WebRenderWindowErrorKind::SizeMismatch,
            PresentationErrorKind::FrameSequence => WebRenderWindowErrorKind::FrameSequence,
            PresentationErrorKind::Suspended => WebRenderWindowErrorKind::Suspended,
            PresentationErrorKind::ResourceLimit => WebRenderWindowErrorKind::ResourceLimit,
            PresentationErrorKind::TerminalState => WebRenderWindowErrorKind::TerminalState,
            PresentationErrorKind::UnsupportedContract
            | PresentationErrorKind::UnsupportedCapability
            | PresentationErrorKind::RendererRejected => WebRenderWindowErrorKind::Contract,
        };
        Self::new(
            stage,
            kind,
            format_args!("native {:?}: {}", error.stage(), error.detail()),
        )
    }

    /// Exact failing stage.
    #[must_use]
    pub const fn stage(&self) -> WebRenderWindowFailureStage {
        self.stage
    }

    /// Stable failure class.
    #[must_use]
    pub const fn kind(&self) -> WebRenderWindowErrorKind {
        self.kind
    }

    /// Bounded diagnostic text; never use it for control flow.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }

    /// Whether the current renderer/presenter may not accept another transaction.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        if matches!(
            (self.stage, self.kind),
            (
                WebRenderWindowFailureStage::ComposeScene,
                WebRenderWindowErrorKind::Timeout
            )
        ) {
            return false;
        }
        matches!(
            self.kind,
            WebRenderWindowErrorKind::TransactionDropped
                | WebRenderWindowErrorKind::NotificationOverflow
                | WebRenderWindowErrorKind::Timeout
                | WebRenderWindowErrorKind::Backend
                | WebRenderWindowErrorKind::Renderer
                | WebRenderWindowErrorKind::DeviceLost
                | WebRenderWindowErrorKind::Native
                | WebRenderWindowErrorKind::InternalDrift
                | WebRenderWindowErrorKind::Panic
                | WebRenderWindowErrorKind::TerminalState
        )
    }
}

impl fmt::Display for WebRenderWindowError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{:?}/{:?}: {}",
            self.stage, self.kind, self.detail
        )
    }
}

impl Error for WebRenderWindowError {}

/// Externally visible lifecycle of the internally owned window renderer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WebRenderWindowState {
    /// A nonzero exact native surface may accept one transaction at a time.
    Active,
    /// The renderer/context remain owned but no EGL window surface exists.
    Suspended,
    /// A terminal renderer, backend, GL, or native fault closed admission.
    Lost(WebRenderWindowFailureStage),
    /// Explicit teardown completed.
    Shutdown,
}

/// Evidence returned only after backend build, renderer draw, and EGL swap all succeed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WebRenderWindowFrameReceipt {
    request: WebRenderWindowFrameRequest,
    backend_publish_id: u64,
    rgba8_byte_equivalent: u64,
}

impl WebRenderWindowFrameReceipt {
    pub(crate) const fn new(
        request: WebRenderWindowFrameRequest,
        backend_publish_id: u64,
        rgba8_byte_equivalent: u64,
    ) -> Self {
        Self {
            request,
            backend_publish_id,
            rgba8_byte_equivalent,
        }
    }

    /// Exact immutable request whose three submission stages completed.
    #[must_use]
    pub const fn request(self) -> WebRenderWindowFrameRequest {
        self.request
    }

    /// Exact immutable profile used for this transaction and native swap.
    #[must_use]
    pub const fn capabilities(self) -> LinuxPresentationCapabilities {
        self.request.surface_snapshot().capabilities()
    }

    /// `WebRender` backend publish identity observed for this transaction.
    #[must_use]
    pub const fn backend_publish_id(self) -> u64 {
        self.backend_publish_id
    }

    /// Bounded RGBA8-equivalent native surface bytes.
    #[must_use]
    pub const fn rgba8_byte_equivalent(self) -> u64 {
        self.rgba8_byte_equivalent
    }

    /// The exact transaction reached `WebRender`'s `FrameBuilt` checkpoint.
    #[must_use]
    pub const fn backend_transaction_built(self) -> bool {
        true
    }

    /// `Renderer::render` completed and its `FrameRendered` checkpoint fired.
    #[must_use]
    pub const fn renderer_frame_submitted(self) -> bool {
        true
    }

    /// EGL accepted `swap_buffers` for the exact target revision and sequence.
    #[must_use]
    pub const fn egl_swap_submitted(self) -> bool {
        true
    }

    /// Neither EGL nor `WebRender` acknowledges desktop-compositor display.
    #[must_use]
    pub const fn desktop_compositor_acknowledged(self) -> bool {
        false
    }
}

/// Proof state for one optional stage of renderer/backend teardown.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WebRenderTeardownEvidence {
    /// The owned stage existed and its required completion signal was observed.
    Confirmed,
    /// The stage never existed, so no completion claim is applicable.
    NotApplicable,
    /// The stage may exist but its completion could not be proved.
    Unknown,
}

impl WebRenderTeardownEvidence {
    /// Whether positive completion evidence exists.
    #[must_use]
    pub const fn is_confirmed(self) -> bool {
        matches!(self, Self::Confirmed)
    }
}

/// Successful ordered shutdown evidence for `WebRender` followed by the presenter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WebRenderWindowShutdownReport {
    backend_shutdown: WebRenderTeardownEvidence,
    renderer_deinitialization: WebRenderTeardownEvidence,
    text_font_templates_released: usize,
    text_font_instances_released: usize,
    text_font_bytes_released: usize,
    presentation: PresentationShutdownReport,
}

impl WebRenderWindowShutdownReport {
    #[allow(clippy::too_many_arguments)]
    pub(crate) const fn new(
        backend_shutdown: WebRenderTeardownEvidence,
        renderer_deinitialization: WebRenderTeardownEvidence,
        text_font_templates_released: usize,
        text_font_instances_released: usize,
        text_font_bytes_released: usize,
        presentation: PresentationShutdownReport,
    ) -> Self {
        Self {
            backend_shutdown,
            renderer_deinitialization,
            text_font_templates_released,
            text_font_instances_released,
            text_font_bytes_released,
            presentation,
        }
    }

    /// Exact backend-worker shutdown evidence.
    #[must_use]
    pub const fn backend_shutdown(self) -> WebRenderTeardownEvidence {
        self.backend_shutdown
    }

    /// Exact renderer GL-resource deinitialization evidence.
    #[must_use]
    pub const fn renderer_deinitialization(self) -> WebRenderTeardownEvidence {
        self.renderer_deinitialization
    }

    /// Whether backend shutdown is positively confirmed.
    ///
    /// `NotApplicable` and `Unknown` both map to false. Prefer
    /// [`Self::backend_shutdown`] when the distinction matters.
    #[must_use]
    pub const fn backend_acknowledged(self) -> bool {
        self.backend_shutdown.is_confirmed()
    }

    /// Whether renderer deinitialization is positively confirmed.
    ///
    /// `NotApplicable` and `Unknown` both map to false. Prefer
    /// [`Self::renderer_deinitialization`] when the distinction matters.
    #[must_use]
    pub const fn renderer_deinitialized(self) -> bool {
        self.renderer_deinitialization.is_confirmed()
    }

    /// Raw font templates explicitly deleted in the release transaction.
    #[must_use]
    pub const fn text_font_templates_released(self) -> usize {
        self.text_font_templates_released
    }

    /// Font instances explicitly deleted in the release transaction.
    #[must_use]
    pub const fn text_font_instances_released(self) -> usize {
        self.text_font_instances_released
    }

    /// Copied raw font bytes retired with those templates.
    #[must_use]
    pub const fn text_font_bytes_released(self) -> usize {
        self.text_font_bytes_released
    }

    /// Nested native-presenter wrapper-release evidence.
    #[must_use]
    pub const fn presentation(self) -> PresentationShutdownReport {
        self.presentation
    }

    /// Exact immutable profile retired after renderer shutdown.
    #[must_use]
    pub const fn capabilities(self) -> LinuxPresentationCapabilities {
        self.presentation.capabilities()
    }
}

/// Teardown evidence paired with the first renderer/backend/native shutdown error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WebRenderWindowShutdownFailure {
    primary: WebRenderWindowError,
    backend_shutdown: WebRenderTeardownEvidence,
    renderer_deinitialization: WebRenderTeardownEvidence,
    presentation: PresentationTeardownOutcome,
}

impl WebRenderWindowShutdownFailure {
    pub(crate) const fn new(
        primary: WebRenderWindowError,
        backend_shutdown: WebRenderTeardownEvidence,
        renderer_deinitialization: WebRenderTeardownEvidence,
        presentation: PresentationTeardownOutcome,
    ) -> Self {
        Self {
            primary,
            backend_shutdown,
            renderer_deinitialization,
            presentation,
        }
    }

    /// First authoritative shutdown failure.
    #[must_use]
    pub const fn primary(&self) -> &WebRenderWindowError {
        &self.primary
    }

    /// Exact backend-worker shutdown evidence.
    #[must_use]
    pub const fn backend_shutdown(&self) -> WebRenderTeardownEvidence {
        self.backend_shutdown
    }

    /// Exact renderer GL-resource deinitialization evidence.
    #[must_use]
    pub const fn renderer_deinitialization(&self) -> WebRenderTeardownEvidence {
        self.renderer_deinitialization
    }

    /// Whether backend shutdown is positively confirmed.
    ///
    /// `NotApplicable` and `Unknown` both map to false.
    #[must_use]
    pub const fn backend_acknowledged(&self) -> bool {
        self.backend_shutdown.is_confirmed()
    }

    /// Whether renderer deinitialization is positively confirmed.
    ///
    /// `NotApplicable` and `Unknown` both map to false.
    #[must_use]
    pub const fn renderer_deinitialized(&self) -> bool {
        self.renderer_deinitialization.is_confirmed()
    }

    /// Exact nested presenter release or fail-closed retention outcome.
    #[must_use]
    pub const fn presentation(&self) -> PresentationTeardownOutcome {
        self.presentation
    }

    /// Exact immutable profile whose renderer/native teardown failed.
    #[must_use]
    pub const fn capabilities(&self) -> LinuxPresentationCapabilities {
        self.presentation.capabilities()
    }
}

impl fmt::Display for WebRenderWindowShutdownFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}; backend_shutdown={:?}; renderer_deinitialization={:?}; presentation={:?}",
            self.primary, self.backend_shutdown, self.renderer_deinitialization, self.presentation
        )
    }
}

impl Error for WebRenderWindowShutdownFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.primary)
    }
}

/// Recoverable initialization failure after consuming the exact native presenter owner.
///
/// This value is never constructed for a constructor thread error, constructor
/// panic, or API-creation panic because worker termination is unprovable in
/// those cases and the owning process aborts fail-closed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WebRenderWindowStartupFailure {
    primary: WebRenderWindowError,
    teardown: Result<WebRenderWindowShutdownReport, WebRenderWindowShutdownFailure>,
}

impl WebRenderWindowStartupFailure {
    pub(crate) const fn new(
        primary: WebRenderWindowError,
        teardown: Result<WebRenderWindowShutdownReport, WebRenderWindowShutdownFailure>,
    ) -> Self {
        Self { primary, teardown }
    }

    /// Original renderer initialization failure.
    #[must_use]
    pub const fn primary(&self) -> &WebRenderWindowError {
        &self.primary
    }

    /// Exact ordered teardown result for the consumed partial owner.
    pub const fn teardown(
        &self,
    ) -> &Result<WebRenderWindowShutdownReport, WebRenderWindowShutdownFailure> {
        &self.teardown
    }

    /// Exact native release-or-retention outcome, independent of renderer cleanup.
    #[must_use]
    pub const fn presentation_teardown(&self) -> PresentationTeardownOutcome {
        match &self.teardown {
            Ok(report) => PresentationTeardownOutcome::WrappersReleased(report.presentation()),
            Err(failure) => failure.presentation(),
        }
    }

    /// Exact immutable profile consumed by the failed renderer startup.
    #[must_use]
    pub const fn capabilities(&self) -> LinuxPresentationCapabilities {
        self.presentation_teardown().capabilities()
    }

    /// Consumes the failure without discarding either result.
    pub fn into_parts(
        self,
    ) -> (
        WebRenderWindowError,
        Result<WebRenderWindowShutdownReport, WebRenderWindowShutdownFailure>,
    ) {
        (self.primary, self.teardown)
    }
}

impl fmt::Display for WebRenderWindowStartupFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}; partial-owner teardown: {:?}",
            self.primary, self.teardown
        )
    }
}

impl Error for WebRenderWindowStartupFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.primary)
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct WebRenderWindowContract {
    snapshot: WebRenderSurfaceSnapshot,
    limits: WebRenderWindowLimits,
    state: WebRenderWindowState,
    last_document_version: Option<DocumentVersion>,
    last_epoch: Option<u32>,
    last_pipeline: Option<PipelineKey>,
    last_sequence: Option<u64>,
    submitted_frames: u64,
}

impl WebRenderWindowContract {
    #[cfg(test)]
    pub(crate) const fn new(descriptor: SurfaceDescriptor) -> Self {
        Self::new_with_capabilities(descriptor, LinuxPresentationCapabilities::STRICT_HARDWARE)
    }

    pub(crate) const fn new_with_capabilities(
        descriptor: SurfaceDescriptor,
        capabilities: LinuxPresentationCapabilities,
    ) -> Self {
        let state = if descriptor.size.width == 0 || descriptor.size.height == 0 {
            WebRenderWindowState::Suspended
        } else {
            WebRenderWindowState::Active
        };
        Self {
            snapshot: WebRenderSurfaceSnapshot::initial_with_capabilities(descriptor, capabilities),
            limits: WebRenderWindowLimits {
                max_scene_items: MAX_WINDOW_SCENE_ITEMS,
                max_pending_text_runs: MAX_WINDOW_PENDING_TEXT_RUNS,
                max_display_list_bytes: MAX_WINDOW_DISPLAY_LIST_BYTES,
                frame_timeout: WINDOW_FRAME_TIMEOUT,
                shutdown_timeout: WINDOW_SHUTDOWN_TIMEOUT,
            },
            state,
            last_document_version: None,
            last_epoch: None,
            last_pipeline: None,
            last_sequence: None,
            submitted_frames: 0,
        }
    }

    #[cfg(test)]
    pub(crate) const fn with_limits(
        descriptor: SurfaceDescriptor,
        limits: WebRenderWindowLimits,
    ) -> Self {
        let mut contract = Self::new(descriptor);
        contract.limits = limits;
        contract
    }

    pub(crate) const fn limits(&self) -> WebRenderWindowLimits {
        self.limits
    }

    pub(crate) const fn snapshot(&self) -> WebRenderSurfaceSnapshot {
        self.snapshot
    }

    pub(crate) const fn state(&self) -> WebRenderWindowState {
        self.state
    }

    pub(crate) const fn last_pipeline(&self) -> Option<PipelineKey> {
        self.last_pipeline
    }

    pub(crate) const fn last_epoch(&self) -> Option<u32> {
        self.last_epoch
    }

    pub(crate) const fn last_sequence(&self) -> Option<u64> {
        self.last_sequence
    }

    pub(crate) const fn submitted_frames(&self) -> u64 {
        self.submitted_frames
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn validate_submission(
        &self,
        request: WebRenderWindowFrameRequest,
        scene_document_version: DocumentVersion,
        scene_width: u32,
        scene_height: u32,
        scene_items: usize,
        pending_text_runs: usize,
        display_list_bytes: usize,
    ) -> Result<(), WebRenderWindowError> {
        self.validate_live_snapshot(
            request.surface,
            WebRenderWindowFailureStage::ValidateRequest,
        )?;
        if self.state == WebRenderWindowState::Suspended {
            return Err(WebRenderWindowError::new(
                WebRenderWindowFailureStage::ValidateRequest,
                WebRenderWindowErrorKind::Suspended,
                "zero-sized or suspended surface cannot accept a WebRender frame",
            ));
        }
        if scene_document_version != request.document_version {
            return Err(WebRenderWindowError::new(
                WebRenderWindowFailureStage::ValidateRequest,
                WebRenderWindowErrorKind::DocumentMismatch,
                format_args!(
                    "scene document {} revision {} differs from requested document {} revision {}",
                    scene_document_version.document_id().get(),
                    scene_document_version.revision(),
                    request.document_version.document_id().get(),
                    request.document_version.revision()
                ),
            ));
        }
        if let Some(previous) = self.last_document_version
            && previous.document_id() == scene_document_version.document_id()
            && scene_document_version.revision() < previous.revision()
        {
            return Err(revision_regressed(previous, scene_document_version));
        }
        if (scene_width, scene_height)
            != (request.surface.size().width, request.surface.size().height)
        {
            return Err(WebRenderWindowError::new(
                WebRenderWindowFailureStage::ValidateRequest,
                WebRenderWindowErrorKind::SizeMismatch,
                format_args!(
                    "scene viewport {scene_width}x{scene_height} differs from exact native target {}x{}",
                    request.surface.size().width,
                    request.surface.size().height
                ),
            ));
        }
        if request.epoch == INVALID_WEBRENDER_EPOCH
            || self
                .last_epoch
                .is_some_and(|previous| request.epoch <= previous)
        {
            return Err(WebRenderWindowError::new(
                WebRenderWindowFailureStage::ValidateRequest,
                WebRenderWindowErrorKind::Epoch,
                "WebRender epoch is reserved, repeated, or nonmonotonic",
            ));
        }
        if request.sequence == 0
            || self
                .last_sequence
                .is_some_and(|previous| request.sequence <= previous)
            || self.submitted_frames == u64::MAX
        {
            return Err(WebRenderWindowError::new(
                WebRenderWindowFailureStage::ValidateRequest,
                WebRenderWindowErrorKind::FrameSequence,
                "native frame sequence is zero, repeated, exhausted, or nonmonotonic",
            ));
        }
        for (resource, observed, limit) in [
            ("scene items", scene_items, self.limits.max_scene_items),
            (
                "pending text runs",
                pending_text_runs,
                self.limits.max_pending_text_runs,
            ),
            (
                "display-list bytes",
                display_list_bytes,
                self.limits.max_display_list_bytes,
            ),
        ] {
            if observed > limit {
                return Err(WebRenderWindowError::new(
                    WebRenderWindowFailureStage::ValidateRequest,
                    WebRenderWindowErrorKind::ResourceLimit,
                    format_args!("{resource} {observed} exceeds fixed limit {limit}"),
                ));
            }
        }
        Ok(())
    }

    pub(crate) fn validate_pipeline(
        request: WebRenderWindowFrameRequest,
        actual: PipelineKey,
    ) -> Result<(), WebRenderWindowError> {
        if request.pipeline == actual {
            Ok(())
        } else {
            Err(WebRenderWindowError::new(
                WebRenderWindowFailureStage::ValidateRequest,
                WebRenderWindowErrorKind::PipelineMismatch,
                format_args!(
                    "compiled pipeline ({}, {}) differs from requested ({}, {})",
                    actual.source(),
                    actual.pipeline(),
                    request.pipeline.source(),
                    request.pipeline.pipeline()
                ),
            ))
        }
    }

    pub(crate) fn prepare_resize(
        &self,
        request: WebRenderWindowResizeRequest,
    ) -> Result<WebRenderSurfaceRevision, WebRenderWindowError> {
        let stage = WebRenderWindowFailureStage::ResizeSurface;
        self.validate_live_snapshot(request.expected(), stage)?;
        PresentationLimits::default()
            .rgba8_bytes(request.size())
            .map_err(|error| WebRenderWindowError::presentation(stage, &error))?;
        self.next_surface_revision(stage)
    }

    pub(crate) fn prepare_surface_transition(
        &self,
        expected: WebRenderSurfaceSnapshot,
        stage: WebRenderWindowFailureStage,
    ) -> Result<WebRenderSurfaceRevision, WebRenderWindowError> {
        self.validate_live_snapshot(expected, stage)?;
        self.next_surface_revision(stage)
    }

    pub(crate) fn prepare_suspend(
        &self,
        expected: WebRenderSurfaceSnapshot,
    ) -> Result<WebRenderSurfaceRevision, WebRenderWindowError> {
        let stage = WebRenderWindowFailureStage::SuspendSurface;
        self.validate_live_snapshot(expected, stage)?;
        if self.state != WebRenderWindowState::Active {
            return Err(WebRenderWindowError::new(
                stage,
                WebRenderWindowErrorKind::Suspended,
                "only an active nonzero surface can enter explicit suspension",
            ));
        }
        self.next_surface_revision(stage)
    }

    pub(crate) fn prepare_resume(
        &self,
        expected: WebRenderSurfaceSnapshot,
    ) -> Result<WebRenderSurfaceRevision, WebRenderWindowError> {
        let stage = WebRenderWindowFailureStage::ResumeSurface;
        self.validate_live_snapshot(expected, stage)?;
        if self.state != WebRenderWindowState::Suspended {
            return Err(WebRenderWindowError::new(
                stage,
                WebRenderWindowErrorKind::Contract,
                "only an explicitly suspended surface can resume",
            ));
        }
        if expected.size().width == 0 || expected.size().height == 0 {
            return Err(WebRenderWindowError::new(
                stage,
                WebRenderWindowErrorKind::Suspended,
                "a zero-sized surface must receive a nonzero resize before resume",
            ));
        }
        self.next_surface_revision(stage)
    }

    fn next_surface_revision(
        &self,
        stage: WebRenderWindowFailureStage,
    ) -> Result<WebRenderSurfaceRevision, WebRenderWindowError> {
        self.snapshot.revision.checked_next().ok_or_else(|| {
            WebRenderWindowError::new(
                stage,
                WebRenderWindowErrorKind::ResourceLimit,
                "surface configuration revision space is exhausted",
            )
        })
    }

    pub(crate) fn commit_surface_transition(
        &mut self,
        descriptor: SurfaceDescriptor,
        revision: WebRenderSurfaceRevision,
        explicitly_suspended: bool,
    ) {
        self.snapshot = WebRenderSurfaceSnapshot {
            descriptor,
            revision,
            capabilities: self.snapshot.capabilities,
        };
        self.state =
            if explicitly_suspended || descriptor.size.width == 0 || descriptor.size.height == 0 {
                WebRenderWindowState::Suspended
            } else {
                WebRenderWindowState::Active
            };
    }

    pub(crate) fn commit_scale_transition(
        &mut self,
        descriptor: SurfaceDescriptor,
        revision: WebRenderSurfaceRevision,
    ) {
        let preserve_suspension = self.state == WebRenderWindowState::Suspended;
        self.commit_surface_transition(descriptor, revision, preserve_suspension);
    }

    pub(crate) fn commit_transaction(&mut self, request: WebRenderWindowFrameRequest) {
        self.last_document_version = Some(request.document_version);
        self.last_epoch = Some(request.epoch);
        self.last_pipeline = Some(request.pipeline);
    }

    pub(crate) fn commit_browser_transaction(&mut self, epoch: u32, root: PipelineKey) {
        self.last_document_version = None;
        self.last_epoch = Some(epoch);
        self.last_pipeline = Some(root);
    }

    pub(crate) fn commit_swap(&mut self, sequence: u64) {
        self.last_sequence = Some(sequence);
        self.submitted_frames += 1;
    }

    pub(crate) fn lose(&mut self, stage: WebRenderWindowFailureStage) {
        if matches!(
            self.state,
            WebRenderWindowState::Active | WebRenderWindowState::Suspended
        ) {
            self.state = WebRenderWindowState::Lost(stage);
        }
    }

    pub(crate) fn shutdown(&mut self) {
        self.state = WebRenderWindowState::Shutdown;
    }

    fn validate_live_snapshot(
        &self,
        supplied: WebRenderSurfaceSnapshot,
        requested_stage: WebRenderWindowFailureStage,
    ) -> Result<(), WebRenderWindowError> {
        match self.state {
            WebRenderWindowState::Lost(stage) => {
                return Err(WebRenderWindowError::new(
                    stage,
                    WebRenderWindowErrorKind::TerminalState,
                    "window renderer is permanently lost",
                ));
            }
            WebRenderWindowState::Shutdown => {
                return Err(WebRenderWindowError::new(
                    requested_stage,
                    WebRenderWindowErrorKind::TerminalState,
                    "window renderer is shut down",
                ));
            }
            WebRenderWindowState::Active | WebRenderWindowState::Suspended => {}
        }
        if supplied.surface() != self.snapshot.surface() {
            return Err(WebRenderWindowError::new(
                requested_stage,
                WebRenderWindowErrorKind::SurfaceMismatch,
                "surface identity differs from the internally owned presenter",
            ));
        }
        if supplied.revision != self.snapshot.revision {
            return Err(WebRenderWindowError::new(
                requested_stage,
                WebRenderWindowErrorKind::StaleSurfaceRevision,
                format_args!(
                    "surface revision {} differs from current {}",
                    supplied.revision.get(),
                    self.snapshot.revision.get()
                ),
            ));
        }
        if supplied.descriptor != self.snapshot.descriptor {
            return Err(WebRenderWindowError::new(
                requested_stage,
                WebRenderWindowErrorKind::SizeMismatch,
                "surface descriptor differs at the current revision",
            ));
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn force_revision_for_test(&mut self, value: NonZeroU64) {
        self.snapshot.revision = WebRenderSurfaceRevision::from_nonzero_for_test(value);
    }

    #[cfg(test)]
    pub(crate) const fn last_sequence_for_test(&self) -> Option<u64> {
        self.last_sequence
    }

    #[cfg(test)]
    pub(crate) const fn submitted_frames_for_test(&self) -> u64 {
        self.submitted_frames
    }
}

fn revision_regressed(previous: DocumentVersion, actual: DocumentVersion) -> WebRenderWindowError {
    let document_id: DocumentId = actual.document_id();
    WebRenderWindowError::new(
        WebRenderWindowFailureStage::ValidateRequest,
        WebRenderWindowErrorKind::RevisionRegressed,
        format_args!(
            "scene revision for document {} regressed from {} to {}",
            document_id.get(),
            previous.revision(),
            actual.revision()
        ),
    )
}

pub(crate) const fn presentation_outcome(
    result: Result<PresentationShutdownReport, crate::PresentationRetentionReport>,
) -> PresentationTeardownOutcome {
    match result {
        Ok(report) => PresentationTeardownOutcome::WrappersReleased(report),
        Err(report) => PresentationTeardownOutcome::RetainedAfterTeardownFailure(report),
    }
}

pub(crate) fn presentation_retention_error(
    stage: WebRenderWindowFailureStage,
    native_stage: PresentationFailureStage,
    kind: PresentationErrorKind,
) -> WebRenderWindowError {
    WebRenderWindowError::new(
        stage,
        if kind == PresentationErrorKind::ContextLost {
            WebRenderWindowErrorKind::DeviceLost
        } else {
            WebRenderWindowErrorKind::Native
        },
        format_args!("native presenter retained owners after {native_stage:?}/{kind:?}"),
    )
}

fn bounded_detail(value: &str) -> String {
    if value.len() <= MAX_ERROR_DETAIL_BYTES {
        return value.to_owned();
    }
    let mut boundary = MAX_ERROR_DETAIL_BYTES - 3;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    let mut result = value[..boundary].to_owned();
    result.push_str("...");
    result
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;
    use std::time::Duration;

    use wild_buzzard_dom::Document;
    use wild_buzzard_platform::{
        PhysicalSize, PixelFormat, ScaleFactor, SurfaceDescriptor, SurfaceIdAllocator,
        SurfaceNamespace, SurfaceRole,
    };
    use wild_buzzard_renderer::PipelineKey;

    use super::{
        MAX_ERROR_DETAIL_BYTES, WebRenderSurfaceRevision, WebRenderTeardownEvidence,
        WebRenderWindowContract, WebRenderWindowError, WebRenderWindowErrorKind,
        WebRenderWindowFailureStage, WebRenderWindowFrameReceipt, WebRenderWindowFrameRequest,
        WebRenderWindowLimits, WebRenderWindowResizeRequest, WebRenderWindowShutdownReport,
        WebRenderWindowStartupFailure, WebRenderWindowState,
    };
    use crate::{
        LinuxAccelerationClass, LinuxPresentationCapabilities, LinuxResetProtection,
        PresentationShutdownReport, PresentationTeardownOutcome,
    };

    fn descriptor() -> SurfaceDescriptor {
        let mut allocator =
            SurfaceIdAllocator::new(SurfaceNamespace::new(5_401).expect("nonzero namespace"));
        SurfaceDescriptor {
            id: allocator.allocate().expect("surface identity"),
            size: PhysicalSize::new(800, 600).expect("bounded size"),
            scale: ScaleFactor::new(1.0).expect("valid scale"),
            format: PixelFormat::Rgba8Srgb,
            role: SurfaceRole::Window,
        }
    }

    fn request(
        contract: &WebRenderWindowContract,
        document: &Document,
        epoch: u32,
        sequence: u64,
    ) -> WebRenderWindowFrameRequest {
        WebRenderWindowFrameRequest::new(
            contract.snapshot(),
            document.version(),
            PipelineKey::new(17, 23),
            epoch,
            sequence,
        )
    }

    #[test]
    fn capability_binding_survives_revisions_receipts_and_shutdown() {
        let descriptor = descriptor();
        let capabilities = LinuxPresentationCapabilities::new(
            LinuxAccelerationClass::Software,
            LinuxResetProtection::Unavailable,
        );
        let mut contract = WebRenderWindowContract::new_with_capabilities(descriptor, capabilities);
        assert_eq!(contract.snapshot().capabilities(), capabilities);

        let original = contract.snapshot();
        let revision = contract
            .prepare_surface_transition(original, WebRenderWindowFailureStage::ResizeSurface)
            .expect("exact snapshot may resize");
        let resized = SurfaceDescriptor {
            size: PhysicalSize::new(1_024, 768).expect("bounded size"),
            ..descriptor
        };
        contract.commit_surface_transition(resized, revision, false);
        assert_eq!(contract.snapshot().capabilities(), capabilities);

        let document = Document::new();
        let request = request(&contract, &document, 1, 1);
        let receipt = WebRenderWindowFrameReceipt::new(request, 7, 3_145_728);
        assert_eq!(receipt.capabilities(), capabilities);

        let native = PresentationShutdownReport::new_with_capabilities(
            descriptor.id,
            1,
            Some(1),
            capabilities,
        );
        let shutdown = WebRenderWindowShutdownReport::new(
            WebRenderTeardownEvidence::Confirmed,
            WebRenderTeardownEvidence::Confirmed,
            0,
            0,
            0,
            native,
        );
        assert_eq!(shutdown.capabilities(), capabilities);
    }

    #[test]
    fn surface_revision_changes_once_and_rejects_the_old_snapshot() {
        let descriptor = descriptor();
        let mut contract = WebRenderWindowContract::new(descriptor);
        let original = contract.snapshot();
        let revision = contract
            .prepare_surface_transition(original, WebRenderWindowFailureStage::ResizeSurface)
            .expect("fresh revision");
        let resized = SurfaceDescriptor {
            size: PhysicalSize::new(1_024, 768).expect("bounded size"),
            ..descriptor
        };
        contract.commit_surface_transition(resized, revision, false);

        assert_eq!(contract.snapshot().revision().get(), 2);
        assert_eq!(contract.snapshot().size(), resized.size);
        assert_eq!(
            contract
                .prepare_surface_transition(original, WebRenderWindowFailureStage::ResizeSurface)
                .expect_err("old revision must not replay")
                .kind(),
            WebRenderWindowErrorKind::StaleSurfaceRevision
        );
    }

    #[test]
    fn explicit_suspend_and_resume_publish_distinct_checked_revisions() {
        let descriptor = descriptor();
        let mut contract = WebRenderWindowContract::new(descriptor);
        let document = Document::new();
        let active = contract.snapshot();
        let suspended_revision = contract
            .prepare_suspend(active)
            .expect("active surface may suspend");
        contract.commit_surface_transition(descriptor, suspended_revision, true);
        let suspended = contract.snapshot();

        assert_eq!(contract.state(), WebRenderWindowState::Suspended);
        assert_eq!(suspended.revision().get(), 2);
        assert_eq!(
            contract
                .prepare_suspend(suspended)
                .expect_err("suspension cannot be repeated")
                .kind(),
            WebRenderWindowErrorKind::Suspended
        );

        let scale_revision = contract
            .prepare_surface_transition(suspended, WebRenderWindowFailureStage::ResizeSurface)
            .expect("exact suspended surface may change scale");
        let scaled_descriptor = SurfaceDescriptor {
            scale: ScaleFactor::new(2.0).expect("valid scale"),
            ..descriptor
        };
        contract.commit_scale_transition(scaled_descriptor, scale_revision);
        let scaled_suspended = contract.snapshot();
        assert_eq!(contract.state(), WebRenderWindowState::Suspended);
        assert_eq!(scaled_suspended.descriptor(), scaled_descriptor);
        assert_eq!(scaled_suspended.revision().get(), 3);
        assert_eq!(
            contract
                .validate_submission(
                    request(&contract, &document, 1, 1),
                    document.version(),
                    descriptor.size.width,
                    descriptor.size.height,
                    0,
                    0,
                    0,
                )
                .expect_err("scale-only change cannot reactivate a suspended surface")
                .kind(),
            WebRenderWindowErrorKind::Suspended
        );

        let resumed_revision = contract
            .prepare_resume(scaled_suspended)
            .expect("exact suspended surface may resume");
        contract.commit_surface_transition(scaled_descriptor, resumed_revision, false);
        assert_eq!(contract.state(), WebRenderWindowState::Active);
        assert_eq!(contract.snapshot().revision().get(), 4);
        assert_eq!(contract.snapshot().descriptor(), scaled_descriptor);
        assert_eq!(
            contract
                .prepare_resume(suspended)
                .expect_err("old suspended revision cannot replay")
                .kind(),
            WebRenderWindowErrorKind::StaleSurfaceRevision
        );
    }

    #[test]
    fn exact_document_epoch_pipeline_and_sequence_are_monotonic() {
        let descriptor = descriptor();
        let mut contract = WebRenderWindowContract::new(descriptor);
        let document = Document::new();
        let first = request(&contract, &document, 4, 9);
        contract
            .validate_submission(
                first,
                document.version(),
                descriptor.size.width,
                descriptor.size.height,
                3,
                0,
                64,
            )
            .expect("first exact submission");
        WebRenderWindowContract::validate_pipeline(first, PipelineKey::new(17, 23))
            .expect("exact pipeline");
        contract.commit_transaction(first);
        contract.commit_swap(first.sequence());

        let repeated_epoch = request(&contract, &document, 4, 10);
        assert_eq!(
            contract
                .validate_submission(
                    repeated_epoch,
                    document.version(),
                    descriptor.size.width,
                    descriptor.size.height,
                    3,
                    0,
                    64,
                )
                .expect_err("epoch cannot replay")
                .kind(),
            WebRenderWindowErrorKind::Epoch
        );

        let repeated_sequence = request(&contract, &document, 5, 9);
        assert_eq!(
            contract
                .validate_submission(
                    repeated_sequence,
                    document.version(),
                    descriptor.size.width,
                    descriptor.size.height,
                    3,
                    0,
                    64,
                )
                .expect_err("swap sequence cannot replay")
                .kind(),
            WebRenderWindowErrorKind::FrameSequence
        );
        assert_eq!(
            WebRenderWindowContract::validate_pipeline(
                request(&contract, &document, 5, 10),
                PipelineKey::new(17, 24),
            )
            .expect_err("pipeline scalar must match")
            .kind(),
            WebRenderWindowErrorKind::PipelineMismatch
        );
    }

    #[test]
    fn fixed_limits_and_suspension_reject_before_transaction_commit() {
        let mut suspended_descriptor = descriptor();
        let limits = WebRenderWindowLimits::for_test(
            2,
            1,
            32,
            Duration::from_millis(3),
            Duration::from_millis(4),
        );
        let document = Document::new();
        let contract = WebRenderWindowContract::with_limits(suspended_descriptor, limits);
        let exact = request(&contract, &document, 1, 1);
        assert_eq!(
            contract
                .validate_submission(
                    exact,
                    document.version(),
                    suspended_descriptor.size.width,
                    suspended_descriptor.size.height,
                    3,
                    0,
                    16,
                )
                .expect_err("scene item limit")
                .kind(),
            WebRenderWindowErrorKind::ResourceLimit
        );

        suspended_descriptor.size = PhysicalSize::new(0, 0).expect("zero size is suspension");
        let suspended = WebRenderWindowContract::with_limits(suspended_descriptor, limits);
        assert_eq!(suspended.state(), WebRenderWindowState::Suspended);
        let suspended_request = request(&suspended, &document, 1, 1);
        assert_eq!(
            suspended
                .validate_submission(suspended_request, document.version(), 0, 0, 0, 0, 0,)
                .expect_err("suspended surface")
                .kind(),
            WebRenderWindowErrorKind::Suspended
        );

        let active = WebRenderWindowContract::new(descriptor());
        let oversized =
            PhysicalSize::new(16_385, 1).expect("value type does not apply presenter cap");
        assert_eq!(
            active
                .prepare_resize(WebRenderWindowResizeRequest::new(
                    active.snapshot(),
                    oversized
                ))
                .expect_err("resize resource bound must reject before native admission")
                .kind(),
            WebRenderWindowErrorKind::ResourceLimit
        );
        assert_eq!(active.state(), WebRenderWindowState::Active);
    }

    #[test]
    fn exhausted_surface_revision_fails_closed_without_wrapping() {
        let mut contract = WebRenderWindowContract::new(descriptor());
        contract.force_revision_for_test(NonZeroU64::MAX);
        assert_eq!(
            contract
                .prepare_surface_transition(
                    contract.snapshot(),
                    WebRenderWindowFailureStage::ResizeSurface,
                )
                .expect_err("revision must not wrap")
                .kind(),
            WebRenderWindowErrorKind::ResourceLimit
        );
        assert_eq!(
            WebRenderSurfaceRevision::from_nonzero_for_test(NonZeroU64::MAX).get(),
            u64::MAX
        );
    }

    #[test]
    fn startup_failure_preserves_primary_and_independent_teardown_evidence() {
        let descriptor = descriptor();
        let primary = WebRenderWindowError::new(
            WebRenderWindowFailureStage::InitializeRenderer,
            WebRenderWindowErrorKind::Renderer,
            "renderer initialization failed",
        );
        let native = PresentationShutdownReport::new(descriptor.id, 0, None);
        let teardown = Ok(WebRenderWindowShutdownReport::new(
            WebRenderTeardownEvidence::NotApplicable,
            WebRenderTeardownEvidence::NotApplicable,
            0,
            0,
            0,
            native,
        ));
        let failure = WebRenderWindowStartupFailure::new(primary.clone(), teardown);
        assert_eq!(
            failure.capabilities(),
            crate::LinuxPresentationCapabilities::STRICT_HARDWARE
        );
        assert_eq!(
            failure.presentation_teardown(),
            PresentationTeardownOutcome::WrappersReleased(native)
        );
        let (observed_primary, observed_teardown) = failure.into_parts();

        assert_eq!(observed_primary, primary);
        let observed = observed_teardown.expect("checked wrapper release");
        assert_eq!(
            observed.backend_shutdown(),
            WebRenderTeardownEvidence::NotApplicable
        );
        assert_eq!(
            observed.renderer_deinitialization(),
            WebRenderTeardownEvidence::NotApplicable
        );
        assert!(!observed.backend_acknowledged());
        assert!(!observed.renderer_deinitialized());
        assert_eq!(observed.presentation(), native);
    }

    #[test]
    fn diagnostics_are_utf8_safe_and_bounded() {
        let error = WebRenderWindowError::new(
            WebRenderWindowFailureStage::RenderFrame,
            WebRenderWindowErrorKind::Renderer,
            "🦅".repeat(MAX_ERROR_DETAIL_BYTES),
        );
        assert!(error.detail().len() <= MAX_ERROR_DETAIL_BYTES);
        assert!(error.detail().ends_with("..."));
        assert!(error.detail().is_char_boundary(error.detail().len()));
    }

    #[test]
    fn deadline_terminality_changes_only_after_transaction_acceptance() {
        let compose = WebRenderWindowError::new(
            WebRenderWindowFailureStage::ComposeScene,
            WebRenderWindowErrorKind::Timeout,
            "deadline",
        );
        assert!(!compose.is_terminal());

        for stage in [
            WebRenderWindowFailureStage::AwaitFrameBuilt,
            WebRenderWindowFailureStage::AwaitFrameReady,
            WebRenderWindowFailureStage::UpdateRenderer,
            WebRenderWindowFailureStage::RenderFrame,
            WebRenderWindowFailureStage::AwaitFrameRendered,
        ] {
            assert!(
                WebRenderWindowError::new(stage, WebRenderWindowErrorKind::Timeout, "deadline",)
                    .is_terminal()
            );
        }
    }
}
