use std::fmt;
use std::time::Duration;

/// A resource class bounded at the headless renderer boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResourceKind {
    /// Offscreen framebuffer width.
    FrameWidth,
    /// Offscreen framebuffer height.
    FrameHeight,
    /// Owned RGBA8 byte count.
    PixelBytes,
    /// Validated scene item count.
    SceneItems,
    /// Pending text-resource count.
    PendingTextRuns,
    /// Serialized `WebRender` display-list bytes.
    DisplayListBytes,
}

/// A bounded asynchronous frame stage.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrameStage {
    /// `WebRender`'s render backend built the frame.
    FrameBuilt,
    /// `WebRender` published the renderer work and requested a render-thread wake.
    FrameReady,
    /// The renderer submitted the built frame to GL.
    FrameRendered,
    /// `WebRender`'s backend thread acknowledged shutdown.
    Shutdown,
}

/// Linux EGL context source used by an initialization attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ContextBackend {
    /// A display created from an enumerated EGL device.
    EglDevice {
        /// Stable index in EGL's returned device list.
        index: usize,
        /// Driver-provided device name, if available.
        name: Option<String>,
    },
    /// EGL's default X11 display, used only after device attempts fail.
    X11Default,
}

/// EGL/OpenGL initialization stage associated with a diagnostic.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContextStep {
    /// Loading and enumerating EGL devices.
    EnumerateDevices,
    /// Creating and initializing an EGL display.
    CreateDisplay,
    /// Selecting an RGBA8 pbuffer-capable configuration.
    SelectConfig,
    /// Creating a desktop OpenGL context.
    CreateContext,
    /// Creating the fixed-size EGL pbuffer.
    CreateSurface,
    /// Making the context and pbuffer current.
    MakeCurrent,
    /// Loading desktop OpenGL entry points through EGL.
    LoadFunctions,
    /// Releasing a current context after later initialization failed.
    ReleaseAfterFailure,
}

/// One bounded context-initialization diagnostic.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextAttempt {
    backend: Option<ContextBackend>,
    step: ContextStep,
    detail: String,
}

impl ContextAttempt {
    pub(crate) fn new(
        backend: Option<ContextBackend>,
        step: ContextStep,
        detail: impl fmt::Display,
    ) -> Self {
        const MAX_DIAGNOSTIC_BYTES: usize = 1_024;
        let mut detail = detail.to_string();
        if detail.len() > MAX_DIAGNOSTIC_BYTES {
            let mut boundary = MAX_DIAGNOSTIC_BYTES;
            while !detail.is_char_boundary(boundary) {
                boundary -= 1;
            }
            detail.truncate(boundary);
            detail.push_str("...");
        }
        Self {
            backend,
            step,
            detail,
        }
    }

    /// Returns the context source, if one had been selected.
    #[must_use]
    pub const fn backend(&self) -> Option<&ContextBackend> {
        self.backend.as_ref()
    }

    /// Returns the failing initialization stage.
    #[must_use]
    pub const fn step(&self) -> ContextStep {
        self.step
    }

    /// Returns the bounded driver/library diagnostic.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

/// Structured failures from validation, EGL, `WebRender`, readback, or teardown.
#[derive(Debug)]
pub enum HeadlessError {
    /// A frame dimension was zero or cannot be represented by `WebRender`.
    InvalidFrameSize {
        /// Requested width.
        width: u32,
        /// Requested height.
        height: u32,
    },
    /// A configured resource maximum is internally invalid.
    InvalidLimit {
        /// Invalid field name.
        field: &'static str,
        /// Rejected value.
        value: u128,
    },
    /// A resource exceeded its configured maximum.
    ResourceLimitExceeded {
        /// Resource class.
        resource: ResourceKind,
        /// Observed count or byte size.
        observed: usize,
        /// Configured maximum.
        limit: usize,
    },
    /// Exact RGBA8 size computation overflowed.
    PixelSizeOverflow {
        /// Requested width.
        width: u32,
        /// Requested height.
        height: u32,
    },
    /// A first-party allocation could not reserve the exact frame byte count.
    PixelAllocationFailed {
        /// Requested bytes.
        requested: usize,
    },
    /// The scene viewport is not an exact number of CSS pixels at scale 1.
    FractionalViewport {
        /// Width in app units.
        width_app_units: i32,
        /// Height in app units.
        height_app_units: i32,
    },
    /// The scene viewport differs from the fixed EGL pbuffer.
    ViewportMismatch {
        /// Scene width in device pixels.
        scene_width: u32,
        /// Scene height in device pixels.
        scene_height: u32,
        /// Pbuffer width.
        frame_width: u32,
        /// Pbuffer height.
        frame_height: u32,
    },
    /// The caller expected a different immutable document revision.
    StaleRevision {
        /// Requested revision.
        expected: u64,
        /// Scene revision.
        actual: u64,
    },
    /// A scene older than the most recently submitted scene was rejected.
    RevisionRegressed {
        /// Most recently submitted revision.
        previous: u64,
        /// Rejected revision.
        actual: u64,
    },
    /// `WebRender` epochs must increase within this renderer instance.
    StaleEpoch {
        /// Last accepted epoch.
        previous: u32,
        /// Rejected epoch.
        actual: u32,
    },
    /// Every permitted Linux EGL initialization path failed.
    ContextUnavailable {
        /// Bounded attempt diagnostics in execution order.
        attempts: Vec<ContextAttempt>,
    },
    /// A GL function name unexpectedly contained an interior NUL.
    InvalidGlSymbol,
    /// `WebRender` could not initialize on the current GL context.
    RendererInitialization {
        /// Bounded renderer diagnostic.
        detail: String,
    },
    /// `WebRender` failed while drawing the current frame.
    RenderFailed {
        /// Bounded renderer diagnostic.
        detail: String,
    },
    /// `WebRender` panicked while deleting GL resources during teardown.
    RendererDeinitialization {
        /// Bounded panic diagnostic.
        detail: String,
    },
    /// The renderer's own EGL context could not be made current before GL use.
    ContextActivation {
        /// Bounded EGL diagnostic or panic payload.
        detail: String,
    },
    /// The renderer/backend channel disconnected while submitting work.
    BackendDisconnected,
    /// `WebRender` requested a renderer-thread external event that this boundary
    /// never registers.
    UnexpectedExternalEvent,
    /// A requested asynchronous stage did not complete by its deadline.
    FrameTimeout {
        /// Stage that timed out.
        stage: FrameStage,
        /// Configured total frame or shutdown budget.
        timeout: Duration,
    },
    /// A `WebRender` transaction was dropped before the requested stage.
    TransactionDropped {
        /// Stage the caller was awaiting.
        expected: FrameStage,
    },
    /// The fixed-capacity notification path overflowed.
    NotificationOverflow,
    /// The built frame did not publish the submitted pipeline epoch.
    EpochNotPublished {
        /// Submitted epoch.
        expected: u32,
        /// Published epoch, if any.
        actual: Option<u32>,
    },
    /// A previous asynchronous or GL failure makes further submissions unsafe.
    RendererUnusable,
    /// EGL could not safely release a current context during construction failure or teardown.
    ContextRelease {
        /// Bounded EGL diagnostic.
        detail: String,
    },
    /// Rendering was attempted after explicit shutdown.
    AlreadyShutdown,
}

impl HeadlessError {
    pub(crate) fn renderer_initialization(detail: impl fmt::Debug) -> Self {
        Self::RendererInitialization {
            detail: bounded_debug(detail),
        }
    }

    pub(crate) fn render_failed(detail: impl fmt::Debug) -> Self {
        Self::RenderFailed {
            detail: bounded_debug(detail),
        }
    }
}

fn bounded_debug(value: impl fmt::Debug) -> String {
    const MAX_DIAGNOSTIC_BYTES: usize = 4_096;
    let mut detail = format!("{value:?}");
    if detail.len() > MAX_DIAGNOSTIC_BYTES {
        let mut boundary = MAX_DIAGNOSTIC_BYTES;
        while !detail.is_char_boundary(boundary) {
            boundary -= 1;
        }
        detail.truncate(boundary);
        detail.push_str("...");
    }
    detail
}

impl fmt::Display for HeadlessError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidFrameSize { width, height } => {
                write!(formatter, "invalid headless frame size {width}x{height}")
            }
            Self::InvalidLimit { field, value } => {
                write!(formatter, "invalid headless limit {field}={value}")
            }
            Self::ResourceLimitExceeded {
                resource,
                observed,
                limit,
            } => write!(
                formatter,
                "{resource:?} resource {observed} exceeds configured limit {limit}"
            ),
            Self::PixelSizeOverflow { width, height } => {
                write!(formatter, "RGBA8 byte size overflows for {width}x{height}")
            }
            Self::PixelAllocationFailed { requested } => {
                write!(formatter, "could not reserve {requested} RGBA8 bytes")
            }
            Self::FractionalViewport {
                width_app_units,
                height_app_units,
            } => write!(
                formatter,
                "scene viewport {width_app_units}x{height_app_units} app units is not integral at device scale 1"
            ),
            Self::ViewportMismatch {
                scene_width,
                scene_height,
                frame_width,
                frame_height,
            } => write!(
                formatter,
                "scene viewport {scene_width}x{scene_height} differs from pbuffer {frame_width}x{frame_height}"
            ),
            Self::StaleRevision { expected, actual } => write!(
                formatter,
                "scene revision {actual} does not match requested revision {expected}"
            ),
            Self::RevisionRegressed { previous, actual } => write!(
                formatter,
                "scene revision regressed from {previous} to {actual}"
            ),
            Self::StaleEpoch { previous, actual } => write!(
                formatter,
                "WebRender epoch {actual} is not newer than {previous}"
            ),
            Self::ContextUnavailable { attempts } => write!(
                formatter,
                "no usable Linux EGL context after {} attempt(s)",
                attempts.len()
            ),
            Self::InvalidGlSymbol => formatter.write_str("invalid OpenGL symbol name"),
            Self::RendererInitialization { detail } => {
                write!(formatter, "WebRender initialization failed: {detail}")
            }
            Self::RenderFailed { detail } => write!(formatter, "WebRender render failed: {detail}"),
            Self::RendererDeinitialization { detail } => {
                write!(formatter, "WebRender deinitialization failed: {detail}")
            }
            Self::ContextActivation { detail } => {
                write!(
                    formatter,
                    "could not activate the renderer's EGL context: {detail}"
                )
            }
            Self::BackendDisconnected => {
                formatter.write_str("WebRender backend disconnected during submission")
            }
            Self::UnexpectedExternalEvent => formatter
                .write_str("WebRender emitted an unexpected external renderer-thread event"),
            Self::FrameTimeout { stage, timeout } => {
                write!(formatter, "{stage:?} timed out after {timeout:?}")
            }
            Self::TransactionDropped { expected } => {
                write!(formatter, "transaction dropped before {expected:?}")
            }
            Self::NotificationOverflow => {
                formatter.write_str("fixed-capacity frame notification queue overflowed")
            }
            Self::EpochNotPublished { expected, actual } => write!(
                formatter,
                "submitted epoch {expected} was not published (actual {actual:?})"
            ),
            Self::RendererUnusable => {
                formatter.write_str("headless renderer is unusable after a previous failure")
            }
            Self::ContextRelease { detail } => {
                write!(formatter, "could not release current EGL context: {detail}")
            }
            Self::AlreadyShutdown => formatter.write_str("headless renderer is shut down"),
        }
    }
}

impl std::error::Error for HeadlessError {}
