#![forbid(unsafe_code)]

use std::error::Error;
use std::fmt;
use std::num::NonZeroU64;

use wild_buzzard_platform::{
    PhysicalSize, PixelFormat, ScaleFactor, SurfaceDescriptor, SurfaceId, SurfaceRole,
};

/// Maximum width or height admitted by the Linux presentation boundary.
pub const MAX_PRESENTATION_DIMENSION: u32 = 16_384;
/// Maximum pixels admitted by one Linux presentation surface.
pub const MAX_PRESENTATION_PIXELS: u64 = 67_108_864;
/// Maximum RGBA8-equivalent bytes admitted by one Linux presentation surface.
pub const MAX_PRESENTATION_PIXEL_BYTES: u64 = 256 << 20;
/// Maximum successful submissions during one presenter lifetime.
pub const MAX_PRESENTATION_FRAMES: u64 = u64::MAX - 1;

const MAX_ERROR_DETAIL_BYTES: usize = 1_024;

/// Native display protocol selected for the exact window.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LinuxPresentationBackend {
    /// Wayland plus `wl_egl_window` and EGL.
    Wayland,
    /// X11 plus an EGL-compatible X visual.
    X11,
}

/// Startup policy for selecting one Linux EGL presentation capability set.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum LinuxPresentationPolicy {
    /// Preserve the original hardware-accelerated, lose-context-on-reset path.
    #[default]
    StrictHardware,
    /// Try the fixed accelerated/software and robust/compatible profile ladder.
    AutomaticCompatible,
}

/// Acceleration fact reported by the selected EGL configuration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LinuxAccelerationClass {
    /// EGL reports that the selected configuration is hardware accelerated.
    Accelerated,
    /// EGL reports that the selected configuration is software rendered.
    Software,
}

/// Verified reset behavior of the selected current desktop-GL context.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LinuxResetProtection {
    /// Robust access and lose-context-on-reset were both verified.
    LoseContextOnReset,
    /// The compatible context has no verified robust-access protection.
    Unavailable,
}

/// Immutable value-only facts for one selected Linux presentation profile.
///
/// These facts authorize only same-process browser-surface presentation.
/// They do not enable WebGL, WebGPU, accelerated canvas, or satisfy release
/// process-isolation and sandbox acceptance.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LinuxPresentationCapabilities {
    acceleration: LinuxAccelerationClass,
    reset_protection: LinuxResetProtection,
}

impl LinuxPresentationCapabilities {
    /// The exact capabilities required by [`LinuxPresentationPolicy::StrictHardware`].
    pub const STRICT_HARDWARE: Self = Self {
        acceleration: LinuxAccelerationClass::Accelerated,
        reset_protection: LinuxResetProtection::LoseContextOnReset,
    };

    /// Creates one value-only capability pair after native verification.
    #[must_use]
    pub const fn new(
        acceleration: LinuxAccelerationClass,
        reset_protection: LinuxResetProtection,
    ) -> Self {
        Self {
            acceleration,
            reset_protection,
        }
    }

    /// EGL-reported acceleration class.
    #[must_use]
    pub const fn acceleration(self) -> LinuxAccelerationClass {
        self.acceleration
    }

    /// Verified context-reset protection.
    #[must_use]
    pub const fn reset_protection(self) -> LinuxResetProtection {
        self.reset_protection
    }
}

impl Default for LinuxPresentationCapabilities {
    fn default() -> Self {
        Self::STRICT_HARDWARE
    }
}

/// Fixed, caller-nonenlargeable limits for one native presentation surface.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PresentationLimits {
    dimension: u32,
    pixels: u64,
    pixel_bytes: u64,
    frames: u64,
}

impl Default for PresentationLimits {
    fn default() -> Self {
        Self {
            dimension: MAX_PRESENTATION_DIMENSION,
            pixels: MAX_PRESENTATION_PIXELS,
            pixel_bytes: MAX_PRESENTATION_PIXEL_BYTES,
            frames: MAX_PRESENTATION_FRAMES,
        }
    }
}

impl PresentationLimits {
    /// Maximum width or height.
    #[must_use]
    pub const fn max_dimension(self) -> u32 {
        self.dimension
    }

    /// Maximum physical pixels.
    #[must_use]
    pub const fn max_pixels(self) -> u64 {
        self.pixels
    }

    /// Maximum RGBA8-equivalent bytes.
    #[must_use]
    pub const fn max_pixel_bytes(self) -> u64 {
        self.pixel_bytes
    }

    /// Maximum successful frame submissions.
    #[must_use]
    pub const fn max_frames(self) -> u64 {
        self.frames
    }

    pub(crate) fn rgba8_bytes(self, size: PhysicalSize) -> Result<u64, PresentationError> {
        if size.width > self.dimension || size.height > self.dimension {
            return Err(PresentationError::contract(
                PresentationFailureStage::ValidateSurface,
                PresentationErrorKind::ResourceLimit,
                format!(
                    "surface {}x{} exceeds per-axis limit {}",
                    size.width, size.height, self.dimension
                ),
            ));
        }
        let pixels = u64::from(size.width)
            .checked_mul(u64::from(size.height))
            .ok_or_else(|| {
                PresentationError::contract(
                    PresentationFailureStage::ValidateSurface,
                    PresentationErrorKind::ResourceLimit,
                    "surface pixel count overflowed",
                )
            })?;
        if pixels > self.pixels {
            return Err(PresentationError::contract(
                PresentationFailureStage::ValidateSurface,
                PresentationErrorKind::ResourceLimit,
                format!("surface has {pixels} pixels; limit is {}", self.pixels),
            ));
        }
        let bytes = pixels.checked_mul(4).ok_or_else(|| {
            PresentationError::contract(
                PresentationFailureStage::ValidateSurface,
                PresentationErrorKind::ResourceLimit,
                "RGBA8 byte count overflowed",
            )
        })?;
        if bytes > self.pixel_bytes {
            return Err(PresentationError::contract(
                PresentationFailureStage::ValidateSurface,
                PresentationErrorKind::ResourceLimit,
                format!(
                    "surface requires {bytes} RGBA8-equivalent bytes; limit is {}",
                    self.pixel_bytes
                ),
            ));
        }
        Ok(bytes)
    }
}

/// Stable stage at which presentation failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PresentationFailureStage {
    /// Validate fixed format, role, dimensions, bytes, or identity.
    ValidateSurface,
    /// Borrow the event loop's display handle.
    DisplayHandle,
    /// Create an EGL display for the selected native display.
    CreateDisplay,
    /// Select an exact window-capable RGBA8 sRGB EGL configuration.
    SelectConfig,
    /// Borrow and validate the owned window handle.
    WindowHandle,
    /// Create the selected robust or compatible desktop GL context.
    CreateContext,
    /// Create or recreate the EGL window surface.
    CreateSurface,
    /// Make the exact context and window surface current.
    MakeCurrent,
    /// Load desktop GL functions for the current EGL display.
    LoadFunctions,
    /// Configure swap submission behavior.
    ConfigureSwap,
    /// Resize or resume the native window surface.
    ResizeSurface,
    /// Execute a bounded direct-GPU frame callback.
    DrawFrame,
    /// Submit the completed native back buffer through EGL.
    SwapBuffers,
    /// Make EGL non-current and release native-owner wrappers in order.
    ReleaseContext,
}

/// Stable class of presentation error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PresentationErrorKind {
    /// The configured pixel format or surface role is not implemented.
    UnsupportedContract,
    /// One exact startup capability profile is unavailable.
    UnsupportedCapability,
    /// A fixed dimension, byte, or lifetime cap was exceeded.
    ResourceLimit,
    /// A stale or foreign surface identity was supplied.
    SurfaceMismatch,
    /// A frame did not name the presenter's exact current physical size.
    SizeMismatch,
    /// A frame sequence was zero, repeated, or nonmonotonic.
    FrameSequence,
    /// The native surface is deliberately absent while zero-sized or suspended.
    Suspended,
    /// This presenter has entered a terminal failure state.
    TerminalState,
    /// The callback did not produce exactly one complete frame.
    RendererRejected,
    /// A diagnostic readback did not match the submitted frame.
    DiagnosticMismatch,
    /// EGL, GL, or the native backend rejected an operation.
    Driver,
    /// EGL or the native backend reported allocation failure.
    OutOfMemory,
    /// The selected graphics context was reported lost.
    ContextLost,
}

/// Bounded presentation failure with stable stage and class.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PresentationError {
    stage: PresentationFailureStage,
    kind: PresentationErrorKind,
    capabilities: Option<LinuxPresentationCapabilities>,
    detail: String,
}

impl PresentationError {
    pub(crate) fn contract(
        stage: PresentationFailureStage,
        kind: PresentationErrorKind,
        detail: impl fmt::Display,
    ) -> Self {
        Self {
            stage,
            kind,
            capabilities: None,
            detail: bounded_detail(&detail.to_string()),
        }
    }

    pub(crate) fn with_capabilities(mut self, capabilities: LinuxPresentationCapabilities) -> Self {
        self.capabilities = Some(capabilities);
        self
    }

    pub(crate) fn driver(stage: PresentationFailureStage, error: &glutin::error::Error) -> Self {
        let kind = match error.error_kind() {
            glutin::error::ErrorKind::ContextLost => PresentationErrorKind::ContextLost,
            glutin::error::ErrorKind::OutOfMemory => PresentationErrorKind::OutOfMemory,
            _ => PresentationErrorKind::Driver,
        };
        Self::contract(stage, kind, error)
    }

    /// Stable failure stage.
    #[must_use]
    pub const fn stage(&self) -> PresentationFailureStage {
        self.stage
    }

    /// Stable failure class.
    #[must_use]
    pub const fn kind(&self) -> PresentationErrorKind {
        self.kind
    }

    /// Exact attempted or selected capabilities when the failure is profile-bound.
    #[must_use]
    pub const fn capabilities(&self) -> Option<LinuxPresentationCapabilities> {
        self.capabilities
    }

    /// Bounded diagnostic text; never use it for control flow.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }

    /// Whether this failure permanently poisons the current presenter.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(
            self.kind,
            PresentationErrorKind::Driver
                | PresentationErrorKind::OutOfMemory
                | PresentationErrorKind::ContextLost
                | PresentationErrorKind::DiagnosticMismatch
                | PresentationErrorKind::TerminalState
        )
    }
}

impl fmt::Display for PresentationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{:?}/{:?}: {}",
            self.stage, self.kind, self.detail
        )
    }
}

impl Error for PresentationError {}

/// Externally observable presenter lifecycle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PresentationState {
    /// A nonzero native window surface may accept frames.
    Active,
    /// No EGL window surface exists while zero-sized or explicitly suspended.
    Suspended,
    /// A native failure permanently closed frame and resize admission.
    Lost(PresentationFailureStage),
    /// Normal teardown completed.
    Shutdown,
}

/// Exact identity, size, and monotonic sequence for one direct frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DirectFrameRequest {
    surface: SurfaceId,
    size: PhysicalSize,
    sequence: u64,
}

impl DirectFrameRequest {
    /// Creates a frame request. Validation occurs against the live presenter.
    #[must_use]
    pub const fn new(surface: SurfaceId, size: PhysicalSize, sequence: u64) -> Self {
        Self {
            surface,
            size,
            sequence,
        }
    }

    /// Exact generational surface identity.
    #[must_use]
    pub const fn surface(self) -> SurfaceId {
        self.surface
    }

    /// Exact current physical size expected by the producer.
    #[must_use]
    pub const fn size(self) -> PhysicalSize {
        self.size
    }

    /// Strictly increasing, nonzero producer sequence.
    #[must_use]
    pub const fn sequence(self) -> u64 {
        self.sequence
    }
}

/// Eight-bit non-premultiplied RGBA clear color for the first direct frame proof.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SolidColor {
    rgba: [u8; 4],
}

impl SolidColor {
    /// Creates an opaque or translucent RGBA color.
    #[must_use]
    pub const fn new(red: u8, green: u8, blue: u8, alpha: u8) -> Self {
        Self {
            rgba: [red, green, blue, alpha],
        }
    }

    /// Channel values in red, green, blue, alpha order.
    #[must_use]
    pub const fn rgba(self) -> [u8; 4] {
        self.rgba
    }
}

/// Bounded first-frame source rendered directly into the native GPU back buffer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SolidColorFrame {
    request: DirectFrameRequest,
    color: SolidColor,
}

impl SolidColorFrame {
    /// Binds an exact request to one Wild Buzzard-owned frame color.
    #[must_use]
    pub const fn new(request: DirectFrameRequest, color: SolidColor) -> Self {
        Self { request, color }
    }

    /// Exact frame request.
    #[must_use]
    pub const fn request(self) -> DirectFrameRequest {
        self.request
    }

    /// Exact frame color.
    #[must_use]
    pub const fn color(self) -> SolidColor {
        self.color
    }
}

/// Failure returned by a callback-scoped direct renderer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DirectRenderError {
    /// The renderer deliberately rejected this frame without touching native ownership.
    Rejected,
    /// The renderer returned without producing exactly one complete frame.
    NoCompleteFrame,
    /// More than one full-frame output was attempted in one submission.
    MultipleCompleteFrames,
    /// A GL command failed with the recorded GL error code.
    GlError(u32),
    /// Diagnostic readback did not match the just-rendered pixel value.
    DiagnosticMismatch {
        expected: [u8; 4],
        observed: [u8; 4],
    },
}

impl fmt::Display for DirectRenderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rejected => formatter.write_str("direct renderer rejected the frame"),
            Self::NoCompleteFrame => formatter.write_str("direct renderer produced no frame"),
            Self::MultipleCompleteFrames => {
                formatter.write_str("direct renderer produced more than one complete frame")
            }
            Self::GlError(code) => write!(formatter, "GL error {code:#x}"),
            Self::DiagnosticMismatch { expected, observed } => write!(
                formatter,
                "diagnostic pixel mismatch: expected {expected:?}, observed {observed:?}"
            ),
        }
    }
}

impl Error for DirectRenderError {}

/// Callback-scoped producer of one complete direct GPU frame.
///
/// The target exposes bounded drawing capabilities but no GL object or native
/// handle. A future `WebRender` adapter belongs inside this crate and can add a
/// similarly narrow target operation without expanding native authority.
pub trait DirectRenderer {
    /// Emits exactly one complete frame into the current native back buffer.
    ///
    /// # Errors
    ///
    /// Returns a bounded renderer failure if it cannot produce exactly one
    /// complete frame through the supplied capability.
    fn render(
        &mut self,
        target: &mut crate::DirectFrameTarget<'_>,
    ) -> Result<(), DirectRenderError>;
}

/// Evidence returned after draw verification and a successful EGL swap call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SwapSubmissionReceipt {
    surface: SurfaceId,
    size: PhysicalSize,
    sequence: u64,
    capabilities: LinuxPresentationCapabilities,
    rgba8_byte_equivalent: u64,
    diagnostic_sample: [u8; 4],
}

impl SwapSubmissionReceipt {
    pub(crate) const fn new(
        request: DirectFrameRequest,
        capabilities: LinuxPresentationCapabilities,
        rgba8_byte_equivalent: u64,
        diagnostic_sample: [u8; 4],
    ) -> Self {
        Self {
            surface: request.surface,
            size: request.size,
            sequence: request.sequence,
            capabilities,
            rgba8_byte_equivalent,
            diagnostic_sample,
        }
    }

    /// Exact surface which received the draw and swap submission.
    #[must_use]
    pub const fn surface(self) -> SurfaceId {
        self.surface
    }

    /// Exact physical back-buffer size used for the draw.
    #[must_use]
    pub const fn size(self) -> PhysicalSize {
        self.size
    }

    /// Committed producer sequence.
    #[must_use]
    pub const fn sequence(self) -> u64 {
        self.sequence
    }

    /// Exact immutable profile used for this draw and swap.
    #[must_use]
    pub const fn capabilities(self) -> LinuxPresentationCapabilities {
        self.capabilities
    }

    /// Bounded RGBA8-equivalent bytes for this surface.
    #[must_use]
    pub const fn rgba8_byte_equivalent(self) -> u64 {
        self.rgba8_byte_equivalent
    }

    /// Pixel sampled from the native back buffer immediately before swap.
    #[must_use]
    pub const fn diagnostic_sample(self) -> [u8; 4] {
        self.diagnostic_sample
    }

    /// EGL exposes successful swap submission, not compositor display acknowledgement.
    #[must_use]
    pub const fn compositor_acknowledged(self) -> bool {
        false
    }
}

/// Evidence returned after every Rust native-owner wrapper released normally.
///
/// Glutin does not expose the result of `eglDestroySurface` or the other EGL
/// destructor calls it performs from `Drop`. This report therefore proves
/// checked non-current admission followed by normal Rust-wrapper release
/// order; it does not claim that EGL acknowledged native destruction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PresentationShutdownReport {
    surface: SurfaceId,
    submitted_frames: u64,
    last_sequence: Option<NonZeroU64>,
    capabilities: LinuxPresentationCapabilities,
}

impl PresentationShutdownReport {
    #[cfg(test)]
    pub(crate) const fn new(
        surface: SurfaceId,
        submitted_frames: u64,
        last_sequence: Option<u64>,
    ) -> Self {
        Self::new_with_capabilities(
            surface,
            submitted_frames,
            last_sequence,
            LinuxPresentationCapabilities::STRICT_HARDWARE,
        )
    }

    pub(crate) const fn new_with_capabilities(
        surface: SurfaceId,
        submitted_frames: u64,
        last_sequence: Option<u64>,
        capabilities: LinuxPresentationCapabilities,
    ) -> Self {
        Self {
            surface,
            submitted_frames,
            last_sequence: optional_nonzero(last_sequence),
            capabilities,
        }
    }

    /// Retired surface identity.
    #[must_use]
    pub const fn surface(self) -> SurfaceId {
        self.surface
    }

    /// Successful draw-and-swap submissions.
    #[must_use]
    pub const fn submitted_frames(self) -> u64 {
        self.submitted_frames
    }

    /// Last successful producer sequence.
    #[must_use]
    pub const fn last_sequence(self) -> Option<u64> {
        match self.last_sequence {
            Some(sequence) => Some(sequence.get()),
            None => None,
        }
    }

    /// Exact immutable profile retired by this shutdown.
    #[must_use]
    pub const fn capabilities(self) -> LinuxPresentationCapabilities {
        self.capabilities
    }
}

/// Terminal teardown evidence when extant native owners were deliberately leaked.
///
/// Retention is the fail-closed response to a panic or error while checking or
/// making EGL non-current, or while releasing an owner wrapper. In particular,
/// dropping the window after a failed unbind could leave a current EGL surface
/// referring to freed native storage.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PresentationRetentionReport {
    surface: SurfaceId,
    submitted_frames: u64,
    last_sequence: Option<NonZeroU64>,
    failure_stage: PresentationFailureStage,
    failure_kind: PresentationErrorKind,
    capabilities: LinuxPresentationCapabilities,
}

impl PresentationRetentionReport {
    #[cfg(test)]
    pub(crate) const fn new(
        surface: SurfaceId,
        submitted_frames: u64,
        last_sequence: Option<u64>,
        failure_stage: PresentationFailureStage,
        failure_kind: PresentationErrorKind,
    ) -> Self {
        Self::new_with_capabilities(
            surface,
            submitted_frames,
            last_sequence,
            failure_stage,
            failure_kind,
            LinuxPresentationCapabilities::STRICT_HARDWARE,
        )
    }

    pub(crate) const fn new_with_capabilities(
        surface: SurfaceId,
        submitted_frames: u64,
        last_sequence: Option<u64>,
        failure_stage: PresentationFailureStage,
        failure_kind: PresentationErrorKind,
        capabilities: LinuxPresentationCapabilities,
    ) -> Self {
        Self {
            surface,
            submitted_frames,
            last_sequence: optional_nonzero(last_sequence),
            failure_stage,
            failure_kind,
            capabilities,
        }
    }

    /// Logically retired surface whose extant native owners were retained.
    #[must_use]
    pub const fn surface(self) -> SurfaceId {
        self.surface
    }

    /// Successful draw-and-swap submissions before retention.
    #[must_use]
    pub const fn submitted_frames(self) -> u64 {
        self.submitted_frames
    }

    /// Last successful producer sequence before retention.
    #[must_use]
    pub const fn last_sequence(self) -> Option<u64> {
        match self.last_sequence {
            Some(sequence) => Some(sequence.get()),
            None => None,
        }
    }

    /// Exact native teardown stage which failed or panicked.
    #[must_use]
    pub const fn failure_stage(self) -> PresentationFailureStage {
        self.failure_stage
    }

    /// Stable class of the native teardown failure.
    #[must_use]
    pub const fn failure_kind(self) -> PresentationErrorKind {
        self.failure_kind
    }

    /// Exact immutable profile whose owners were retained.
    #[must_use]
    pub const fn capabilities(self) -> LinuxPresentationCapabilities {
        self.capabilities
    }
}

const fn optional_nonzero(value: Option<u64>) -> Option<NonZeroU64> {
    match value {
        Some(value) => NonZeroU64::new(value),
        None => None,
    }
}

/// Exact terminal result of releasing a presenter which reached native ownership.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PresentationTeardownOutcome {
    /// EGL was checked non-current and every Rust owner wrapper released normally.
    WrappersReleased(PresentationShutdownReport),
    /// A teardown fault caused every still-extant native owner to be retained.
    RetainedAfterTeardownFailure(PresentationRetentionReport),
}

impl PresentationTeardownOutcome {
    /// Logical surface whose presentation ownership reached teardown.
    #[must_use]
    pub const fn surface(self) -> SurfaceId {
        match self {
            Self::WrappersReleased(report) => report.surface(),
            Self::RetainedAfterTeardownFailure(report) => report.surface(),
        }
    }

    /// Exact immutable profile retired or retained by this outcome.
    #[must_use]
    pub const fn capabilities(self) -> LinuxPresentationCapabilities {
        match self {
            Self::WrappersReleased(report) => report.capabilities(),
            Self::RetainedAfterTeardownFailure(report) => report.capabilities(),
        }
    }
}

/// Startup failure after a native presenter owner had already been established.
///
/// The primary failure retains its original stage and class. `teardown` is
/// separate evidence describing what happened while retiring the partially
/// initialized owner; it never replaces or obscures the startup cause.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PresentationStartupFailure {
    primary: PresentationError,
    teardown: PresentationTeardownOutcome,
}

impl PresentationStartupFailure {
    pub(crate) const fn new(
        primary: PresentationError,
        teardown: PresentationTeardownOutcome,
    ) -> Self {
        Self { primary, teardown }
    }

    /// Original startup failure.
    #[must_use]
    pub const fn primary(&self) -> &PresentationError {
        &self.primary
    }

    /// Exact cleanup result for the partially initialized native owner.
    #[must_use]
    pub const fn teardown(&self) -> PresentationTeardownOutcome {
        self.teardown
    }

    /// Exact immutable profile which failed during startup.
    #[must_use]
    pub const fn capabilities(&self) -> LinuxPresentationCapabilities {
        self.teardown.capabilities()
    }

    /// Consumes the failure without discarding either result.
    #[must_use]
    pub fn into_parts(self) -> (PresentationError, PresentationTeardownOutcome) {
        (self.primary, self.teardown)
    }
}

impl fmt::Display for PresentationStartupFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}; partial-owner teardown: {:?}",
            self.primary, self.teardown
        )
    }
}

impl Error for PresentationStartupFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.primary)
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PresentationContract {
    descriptor: SurfaceDescriptor,
    capabilities: LinuxPresentationCapabilities,
    limits: PresentationLimits,
    state: PresentationState,
    submitted_frames: u64,
    last_sequence: Option<u64>,
}

impl PresentationContract {
    #[cfg(test)]
    pub(crate) fn new(
        descriptor: SurfaceDescriptor,
        limits: PresentationLimits,
    ) -> Result<Self, PresentationError> {
        Self::new_with_capabilities(
            descriptor,
            limits,
            LinuxPresentationCapabilities::STRICT_HARDWARE,
        )
    }

    pub(crate) fn new_with_capabilities(
        descriptor: SurfaceDescriptor,
        limits: PresentationLimits,
        capabilities: LinuxPresentationCapabilities,
    ) -> Result<Self, PresentationError> {
        if descriptor.role != SurfaceRole::Window || descriptor.format != PixelFormat::Rgba8Srgb {
            return Err(PresentationError::contract(
                PresentationFailureStage::ValidateSurface,
                PresentationErrorKind::UnsupportedContract,
                "presenter requires a Window surface in Rgba8Srgb format",
            )
            .with_capabilities(capabilities));
        }
        limits
            .rgba8_bytes(descriptor.size)
            .map_err(|error| error.with_capabilities(capabilities))?;
        let state = if descriptor.size.width == 0 || descriptor.size.height == 0 {
            PresentationState::Suspended
        } else {
            PresentationState::Active
        };
        Ok(Self {
            descriptor,
            capabilities,
            limits,
            state,
            submitted_frames: 0,
            last_sequence: None,
        })
    }

    pub(crate) const fn descriptor(&self) -> SurfaceDescriptor {
        self.descriptor
    }

    pub(crate) const fn capabilities(&self) -> LinuxPresentationCapabilities {
        self.capabilities
    }

    pub(crate) const fn state(&self) -> PresentationState {
        self.state
    }

    pub(crate) fn check_live(&self, surface: SurfaceId) -> Result<(), PresentationError> {
        match self.state {
            PresentationState::Lost(stage) => Err(PresentationError::contract(
                stage,
                PresentationErrorKind::TerminalState,
                "presenter is permanently lost",
            )
            .with_capabilities(self.capabilities)),
            PresentationState::Shutdown => Err(PresentationError::contract(
                PresentationFailureStage::ValidateSurface,
                PresentationErrorKind::TerminalState,
                "presenter is shut down",
            )
            .with_capabilities(self.capabilities)),
            PresentationState::Active | PresentationState::Suspended => {
                if surface == self.descriptor.id {
                    Ok(())
                } else {
                    Err(PresentationError::contract(
                        PresentationFailureStage::ValidateSurface,
                        PresentationErrorKind::SurfaceMismatch,
                        "surface identity does not match the owned presenter",
                    )
                    .with_capabilities(self.capabilities))
                }
            }
        }
    }

    pub(crate) fn check_resize(
        &self,
        surface: SurfaceId,
        size: PhysicalSize,
    ) -> Result<u64, PresentationError> {
        self.check_live(surface)?;
        self.limits
            .rgba8_bytes(size)
            .map_err(|error| error.with_capabilities(self.capabilities))
    }

    pub(crate) fn commit_resize(&mut self, size: PhysicalSize) {
        if matches!(
            self.state,
            PresentationState::Lost(_) | PresentationState::Shutdown
        ) {
            return;
        }
        self.descriptor.size = size;
        self.state = if size.width == 0 || size.height == 0 {
            PresentationState::Suspended
        } else {
            PresentationState::Active
        };
    }

    pub(crate) fn update_scale(
        &mut self,
        surface: SurfaceId,
        scale: ScaleFactor,
    ) -> Result<(), PresentationError> {
        self.check_live(surface)?;
        self.descriptor.scale = scale;
        Ok(())
    }

    pub(crate) fn suspend(&mut self) {
        if matches!(self.state, PresentationState::Active) {
            self.state = PresentationState::Suspended;
        }
    }

    pub(crate) fn resume(&mut self) {
        if matches!(self.state, PresentationState::Suspended)
            && self.descriptor.size.width != 0
            && self.descriptor.size.height != 0
        {
            self.state = PresentationState::Active;
        }
    }

    pub(crate) fn admit_frame(
        &self,
        request: DirectFrameRequest,
    ) -> Result<u64, PresentationError> {
        self.check_live(request.surface)?;
        if self.state == PresentationState::Suspended {
            return Err(PresentationError::contract(
                PresentationFailureStage::ValidateSurface,
                PresentationErrorKind::Suspended,
                "zero-sized or suspended surface cannot accept a frame",
            )
            .with_capabilities(self.capabilities));
        }
        if request.size != self.descriptor.size {
            return Err(PresentationError::contract(
                PresentationFailureStage::ValidateSurface,
                PresentationErrorKind::SizeMismatch,
                format!(
                    "frame size {:?} does not match live surface {:?}",
                    request.size, self.descriptor.size
                ),
            )
            .with_capabilities(self.capabilities));
        }
        if request.sequence == 0
            || self
                .last_sequence
                .is_some_and(|last| request.sequence <= last)
            || self.submitted_frames >= self.limits.frames
        {
            return Err(PresentationError::contract(
                PresentationFailureStage::ValidateSurface,
                PresentationErrorKind::FrameSequence,
                "frame sequence must be nonzero, monotonic, and within the lifetime cap",
            )
            .with_capabilities(self.capabilities));
        }
        self.limits
            .rgba8_bytes(request.size)
            .map_err(|error| error.with_capabilities(self.capabilities))
    }

    pub(crate) fn commit_frame(&mut self, sequence: u64) {
        self.submitted_frames += 1;
        self.last_sequence = Some(sequence);
    }

    pub(crate) fn lose(&mut self, stage: PresentationFailureStage) {
        if matches!(
            self.state,
            PresentationState::Active | PresentationState::Suspended
        ) {
            self.state = PresentationState::Lost(stage);
        }
    }

    pub(crate) fn shutdown(&mut self) -> PresentationShutdownReport {
        self.state = PresentationState::Shutdown;
        PresentationShutdownReport::new_with_capabilities(
            self.descriptor.id,
            self.submitted_frames,
            self.last_sequence,
            self.capabilities,
        )
    }

    pub(crate) const fn retention(
        &self,
        failure: &PresentationError,
    ) -> PresentationRetentionReport {
        PresentationRetentionReport::new_with_capabilities(
            self.descriptor.id,
            self.submitted_frames,
            self.last_sequence,
            failure.stage(),
            failure.kind(),
            self.capabilities,
        )
    }
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
    use super::{
        DirectFrameRequest, LinuxAccelerationClass, LinuxPresentationCapabilities,
        LinuxResetProtection, MAX_ERROR_DETAIL_BYTES, PresentationContract, PresentationError,
        PresentationErrorKind, PresentationFailureStage, PresentationLimits, PresentationState,
        SwapSubmissionReceipt,
    };
    use wild_buzzard_platform::{
        PhysicalSize, PixelFormat, ScaleFactor, SurfaceDescriptor, SurfaceIdAllocator,
        SurfaceNamespace, SurfaceRole,
    };

    fn descriptor() -> (SurfaceDescriptor, SurfaceIdAllocator) {
        let mut allocator = SurfaceIdAllocator::new(SurfaceNamespace::new(4_401).unwrap());
        let descriptor = SurfaceDescriptor {
            id: allocator.allocate().unwrap(),
            size: PhysicalSize::new(800, 600).unwrap(),
            scale: ScaleFactor::new(1.0).unwrap(),
            format: PixelFormat::Rgba8Srgb,
            role: SurfaceRole::Window,
        };
        (descriptor, allocator)
    }

    #[test]
    fn capability_values_bind_errors_receipts_and_terminal_reports() {
        let (descriptor, _) = descriptor();
        let capabilities = LinuxPresentationCapabilities::new(
            LinuxAccelerationClass::Software,
            LinuxResetProtection::Unavailable,
        );
        let mut contract = PresentationContract::new_with_capabilities(
            descriptor,
            PresentationLimits::default(),
            capabilities,
        )
        .unwrap();
        let wrong_size =
            DirectFrameRequest::new(descriptor.id, PhysicalSize::new(801, 600).unwrap(), 1);
        assert_eq!(
            contract.admit_frame(wrong_size).unwrap_err().capabilities(),
            Some(capabilities)
        );

        let request = DirectFrameRequest::new(descriptor.id, descriptor.size, 1);
        let receipt = SwapSubmissionReceipt::new(request, capabilities, 1_920_000, [1, 2, 3, 4]);
        assert_eq!(receipt.capabilities(), capabilities);
        contract.admit_frame(request).unwrap();
        contract.commit_frame(1);
        let shutdown = contract.shutdown();
        assert_eq!(shutdown.capabilities(), capabilities);
    }

    #[test]
    fn fixed_limits_reject_oversized_surfaces() {
        let (mut descriptor, _) = descriptor();
        descriptor.size = PhysicalSize::new(16_385, 1).unwrap();
        let error =
            PresentationContract::new(descriptor, PresentationLimits::default()).unwrap_err();
        assert_eq!(error.kind(), PresentationErrorKind::ResourceLimit);
    }

    #[test]
    fn exact_window_and_rgba8_srgb_contract_is_required() {
        let (mut descriptor, _) = descriptor();
        descriptor.role = SurfaceRole::Offscreen;
        let error =
            PresentationContract::new(descriptor, PresentationLimits::default()).unwrap_err();
        assert_eq!(error.kind(), PresentationErrorKind::UnsupportedContract);

        descriptor.role = SurfaceRole::Window;
        descriptor.format = PixelFormat::Rgba16Float;
        let error =
            PresentationContract::new(descriptor, PresentationLimits::default()).unwrap_err();
        assert_eq!(error.kind(), PresentationErrorKind::UnsupportedContract);
    }

    #[test]
    fn zero_size_suspends_and_nonzero_resize_resumes_exact_identity() {
        let (descriptor, mut allocator) = descriptor();
        let mut contract =
            PresentationContract::new(descriptor, PresentationLimits::default()).unwrap();
        let zero = PhysicalSize::new(0, 0).unwrap();
        contract.check_resize(descriptor.id, zero).unwrap();
        contract.commit_resize(zero);
        assert_eq!(contract.state(), PresentationState::Suspended);

        let resumed = PhysicalSize::new(1_024, 768).unwrap();
        contract.check_resize(descriptor.id, resumed).unwrap();
        contract.commit_resize(resumed);
        assert_eq!(contract.state(), PresentationState::Active);

        allocator.release(descriptor.id).unwrap();
        let stale = allocator.allocate().unwrap();
        let error = contract.check_resize(stale, resumed).unwrap_err();
        assert_eq!(error.kind(), PresentationErrorKind::SurfaceMismatch);
    }

    #[test]
    fn failed_frame_admission_is_atomic_and_sequences_are_monotonic() {
        let (descriptor, _) = descriptor();
        let mut contract =
            PresentationContract::new(descriptor, PresentationLimits::default()).unwrap();
        let first = DirectFrameRequest::new(descriptor.id, descriptor.size, 9);
        contract.admit_frame(first).unwrap();
        contract.commit_frame(first.sequence());

        let repeated = contract.admit_frame(first).unwrap_err();
        assert_eq!(repeated.kind(), PresentationErrorKind::FrameSequence);
        let next = DirectFrameRequest::new(descriptor.id, descriptor.size, 10);
        contract.admit_frame(next).unwrap();
        contract.commit_frame(next.sequence());
        let report = contract.shutdown();
        assert_eq!(report.submitted_frames(), 2);
        assert_eq!(report.last_sequence(), Some(10));
    }

    #[test]
    fn lifetime_frame_cap_rejects_before_counter_wrap() {
        let (descriptor, _) = descriptor();
        let limits = PresentationLimits {
            frames: 1,
            ..PresentationLimits::default()
        };
        let mut contract = PresentationContract::new(descriptor, limits).unwrap();
        let first = DirectFrameRequest::new(descriptor.id, descriptor.size, 1);
        contract.admit_frame(first).unwrap();
        contract.commit_frame(first.sequence());

        let error = contract
            .admit_frame(DirectFrameRequest::new(descriptor.id, descriptor.size, 2))
            .unwrap_err();
        assert_eq!(error.kind(), PresentationErrorKind::FrameSequence);
    }

    #[test]
    fn frame_admission_rejects_zero_sequence_and_inexact_size() {
        let (descriptor, _) = descriptor();
        let contract =
            PresentationContract::new(descriptor, PresentationLimits::default()).unwrap();

        let zero = DirectFrameRequest::new(descriptor.id, descriptor.size, 0);
        assert_eq!(
            contract.admit_frame(zero).unwrap_err().kind(),
            PresentationErrorKind::FrameSequence
        );

        let wrong_size =
            DirectFrameRequest::new(descriptor.id, PhysicalSize::new(801, 600).unwrap(), 1);
        assert_eq!(
            contract.admit_frame(wrong_size).unwrap_err().kind(),
            PresentationErrorKind::SizeMismatch
        );
    }

    #[test]
    fn scale_change_preserves_exact_physical_size() {
        let (descriptor, _) = descriptor();
        let mut contract =
            PresentationContract::new(descriptor, PresentationLimits::default()).unwrap();
        let scale = ScaleFactor::new(2.0).unwrap();
        contract.update_scale(descriptor.id, scale).unwrap();
        assert_eq!(contract.descriptor().size, descriptor.size);
        assert_eq!(contract.descriptor().scale, scale);
    }

    #[test]
    fn diagnostic_details_are_utf8_safe_and_strictly_bounded() {
        let oversized = "🦅".repeat(MAX_ERROR_DETAIL_BYTES);
        let error = PresentationError::contract(
            PresentationFailureStage::DrawFrame,
            PresentationErrorKind::Driver,
            oversized,
        );
        assert!(error.detail().len() <= MAX_ERROR_DETAIL_BYTES);
        assert!(error.detail().ends_with("..."));
        assert!(error.detail().is_char_boundary(error.detail().len()));
    }

    #[test]
    fn native_diagnostic_mismatch_is_terminal_but_renderer_rejection_is_not() {
        let mismatch = PresentationError::contract(
            PresentationFailureStage::DrawFrame,
            PresentationErrorKind::DiagnosticMismatch,
            "native back-buffer sample disagreed",
        );
        assert!(mismatch.is_terminal());

        let rejected = PresentationError::contract(
            PresentationFailureStage::DrawFrame,
            PresentationErrorKind::RendererRejected,
            "renderer declined this frame",
        );
        assert!(!rejected.is_terminal());
    }

    #[test]
    fn terminal_loss_cannot_be_resumed_or_replaced() {
        let (descriptor, _) = descriptor();
        let mut contract =
            PresentationContract::new(descriptor, PresentationLimits::default()).unwrap();
        contract.lose(PresentationFailureStage::SwapBuffers);
        contract.resume();
        contract.commit_resize(descriptor.size);
        assert_eq!(
            contract.state(),
            PresentationState::Lost(PresentationFailureStage::SwapBuffers)
        );

        contract.lose(PresentationFailureStage::MakeCurrent);
        assert_eq!(
            contract.state(),
            PresentationState::Lost(PresentationFailureStage::SwapBuffers)
        );
    }
}
