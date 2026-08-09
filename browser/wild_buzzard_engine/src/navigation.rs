//! Bounded, generation-checked navigation and presentation facade.
//!
//! The facade owns one synchronous executor on one dedicated worker thread. It
//! deliberately does not expose DOM, layout, renderer, headless, or platform
//! window types. A successful result is published as an opaque frame lease only
//! while its navigation generation is still current.

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::num::{NonZeroU64, NonZeroUsize};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread::{self, JoinHandle};

use wild_buzzard_headless::RgbaFrame;

use crate::{
    CancellationToken, CompositionStatus, PipelineError, PipelineStage, RenderedStaticPage,
    StaticPageConfig, StaticPageEngine,
};

/// Hard upper bound for one user-supplied navigation URL.
pub const MAX_NAVIGATION_URL_BYTES: usize = 16 * 1024;

const MAX_QUEUE_CAPACITY: usize = 4_096;
const MAX_CONTEXTS: usize = 1_024;
const MAX_FRAME_BYTES: usize = 256 * 1024 * 1024;
const RGBA8_BYTES_PER_PIXEL: usize = 4;

/// Opaque identity for one top-level browsing context.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TopLevelContextId(NonZeroU64);

impl TopLevelContextId {
    /// Creates an identity. Zero is permanently reserved as invalid.
    #[must_use]
    pub const fn new(raw: u64) -> Option<Self> {
        match NonZeroU64::new(raw) {
            Some(raw) => Some(Self(raw)),
            None => None,
        }
    }

    /// Returns the opaque numeric representation for diagnostics or transport.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// Monotonic generation within one [`TopLevelContextId`].
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct NavigationGeneration(NonZeroU64);

impl NavigationGeneration {
    /// First accepted generation for a newly admitted context.
    pub const INITIAL: Self = Self(NonZeroU64::MIN);

    /// Creates a nonzero generation.
    #[must_use]
    pub const fn new(raw: u64) -> Option<Self> {
        match NonZeroU64::new(raw) {
            Some(raw) => Some(Self(raw)),
            None => None,
        }
    }

    /// Returns the numeric generation.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }

    /// Returns the next generation, or `None` after permanent exhaustion.
    #[must_use]
    pub const fn checked_next(self) -> Option<Self> {
        match self.get().checked_add(1) {
            Some(raw) => Self::new(raw),
            None => None,
        }
    }
}

/// Exact context and generation of one navigation.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct NavigationId {
    context: TopLevelContextId,
    generation: NavigationGeneration,
}

impl NavigationId {
    /// Creates a typed navigation identity.
    #[must_use]
    pub const fn new(context: TopLevelContextId, generation: NavigationGeneration) -> Self {
        Self {
            context,
            generation,
        }
    }

    /// Returns the top-level context.
    #[must_use]
    pub const fn context(self) -> TopLevelContextId {
        self.context
    }

    /// Returns the context-local generation.
    #[must_use]
    pub const fn generation(self) -> NavigationGeneration {
        self.generation
    }
}

/// A bounded, owned navigation request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NavigationRequest {
    url: Box<str>,
}

impl NavigationRequest {
    /// Copies a nonempty URL after enforcing the hard byte bound.
    ///
    /// # Errors
    ///
    /// Returns [`NavigationRequestError`] for an empty or oversized URL.
    pub fn new(url: &str) -> Result<Self, NavigationRequestError> {
        if url.is_empty() {
            return Err(NavigationRequestError::EmptyUrl);
        }
        if url.len() > MAX_NAVIGATION_URL_BYTES {
            return Err(NavigationRequestError::UrlTooLong {
                actual: url.len(),
                maximum: MAX_NAVIGATION_URL_BYTES,
            });
        }
        Ok(Self { url: url.into() })
    }

    /// Returns the requested URL without transferring ownership.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }
}

/// Validation failure while constructing a bounded navigation request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NavigationRequestError {
    /// The URL is empty.
    EmptyUrl,
    /// The URL exceeds the hard byte bound.
    UrlTooLong {
        /// Supplied byte length.
        actual: usize,
        /// Accepted byte length.
        maximum: usize,
    },
}

impl fmt::Display for NavigationRequestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyUrl => formatter.write_str("navigation URL must not be empty"),
            Self::UrlTooLong { actual, maximum } => {
                write!(
                    formatter,
                    "navigation URL has {actual} bytes; maximum is {maximum}"
                )
            }
        }
    }
}

impl std::error::Error for NavigationRequestError {}

/// Fixed resource bounds for one navigation worker.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EngineLimits {
    command_capacity: NonZeroUsize,
    event_capacity: NonZeroUsize,
    max_contexts: NonZeroUsize,
    max_frame_bytes: NonZeroUsize,
    max_retained_frame_bytes: NonZeroUsize,
}

impl EngineLimits {
    /// Creates checked worker, context, and retained-frame bounds.
    ///
    /// `event_capacity` must be at least three so one undrained started event
    /// cannot prevent the indivisible commit and frame-ready transaction.
    ///
    /// # Errors
    ///
    /// Returns [`EngineLimitsError`] when a value is zero, above its hard cap,
    /// or cannot retain at least one maximum-sized frame.
    pub fn new(
        command_capacity: usize,
        event_capacity: usize,
        max_contexts: usize,
        max_frame_bytes: usize,
        max_retained_frame_bytes: usize,
    ) -> Result<Self, EngineLimitsError> {
        let command_capacity =
            checked_nonzero_bounded("command_capacity", command_capacity, MAX_QUEUE_CAPACITY)?;
        let event_capacity =
            checked_nonzero_bounded("event_capacity", event_capacity, MAX_QUEUE_CAPACITY)?;
        if event_capacity.get() < 3 {
            return Err(EngineLimitsError::TooSmall {
                field: "event_capacity",
                actual: event_capacity.get(),
                minimum: 3,
            });
        }
        let max_contexts = checked_nonzero_bounded("max_contexts", max_contexts, MAX_CONTEXTS)?;
        let max_frame_bytes =
            checked_nonzero_bounded("max_frame_bytes", max_frame_bytes, MAX_FRAME_BYTES)?;
        let max_retained_frame_bytes = checked_nonzero_bounded(
            "max_retained_frame_bytes",
            max_retained_frame_bytes,
            MAX_FRAME_BYTES,
        )?;
        if max_retained_frame_bytes < max_frame_bytes {
            return Err(EngineLimitsError::TooSmall {
                field: "max_retained_frame_bytes",
                actual: max_retained_frame_bytes.get(),
                minimum: max_frame_bytes.get(),
            });
        }
        Ok(Self {
            command_capacity,
            event_capacity,
            max_contexts,
            max_frame_bytes,
            max_retained_frame_bytes,
        })
    }

    /// Maximum queued navigations, excluding the one executing.
    #[must_use]
    pub const fn command_capacity(self) -> usize {
        self.command_capacity.get()
    }

    /// Maximum ordinary queued events. One terminal slot is reserved separately.
    #[must_use]
    pub const fn event_capacity(self) -> usize {
        self.event_capacity.get()
    }

    /// Maximum number of admitted top-level contexts.
    #[must_use]
    pub const fn max_contexts(self) -> usize {
        self.max_contexts.get()
    }

    /// Maximum bytes in one executor frame, including its optional glyph proof.
    #[must_use]
    pub const fn max_frame_bytes(self) -> usize {
        self.max_frame_bytes.get()
    }

    /// Maximum aggregate bytes retained behind all current frame leases.
    #[must_use]
    pub const fn max_retained_frame_bytes(self) -> usize {
        self.max_retained_frame_bytes.get()
    }
}

impl Default for EngineLimits {
    fn default() -> Self {
        Self::new(16, 32, 16, 128 * 1024 * 1024, 256 * 1024 * 1024)
            .expect("default engine limits are valid")
    }
}

/// Invalid engine bound.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EngineLimitsError {
    /// A required bound is zero.
    Zero { field: &'static str },
    /// A bound exceeds its hard cap.
    TooLarge {
        field: &'static str,
        actual: usize,
        maximum: usize,
    },
    /// A bound is below a required minimum.
    TooSmall {
        field: &'static str,
        actual: usize,
        minimum: usize,
    },
}

impl fmt::Display for EngineLimitsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Zero { field } => write!(formatter, "{field} must be nonzero"),
            Self::TooLarge {
                field,
                actual,
                maximum,
            } => {
                write!(formatter, "{field} is {actual}; maximum is {maximum}")
            }
            Self::TooSmall {
                field,
                actual,
                minimum,
            } => {
                write!(formatter, "{field} is {actual}; minimum is {minimum}")
            }
        }
    }
}

impl std::error::Error for EngineLimitsError {}

fn checked_nonzero_bounded(
    field: &'static str,
    value: usize,
    maximum: usize,
) -> Result<NonZeroUsize, EngineLimitsError> {
    let value = NonZeroUsize::new(value).ok_or(EngineLimitsError::Zero { field })?;
    if value.get() > maximum {
        return Err(EngineLimitsError::TooLarge {
            field,
            actual: value.get(),
            maximum,
        });
    }
    Ok(value)
}

/// Typed commands accepted by [`NavigationEngine::try_send`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EngineCommand {
    /// Queue a bounded navigation at an explicitly supplied generation.
    Navigate {
        /// Typed context/generation identity.
        navigation: NavigationId,
        /// Bounded URL request.
        request: NavigationRequest,
    },
    /// Request cancellation of the current matching generation.
    Cancel {
        /// Exact navigation to cancel.
        navigation: NavigationId,
    },
    /// Request deterministic worker shutdown.
    Shutdown,
}

/// Result of accepting a command.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandReceipt {
    /// Navigation entered the bounded work queue.
    NavigationQueued(NavigationId),
    /// Cancellation changed a live token from active to cancelled.
    CancellationRequested(NavigationId),
    /// Shutdown was requested; repeated requests are reported without side effects.
    ShutdownRequested { already_requested: bool },
}

/// Reason a command was rejected without changing navigation state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandErrorKind {
    /// The bounded navigation queue is full.
    QueueFull { capacity: usize },
    /// A new context did not start at generation one.
    InitialGenerationRequired,
    /// A generation was not strictly newer than the last accepted generation.
    NonMonotonicGeneration { latest: NavigationGeneration },
    /// No greater generation can be represented.
    GenerationExhausted,
    /// The configured number of live contexts has been reached.
    ContextLimitReached { maximum: usize },
    /// The context has never been admitted.
    UnknownContext,
    /// The cancellation target is not the current active generation.
    NotCurrentNavigation,
    /// No cancellable work remains for the current generation.
    NoActiveNavigation,
    /// The event receiver was dropped.
    EventReceiverDropped,
    /// The worker is shutting down or stopped.
    ShuttingDown,
}

/// Rejected command plus its typed reason.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandError {
    kind: CommandErrorKind,
    command: EngineCommand,
}

impl CommandError {
    /// Returns the rejection reason.
    #[must_use]
    pub const fn kind(&self) -> CommandErrorKind {
        self.kind
    }

    /// Recovers ownership of the rejected command.
    #[must_use]
    pub fn into_command(self) -> EngineCommand {
        self.command
    }
}

impl fmt::Display for CommandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "engine command rejected: {:?}", self.kind)
    }
}

impl std::error::Error for CommandError {}

/// UI-neutral device-pixel dimensions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PixelSize {
    width: u32,
    height: u32,
}

impl PixelSize {
    /// Creates nonzero dimensions with a representable RGBA8 length.
    ///
    /// # Errors
    ///
    /// Returns [`EngineFrameError`] for zero dimensions or byte overflow.
    pub fn new(width: u32, height: u32) -> Result<Self, EngineFrameError> {
        let size = Self { width, height };
        size.rgba8_len()?;
        Ok(size)
    }

    /// Width in device pixels.
    #[must_use]
    pub const fn width(self) -> u32 {
        self.width
    }

    /// Height in device pixels.
    #[must_use]
    pub const fn height(self) -> u32 {
        self.height
    }

    fn rgba8_len(self) -> Result<usize, EngineFrameError> {
        if self.width == 0 || self.height == 0 {
            return Err(EngineFrameError::InvalidSize {
                width: self.width,
                height: self.height,
            });
        }
        let pixels = usize::try_from(self.width)
            .ok()
            .and_then(|width| {
                usize::try_from(self.height)
                    .ok()
                    .and_then(|height| width.checked_mul(height))
            })
            .ok_or(EngineFrameError::ByteLengthOverflow)?;
        pixels
            .checked_mul(RGBA8_BYTES_PER_PIXEL)
            .ok_or(EngineFrameError::ByteLengthOverflow)
    }
}

/// Metadata for top-left row-order RGBA8 bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rgba8Metadata {
    size: PixelSize,
    stride: usize,
    byte_len: usize,
}

impl Rgba8Metadata {
    fn checked(size: PixelSize, byte_len: usize) -> Result<Self, EngineFrameError> {
        let expected = size.rgba8_len()?;
        if byte_len != expected {
            return Err(EngineFrameError::WrongByteLength {
                actual: byte_len,
                expected,
            });
        }
        if byte_len > MAX_FRAME_BYTES {
            return Err(EngineFrameError::FrameTooLarge {
                actual: byte_len,
                maximum: MAX_FRAME_BYTES,
            });
        }
        let stride = usize::try_from(size.width)
            .ok()
            .and_then(|width| width.checked_mul(RGBA8_BYTES_PER_PIXEL))
            .ok_or(EngineFrameError::ByteLengthOverflow)?;
        Ok(Self {
            size,
            stride,
            byte_len,
        })
    }

    /// Device-pixel dimensions.
    #[must_use]
    pub const fn size(self) -> PixelSize {
        self.size
    }

    /// Bytes per row.
    #[must_use]
    pub const fn stride(self) -> usize {
        self.stride
    }

    /// Total RGBA8 bytes.
    #[must_use]
    pub const fn byte_len(self) -> usize {
        self.byte_len
    }
}

/// Honest composition state of a published static frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrameComposition {
    /// No admitted text was omitted from the page frame.
    Complete,
    /// Page decorations and one separate glyph proof are available.
    SeparateGlyphProof {
        /// Text runs omitted from the page frame.
        pending_page_runs: u32,
        /// Run represented by the separate proof.
        proof_run_index: u32,
    },
    /// Shaped text exists but contains no paintable non-whitespace proof run.
    WhitespaceOnlyText {
        /// Text runs omitted from the page frame.
        pending_page_runs: u32,
    },
}

/// Fixed metadata carried by a frame-ready event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameMetadata {
    page: Rgba8Metadata,
    glyph_proof: Option<Rgba8Metadata>,
    composition: FrameComposition,
}

impl FrameMetadata {
    /// Page-frame RGBA8 metadata.
    #[must_use]
    pub const fn page(self) -> Rgba8Metadata {
        self.page
    }

    /// Optional separate glyph-proof RGBA8 metadata.
    #[must_use]
    pub const fn glyph_proof(self) -> Option<Rgba8Metadata> {
        self.glyph_proof
    }

    /// Honest composition limitation.
    #[must_use]
    pub const fn composition(self) -> FrameComposition {
        self.composition
    }

    fn total_bytes(self) -> Result<usize, EngineFrameError> {
        self.page
            .byte_len
            .checked_add(self.glyph_proof.map_or(0, |proof| proof.byte_len))
            .ok_or(EngineFrameError::ByteLengthOverflow)
    }
}

enum FramePixels {
    Headless(RgbaFrame),
    Owned(Box<[u8]>),
}

impl FramePixels {
    fn pixels(&self) -> &[u8] {
        match self {
            Self::Headless(frame) => frame.pixels(),
            Self::Owned(pixels) => pixels,
        }
    }
}

/// UI-neutral executor result before generation-checked publication.
pub struct EngineFrame {
    metadata: FrameMetadata,
    page: FramePixels,
    glyph_proof: Option<FramePixels>,
}

impl fmt::Debug for EngineFrame {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EngineFrame")
            .field("metadata", &self.metadata)
            .finish_non_exhaustive()
    }
}

impl EngineFrame {
    /// Creates a page-only RGBA8 frame for an executor or deterministic fake.
    ///
    /// # Errors
    ///
    /// Returns [`EngineFrameError`] when dimensions, length, or the hard byte
    /// cap are invalid.
    pub fn from_rgba8(
        size: PixelSize,
        pixels: Vec<u8>,
        composition: FrameComposition,
    ) -> Result<Self, EngineFrameError> {
        if matches!(composition, FrameComposition::SeparateGlyphProof { .. }) {
            return Err(EngineFrameError::CompositionMismatch);
        }
        let page = Rgba8Metadata::checked(size, pixels.len())?;
        let metadata = FrameMetadata {
            page,
            glyph_proof: None,
            composition,
        };
        checked_total_frame_bytes(metadata)?;
        Ok(Self {
            metadata,
            page: FramePixels::Owned(pixels.into_boxed_slice()),
            glyph_proof: None,
        })
    }

    fn from_rendered(rendered: RenderedStaticPage) -> Result<Self, EngineFrameError> {
        let RenderedStaticPage {
            page_frame,
            glyph_proof_frame,
            composition,
            ..
        } = rendered;
        let page = metadata_from_headless(&page_frame)?;
        let glyph_proof = glyph_proof_frame
            .as_ref()
            .map(metadata_from_headless)
            .transpose()?;
        let composition = match composition {
            CompositionStatus::NoText => {
                if glyph_proof.is_some() {
                    return Err(EngineFrameError::CompositionMismatch);
                }
                FrameComposition::Complete
            }
            CompositionStatus::SeparateGlyphProof {
                pending_page_runs,
                proof_run_index,
            } => {
                if glyph_proof.is_none() {
                    return Err(EngineFrameError::CompositionMismatch);
                }
                FrameComposition::SeparateGlyphProof {
                    pending_page_runs: u32::try_from(pending_page_runs)
                        .map_err(|_| EngineFrameError::CompositionCountOverflow)?,
                    proof_run_index: u32::try_from(proof_run_index)
                        .map_err(|_| EngineFrameError::CompositionCountOverflow)?,
                }
            }
            CompositionStatus::WhitespaceOnlyText { pending_page_runs } => {
                if glyph_proof.is_some() {
                    return Err(EngineFrameError::CompositionMismatch);
                }
                FrameComposition::WhitespaceOnlyText {
                    pending_page_runs: u32::try_from(pending_page_runs)
                        .map_err(|_| EngineFrameError::CompositionCountOverflow)?,
                }
            }
        };
        let metadata = FrameMetadata {
            page,
            glyph_proof,
            composition,
        };
        checked_total_frame_bytes(metadata)?;
        Ok(Self {
            metadata,
            page: FramePixels::Headless(page_frame),
            glyph_proof: glyph_proof_frame.map(FramePixels::Headless),
        })
    }

    /// Fixed publication metadata.
    #[must_use]
    pub const fn metadata(&self) -> FrameMetadata {
        self.metadata
    }

    /// Exact page RGBA8 bytes in top-left row order.
    #[must_use]
    pub fn page_pixels(&self) -> &[u8] {
        self.page.pixels()
    }

    /// Exact separate glyph-proof bytes, when present.
    #[must_use]
    pub fn glyph_proof_pixels(&self) -> Option<&[u8]> {
        self.glyph_proof.as_ref().map(FramePixels::pixels)
    }
}

fn metadata_from_headless(frame: &RgbaFrame) -> Result<Rgba8Metadata, EngineFrameError> {
    let size = frame.size();
    let size = PixelSize::new(size.width(), size.height())?;
    Rgba8Metadata::checked(size, frame.pixels().len())
}

fn checked_total_frame_bytes(metadata: FrameMetadata) -> Result<usize, EngineFrameError> {
    let total = metadata.total_bytes()?;
    if total > MAX_FRAME_BYTES {
        return Err(EngineFrameError::FrameTooLarge {
            actual: total,
            maximum: MAX_FRAME_BYTES,
        });
    }
    Ok(total)
}

/// Invalid executor frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EngineFrameError {
    /// Zero width or height.
    InvalidSize { width: u32, height: u32 },
    /// Checked RGBA8 length arithmetic overflowed.
    ByteLengthOverflow,
    /// Supplied pixel bytes do not match dimensions.
    WrongByteLength { actual: usize, expected: usize },
    /// Frame exceeds the absolute construction cap.
    FrameTooLarge { actual: usize, maximum: usize },
    /// A composition counter does not fit the public bounded representation.
    CompositionCountOverflow,
    /// Composition metadata and proof-frame presence disagree.
    CompositionMismatch,
}

impl fmt::Display for EngineFrameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "invalid engine frame: {self:?}")
    }
}

impl std::error::Error for EngineFrameError {}

/// Coarse, UI-safe stage for a navigation failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NavigationStage {
    /// URL validation and loopback fetch.
    Fetch,
    /// HTML parsing and immutable DOM creation.
    Document,
    /// Style computation.
    Style,
    /// Layout and text shaping.
    Layout,
    /// Scene compilation and rendering.
    Render,
    /// Executor shutdown.
    Shutdown,
}

/// Bounded failure category returned by an executor and carried in events.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutionFailureKind {
    /// Cancellation was observed.
    Cancelled,
    /// A bounded deadline elapsed.
    DeadlineExceeded,
    /// Input or policy rejected the request.
    Rejected,
    /// Network transport failed.
    Network,
    /// Document processing failed.
    Document,
    /// Rendering failed.
    Rendering,
    /// A configured resource limit rejected work.
    ResourceLimit,
    /// Executor invariant failed without exposing private diagnostics.
    Internal,
}

/// Fixed-size executor failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExecutionFailure {
    kind: ExecutionFailureKind,
    stage: NavigationStage,
}

impl ExecutionFailure {
    /// Creates a fixed-size failure.
    #[must_use]
    pub const fn new(kind: ExecutionFailureKind, stage: NavigationStage) -> Self {
        Self { kind, stage }
    }

    /// Failure category.
    #[must_use]
    pub const fn kind(self) -> ExecutionFailureKind {
        self.kind
    }

    /// Last meaningful stage.
    #[must_use]
    pub const fn stage(self) -> NavigationStage {
        self.stage
    }
}

/// Successful executor output before generation-gated publication.
#[derive(Debug)]
pub struct ExecutorOutput {
    http_status: u16,
    frame: EngineFrame,
}

impl ExecutorOutput {
    /// Creates a success result with a 2xx HTTP status.
    ///
    /// # Errors
    ///
    /// Returns [`ExecutionFailure`] if the status is not successful.
    pub fn new(http_status: u16, frame: EngineFrame) -> Result<Self, ExecutionFailure> {
        if !(200..=299).contains(&http_status) {
            return Err(ExecutionFailure::new(
                ExecutionFailureKind::Rejected,
                NavigationStage::Fetch,
            ));
        }
        Ok(Self { http_status, frame })
    }
}

/// Dedicated-worker execution seam used by the real static pipeline and deterministic tests.
///
/// The executor itself need not be [`Send`]: its `Send` factory crosses the
/// thread boundary, then constructs, uses, and destroys the executor on that
/// one worker. This permits thread-affine EGL and renderer state without an
/// unsafe cross-thread assertion.
pub trait NavigationExecutor: 'static {
    /// Executes one request synchronously. Implementations must observe bounded
    /// cancellation and must never publish frames themselves.
    ///
    /// # Errors
    ///
    /// Returns a fixed [`ExecutionFailure`] when execution cannot produce a
    /// publishable response and frame.
    fn execute(
        &mut self,
        navigation: NavigationId,
        request: &NavigationRequest,
        cancellation: &CancellationToken,
    ) -> Result<ExecutorOutput, ExecutionFailure>;

    /// Releases all executor-owned resources on the same worker thread.
    ///
    /// # Errors
    ///
    /// Returns a fixed [`ExecutionFailure`] when cleanup is incomplete.
    fn shutdown(&mut self) -> Result<(), ExecutionFailure>;
}

/// Opaque one-shot identity for a published frame.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct FrameLeaseId(NonZeroU64);

impl FrameLeaseId {
    /// Returns the opaque numeric representation.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// Monotonic event sequence assigned by the worker.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct EventSequence(NonZeroU64);

impl EventSequence {
    /// Returns the sequence number.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// Reason the worker stopped accepting and executing commands.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerStopReason {
    /// Explicit shutdown was requested.
    Requested,
    /// The only event receiver was dropped.
    EventReceiverDropped,
    /// An indivisible event publication could not fit its bounded queue.
    EventQueueSaturated,
    /// Internal event order validation rejected a transition.
    EventOrderViolation,
    /// A monotonic event or frame-lease identity was exhausted.
    IdentityExhausted,
    /// Executor code panicked; the worker caught the unwind.
    ExecutorPanicked,
}

/// Outcome of executor cleanup performed on its owner thread.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutorShutdownStatus {
    /// Executor construction did not complete, so no cleanup was possible.
    NotStarted,
    /// Executor cleanup completed.
    Clean,
    /// Executor cleanup returned a bounded error.
    Failed(ExecutionFailure),
    /// Executor cleanup panicked and was contained by the worker.
    Panicked,
}

/// Stable, repeatable result of joining the worker.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EngineShutdownStatus {
    reason: WorkerStopReason,
    executor: ExecutorShutdownStatus,
}

impl EngineShutdownStatus {
    /// Why command processing stopped.
    #[must_use]
    pub const fn reason(self) -> WorkerStopReason {
        self.reason
    }

    /// Same-thread executor cleanup result.
    #[must_use]
    pub const fn executor(self) -> ExecutorShutdownStatus {
        self.executor
    }
}

/// Fixed-size event payload.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EngineEventKind {
    /// The worker began executing a navigation.
    NavigationStarted { navigation: NavigationId },
    /// The synchronous pipeline committed a successful response.
    NavigationCommitted {
        navigation: NavigationId,
        http_status: u16,
    },
    /// A current frame was published behind a one-shot lease.
    FrameReady {
        navigation: NavigationId,
        lease: FrameLeaseId,
        metadata: FrameMetadata,
    },
    /// Cancellation or supersession prevented publication.
    NavigationCancelled { navigation: NavigationId },
    /// Navigation failed before publication.
    NavigationFailed {
        navigation: NavigationId,
        failure: ExecutionFailure,
    },
    /// Reserved terminal event; it does not consume ordinary queue capacity.
    ShutdownComplete { status: EngineShutdownStatus },
}

/// Sequenced event from the dedicated worker.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EngineEvent {
    sequence: EventSequence,
    kind: EngineEventKind,
}

impl EngineEvent {
    /// Monotonic worker-assigned event sequence.
    #[must_use]
    pub const fn sequence(self) -> EventSequence {
        self.sequence
    }

    /// Typed event payload.
    #[must_use]
    pub const fn kind(self) -> EngineEventKind {
        self.kind
    }
}

/// Event queue state when no event was returned.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EventReceiveError {
    /// No event is currently queued, but the worker can still make progress.
    Empty,
    /// The worker stopped and no further events remain.
    Closed(EngineShutdownStatus),
}

/// A frame transferred out of the current-lease store exactly once.
#[derive(Debug)]
pub struct FrameLease {
    navigation: NavigationId,
    lease: FrameLeaseId,
    frame: EngineFrame,
}

impl FrameLease {
    /// Navigation which produced the frame.
    #[must_use]
    pub const fn navigation(&self) -> NavigationId {
        self.navigation
    }

    /// Lease identity consumed by this transfer.
    #[must_use]
    pub const fn lease_id(&self) -> FrameLeaseId {
        self.lease
    }

    /// Fixed metadata announced in `FrameReady`.
    #[must_use]
    pub const fn metadata(&self) -> FrameMetadata {
        self.frame.metadata()
    }

    /// Page RGBA8 bytes in top-left row order.
    #[must_use]
    pub fn page_pixels(&self) -> &[u8] {
        self.frame.page_pixels()
    }

    /// Optional separate glyph-proof RGBA8 bytes.
    #[must_use]
    pub fn glyph_proof_pixels(&self) -> Option<&[u8]> {
        self.frame.glyph_proof_pixels()
    }
}

/// Failed one-shot lease transfer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrameLeaseError {
    /// A lower, already-replaced or already-consumed lease cannot affect the current frame.
    Stale,
    /// The lease has never been issued by this worker.
    Unknown,
    /// The event receiver has already been detached.
    ReceiverClosed,
}

impl fmt::Display for FrameLeaseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "frame lease unavailable: {self:?}")
    }
}

impl std::error::Error for FrameLeaseError {}

/// Startup failure before a usable worker/receiver pair exists.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EngineStartError {
    /// The operating system rejected thread creation.
    ThreadSpawn,
    /// Executor construction returned a bounded failure.
    Executor(ExecutionFailure),
    /// Executor construction panicked and was contained.
    ExecutorPanicked,
    /// Worker initialization ended without a status message.
    WorkerExited,
}

impl fmt::Display for EngineStartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "navigation worker failed to start: {self:?}")
    }
}

impl std::error::Error for EngineStartError {}

struct StoredFrame {
    lease: FrameLeaseId,
    navigation: NavigationId,
    frame: EngineFrame,
}

struct ContextState {
    latest_generation: NavigationGeneration,
    cancellation: Option<(NavigationId, crate::CancellationSource)>,
    current_frame: Option<StoredFrame>,
}

struct NavigationWork {
    navigation: NavigationId,
    request: NavigationRequest,
    cancellation: CancellationToken,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Lifecycle {
    Running,
    Stopping(WorkerStopReason),
    Stopped(EngineShutdownStatus),
}

struct SharedState {
    lifecycle: Lifecycle,
    receiver_open: bool,
    commands: VecDeque<NavigationWork>,
    events: VecDeque<EngineEvent>,
    terminal_event: Option<EngineEvent>,
    contexts: BTreeMap<TopLevelContextId, ContextState>,
    retained_frame_bytes: usize,
    next_event_sequence: u64,
    next_frame_lease: u64,
}

struct Shared {
    limits: EngineLimits,
    state: Mutex<SharedState>,
    command_ready: Condvar,
    event_ready: Condvar,
}

impl Shared {
    fn new(limits: EngineLimits) -> Self {
        Self {
            limits,
            state: Mutex::new(SharedState {
                lifecycle: Lifecycle::Running,
                receiver_open: true,
                commands: VecDeque::with_capacity(limits.command_capacity()),
                events: VecDeque::with_capacity(limits.event_capacity()),
                terminal_event: None,
                contexts: BTreeMap::new(),
                retained_frame_bytes: 0,
                next_event_sequence: 1,
                next_frame_lease: 1,
            }),
            command_ready: Condvar::new(),
            event_ready: Condvar::new(),
        }
    }
}

/// Controller for one dedicated, bounded navigation worker.
pub struct NavigationEngine {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
    joined_status: Option<EngineShutdownStatus>,
}

impl NavigationEngine {
    /// Spawns the real static pipeline, constructing and owning it entirely on
    /// the dedicated worker thread.
    ///
    /// # Errors
    ///
    /// Returns [`EngineStartError`] if thread or pipeline initialization fails.
    pub fn spawn(
        config: StaticPageConfig,
        limits: EngineLimits,
    ) -> Result<(Self, EngineEventReceiver), EngineStartError> {
        Self::spawn_with_executor(limits, move || StaticPipelineExecutor::new(config))
    }

    /// Spawns a worker around a bounded executor factory. This seam exists for
    /// deterministic contract tests and future engine adapters; the factory,
    /// executor, and executor shutdown all run on the worker thread.
    ///
    /// # Errors
    ///
    /// Returns [`EngineStartError`] if thread or executor initialization fails.
    pub fn spawn_with_executor<E, F>(
        limits: EngineLimits,
        factory: F,
    ) -> Result<(Self, EngineEventReceiver), EngineStartError>
    where
        E: NavigationExecutor,
        F: FnOnce() -> Result<E, ExecutionFailure> + Send + 'static,
    {
        let shared = Arc::new(Shared::new(limits));
        let worker_shared = Arc::clone(&shared);
        let (init_sender, init_receiver) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("wild-buzzard-engine".into())
            .spawn(move || worker_main(&worker_shared, factory, &init_sender))
            .map_err(|_| EngineStartError::ThreadSpawn)?;

        match init_receiver.recv() {
            Ok(WorkerInit::Ready) => Ok((
                Self {
                    shared: Arc::clone(&shared),
                    worker: Some(worker),
                    joined_status: None,
                },
                EngineEventReceiver {
                    shared,
                    attached: true,
                },
            )),
            Ok(WorkerInit::Failed(error)) => {
                let _ = worker.join();
                Err(EngineStartError::Executor(error))
            }
            Ok(WorkerInit::Panicked) => {
                let _ = worker.join();
                Err(EngineStartError::ExecutorPanicked)
            }
            Err(_) => {
                let _ = worker.join();
                Err(EngineStartError::WorkerExited)
            }
        }
    }

    /// Sends a typed command without blocking.
    ///
    /// Navigate admission is transactional: failure does not create a context,
    /// advance its generation, cancel prior work, or consume queue capacity.
    /// Cancellation and shutdown are priority controls and therefore remain
    /// available when the navigation queue is saturated.
    ///
    /// # Errors
    ///
    /// Returns [`CommandError`] with ownership of the rejected command.
    pub fn try_send(&self, command: EngineCommand) -> Result<CommandReceipt, CommandError> {
        let result = match &command {
            EngineCommand::Navigate {
                navigation,
                request,
            } => self.try_queue_navigation(*navigation, request.clone()),
            EngineCommand::Cancel { navigation } => self.try_cancel(*navigation),
            EngineCommand::Shutdown => Ok(self.request_shutdown()),
        };
        result.map_err(|kind| CommandError { kind, command })
    }

    /// Allocates and queues the exact next generation for a context.
    ///
    /// # Errors
    ///
    /// Returns the same transactional errors as [`Self::try_send`].
    pub fn navigate(
        &self,
        context: TopLevelContextId,
        request: NavigationRequest,
    ) -> Result<NavigationId, CommandError> {
        let mut state = lock_unpoisoned(&self.shared.state);
        let generation = match state.contexts.get(&context) {
            Some(context_state) => match context_state.latest_generation.checked_next() {
                Some(generation) => generation,
                None => {
                    return Err(CommandError {
                        kind: CommandErrorKind::GenerationExhausted,
                        command: EngineCommand::Navigate {
                            navigation: NavigationId::new(context, context_state.latest_generation),
                            request,
                        },
                    });
                }
            },
            None => NavigationGeneration::INITIAL,
        };
        let navigation = NavigationId::new(context, generation);
        if let Err(kind) =
            queue_navigation_locked(&mut state, self.shared.limits, navigation, request.clone())
        {
            return Err(CommandError {
                kind,
                command: EngineCommand::Navigate {
                    navigation,
                    request,
                },
            });
        }
        drop(state);
        self.shared.command_ready.notify_one();
        Ok(navigation)
    }

    /// Last accepted generation for a context.
    #[must_use]
    pub fn latest_generation(&self, context: TopLevelContextId) -> Option<NavigationGeneration> {
        lock_unpoisoned(&self.shared.state)
            .contexts
            .get(&context)
            .map(|context| context.latest_generation)
    }

    /// Requests shutdown, joins the worker exactly once, and returns a stable
    /// status. Repeated calls return the same status.
    #[must_use]
    pub fn shutdown(&mut self) -> EngineShutdownStatus {
        if let Some(status) = self.joined_status {
            return status;
        }
        self.request_shutdown();
        let join_result = self.worker.take().map(JoinHandle::join);
        if matches!(join_result, Some(Err(_))) {
            force_worker_stopped(
                &self.shared,
                EngineShutdownStatus {
                    reason: WorkerStopReason::ExecutorPanicked,
                    executor: ExecutorShutdownStatus::Panicked,
                },
            );
        }
        let status = match lock_unpoisoned(&self.shared.state).lifecycle {
            Lifecycle::Stopped(status) => status,
            Lifecycle::Running | Lifecycle::Stopping(_) => EngineShutdownStatus {
                reason: WorkerStopReason::ExecutorPanicked,
                executor: ExecutorShutdownStatus::Panicked,
            },
        };
        self.joined_status = Some(status);
        status
    }

    fn try_queue_navigation(
        &self,
        navigation: NavigationId,
        request: NavigationRequest,
    ) -> Result<CommandReceipt, CommandErrorKind> {
        let mut state = lock_unpoisoned(&self.shared.state);
        let receipt = queue_navigation_locked(&mut state, self.shared.limits, navigation, request)?;
        drop(state);
        self.shared.command_ready.notify_one();
        Ok(receipt)
    }

    fn try_cancel(&self, navigation: NavigationId) -> Result<CommandReceipt, CommandErrorKind> {
        let state = lock_unpoisoned(&self.shared.state);
        ensure_accepting(&state)?;
        let context = state
            .contexts
            .get(&navigation.context())
            .ok_or(CommandErrorKind::UnknownContext)?;
        if context.latest_generation != navigation.generation() {
            return Err(CommandErrorKind::NotCurrentNavigation);
        }
        let (_, cancellation) = context
            .cancellation
            .as_ref()
            .filter(|(active, _)| *active == navigation)
            .ok_or(CommandErrorKind::NoActiveNavigation)?;
        if !cancellation.cancel() {
            return Err(CommandErrorKind::NoActiveNavigation);
        }
        Ok(CommandReceipt::CancellationRequested(navigation))
    }

    fn request_shutdown(&self) -> CommandReceipt {
        let mut state = lock_unpoisoned(&self.shared.state);
        let already_requested = !matches!(state.lifecycle, Lifecycle::Running);
        request_stop_locked(&mut state, WorkerStopReason::Requested);
        drop(state);
        self.shared.command_ready.notify_all();
        CommandReceipt::ShutdownRequested { already_requested }
    }
}

impl Drop for NavigationEngine {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

/// Unique bounded event consumer and frame-lease transfer endpoint.
pub struct EngineEventReceiver {
    shared: Arc<Shared>,
    attached: bool,
}

impl EngineEventReceiver {
    /// Receives the next event without blocking.
    ///
    /// # Errors
    ///
    /// Returns [`EventReceiveError::Empty`] while the worker can still produce
    /// events, or [`EventReceiveError::Closed`] after terminal drain.
    pub fn try_recv(&mut self) -> Result<EngineEvent, EventReceiveError> {
        receive_event_locked(&self.shared, false)
    }

    /// Blocks on a condition variable until an event or terminal status exists.
    ///
    /// # Errors
    ///
    /// Returns [`EventReceiveError::Closed`] after all events are drained.
    pub fn recv(&mut self) -> Result<EngineEvent, EventReceiveError> {
        receive_event_locked(&self.shared, true)
    }

    /// Atomically transfers the current matching frame out of the bounded store.
    /// A stale lease never removes a newer current frame.
    ///
    /// # Errors
    ///
    /// Returns [`FrameLeaseError`] when the lease is stale, unknown, or this
    /// receiver is detached.
    pub fn take_frame(&mut self, lease: FrameLeaseId) -> Result<FrameLease, FrameLeaseError> {
        if !self.attached {
            return Err(FrameLeaseError::ReceiverClosed);
        }
        let mut state = lock_unpoisoned(&self.shared.state);
        let matching_context = state.contexts.iter().find_map(|(context_id, context)| {
            let stored = context.current_frame.as_ref()?;
            if stored.lease != lease {
                return None;
            }
            stored
                .frame
                .metadata()
                .total_bytes()
                .ok()
                .map(|bytes| (*context_id, bytes))
        });
        if let Some((context_id, bytes)) = matching_context {
            let Some(retained_after) = state.retained_frame_bytes.checked_sub(bytes) else {
                return Err(FrameLeaseError::Unknown);
            };
            let Some(stored) = state
                .contexts
                .get_mut(&context_id)
                .and_then(|context| context.current_frame.take())
            else {
                return Err(FrameLeaseError::Unknown);
            };
            state.retained_frame_bytes = retained_after;
            return Ok(FrameLease {
                navigation: stored.navigation,
                lease: stored.lease,
                frame: stored.frame,
            });
        }
        if lease.get() < state.next_frame_lease {
            Err(FrameLeaseError::Stale)
        } else {
            Err(FrameLeaseError::Unknown)
        }
    }
}

impl Drop for EngineEventReceiver {
    fn drop(&mut self) {
        if !self.attached {
            return;
        }
        self.attached = false;
        let mut state = lock_unpoisoned(&self.shared.state);
        state.receiver_open = false;
        state.events.clear();
        state.terminal_event = None;
        for context in state.contexts.values_mut() {
            if let Some((_, cancellation)) = context.cancellation.take() {
                cancellation.cancel();
            }
            context.current_frame = None;
        }
        state.retained_frame_bytes = 0;
        request_stop_locked(&mut state, WorkerStopReason::EventReceiverDropped);
        drop(state);
        self.shared.command_ready.notify_all();
        self.shared.event_ready.notify_all();
    }
}

fn receive_event_locked(shared: &Shared, blocking: bool) -> Result<EngineEvent, EventReceiveError> {
    let mut state = lock_unpoisoned(&shared.state);
    loop {
        if let Some(event) = state.events.pop_front() {
            return Ok(event);
        }
        if let Some(event) = state.terminal_event.take() {
            return Ok(event);
        }
        if let Lifecycle::Stopped(status) = state.lifecycle {
            return Err(EventReceiveError::Closed(status));
        }
        if !blocking {
            return Err(EventReceiveError::Empty);
        }
        state = shared
            .event_ready
            .wait(state)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
    }
}

fn ensure_accepting(state: &SharedState) -> Result<(), CommandErrorKind> {
    if !state.receiver_open {
        return Err(CommandErrorKind::EventReceiverDropped);
    }
    if !matches!(state.lifecycle, Lifecycle::Running) {
        return Err(CommandErrorKind::ShuttingDown);
    }
    Ok(())
}

fn queue_navigation_locked(
    state: &mut SharedState,
    limits: EngineLimits,
    navigation: NavigationId,
    request: NavigationRequest,
) -> Result<CommandReceipt, CommandErrorKind> {
    ensure_accepting(state)?;

    let context_id = navigation.context();
    let generation = navigation.generation();
    match state.contexts.get(&context_id) {
        Some(existing) if existing.latest_generation.checked_next().is_none() => {
            return Err(CommandErrorKind::GenerationExhausted);
        }
        Some(existing) if generation <= existing.latest_generation => {
            return Err(CommandErrorKind::NonMonotonicGeneration {
                latest: existing.latest_generation,
            });
        }
        None if generation != NavigationGeneration::INITIAL => {
            return Err(CommandErrorKind::InitialGenerationRequired);
        }
        None if state.contexts.len() >= limits.max_contexts() => {
            return Err(CommandErrorKind::ContextLimitReached {
                maximum: limits.max_contexts(),
            });
        }
        Some(_) | None => {}
    }
    if state.commands.len() >= limits.command_capacity() {
        return Err(CommandErrorKind::QueueFull {
            capacity: limits.command_capacity(),
        });
    }

    let cancellation = crate::CancellationSource::new();
    state.commands.push_back(NavigationWork {
        navigation,
        request,
        cancellation: cancellation.token(),
    });
    match state.contexts.get_mut(&context_id) {
        Some(context) => {
            if let Some((_, previous)) = context.cancellation.replace((navigation, cancellation)) {
                previous.cancel();
            }
            context.latest_generation = generation;
        }
        None => {
            state.contexts.insert(
                context_id,
                ContextState {
                    latest_generation: generation,
                    cancellation: Some((navigation, cancellation)),
                    current_frame: None,
                },
            );
        }
    }
    Ok(CommandReceipt::NavigationQueued(navigation))
}

fn request_stop_locked(state: &mut SharedState, reason: WorkerStopReason) {
    if matches!(state.lifecycle, Lifecycle::Running) {
        state.lifecycle = Lifecycle::Stopping(reason);
        for context in state.contexts.values() {
            if let Some((_, cancellation)) = &context.cancellation {
                cancellation.cancel();
            }
        }
    }
}

enum WorkerInit {
    Ready,
    Failed(ExecutionFailure),
    Panicked,
}

fn worker_main<E, F>(shared: &Shared, factory: F, init_sender: &mpsc::SyncSender<WorkerInit>)
where
    E: NavigationExecutor,
    F: FnOnce() -> Result<E, ExecutionFailure>,
{
    let executor = catch_unwind(AssertUnwindSafe(factory));
    let mut executor = match executor {
        Ok(Ok(executor)) => executor,
        Ok(Err(error)) => {
            let _ = init_sender.send(WorkerInit::Failed(error));
            finish_worker(
                shared,
                EngineShutdownStatus {
                    reason: WorkerStopReason::Requested,
                    executor: ExecutorShutdownStatus::NotStarted,
                },
            );
            return;
        }
        Err(_) => {
            let _ = init_sender.send(WorkerInit::Panicked);
            finish_worker(
                shared,
                EngineShutdownStatus {
                    reason: WorkerStopReason::ExecutorPanicked,
                    executor: ExecutorShutdownStatus::NotStarted,
                },
            );
            return;
        }
    };
    if init_sender.send(WorkerInit::Ready).is_err() {
        let finalization = finalize_executor(executor);
        finish_worker(
            shared,
            EngineShutdownStatus {
                reason: if finalization.drop_panicked {
                    WorkerStopReason::ExecutorPanicked
                } else {
                    WorkerStopReason::EventReceiverDropped
                },
                executor: finalization.status,
            },
        );
        return;
    }

    let mut reason = catch_unwind(AssertUnwindSafe(|| worker_loop(shared, &mut executor)))
        .unwrap_or(WorkerStopReason::ExecutorPanicked);
    let finalization = finalize_executor(executor);
    if finalization.drop_panicked {
        reason = WorkerStopReason::ExecutorPanicked;
    }
    finish_worker(
        shared,
        EngineShutdownStatus {
            reason,
            executor: finalization.status,
        },
    );
}

struct ExecutorFinalization {
    status: ExecutorShutdownStatus,
    drop_panicked: bool,
}

fn finalize_executor<E: NavigationExecutor>(mut executor: E) -> ExecutorFinalization {
    let shutdown = match catch_unwind(AssertUnwindSafe(|| executor.shutdown())) {
        Ok(Ok(())) => ExecutorShutdownStatus::Clean,
        Ok(Err(error)) => ExecutorShutdownStatus::Failed(error),
        Err(_) => ExecutorShutdownStatus::Panicked,
    };
    let drop_panicked = catch_unwind(AssertUnwindSafe(|| drop(executor))).is_err();
    ExecutorFinalization {
        status: if drop_panicked {
            ExecutorShutdownStatus::Panicked
        } else {
            shutdown
        },
        drop_panicked,
    }
}

fn worker_loop<E: NavigationExecutor>(shared: &Shared, executor: &mut E) -> WorkerStopReason {
    loop {
        let work = match dequeue_work(shared) {
            Ok(work) => work,
            Err(reason) => return reason,
        };

        let mut phase = NavigationEventPhase::Queued;
        match begin_navigation(shared, &work, &mut phase) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(reason) => return reason,
        }

        let result = executor.execute(work.navigation, &work.request, &work.cancellation);
        if let Err(reason) = finish_navigation(shared, &work, &mut phase, result) {
            return reason;
        }
    }
}

fn dequeue_work(shared: &Shared) -> Result<NavigationWork, WorkerStopReason> {
    let mut state = lock_unpoisoned(&shared.state);
    loop {
        match state.lifecycle {
            Lifecycle::Stopping(reason) => return Err(reason),
            Lifecycle::Stopped(status) => return Err(status.reason),
            Lifecycle::Running => {}
        }
        if let Some(work) = state.commands.pop_front() {
            return Ok(work);
        }
        state = shared
            .command_ready
            .wait(state)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
    }
}

fn begin_navigation(
    shared: &Shared,
    work: &NavigationWork,
    phase: &mut NavigationEventPhase,
) -> Result<bool, WorkerStopReason> {
    let mut state = lock_unpoisoned(&shared.state);
    if let Lifecycle::Stopping(reason) = state.lifecycle {
        return Err(reason);
    }
    let should_execute = is_current(&state, work.navigation) && !work.cancellation.is_cancelled();
    let kind = if should_execute {
        EngineEventKind::NavigationStarted {
            navigation: work.navigation,
        }
    } else {
        EngineEventKind::NavigationCancelled {
            navigation: work.navigation,
        }
    };
    if let Err(reason) = enqueue_one(&mut state, shared.limits, phase, kind) {
        request_stop_locked(&mut state, reason);
        return Err(reason);
    }
    if !should_execute {
        clear_cancellation_if_current(&mut state, work.navigation);
    }
    drop(state);
    shared.event_ready.notify_one();
    Ok(should_execute)
}

fn finish_navigation(
    shared: &Shared,
    work: &NavigationWork,
    phase: &mut NavigationEventPhase,
    result: Result<ExecutorOutput, ExecutionFailure>,
) -> Result<(), WorkerStopReason> {
    let mut state = lock_unpoisoned(&shared.state);
    if let Lifecycle::Stopping(reason) = state.lifecycle {
        return Err(reason);
    }
    let publication = if !is_current(&state, work.navigation) || work.cancellation.is_cancelled() {
        let result = enqueue_one(
            &mut state,
            shared.limits,
            phase,
            EngineEventKind::NavigationCancelled {
                navigation: work.navigation,
            },
        );
        clear_cancellation_if_current(&mut state, work.navigation);
        result
    } else {
        publish_execution_result(&mut state, shared.limits, phase, work.navigation, result)
    };
    if let Err(reason) = publication {
        request_stop_locked(&mut state, reason);
        return Err(reason);
    }
    drop(state);
    shared.event_ready.notify_all();
    Ok(())
}

fn publish_execution_result(
    state: &mut SharedState,
    limits: EngineLimits,
    phase: &mut NavigationEventPhase,
    navigation: NavigationId,
    result: Result<ExecutorOutput, ExecutionFailure>,
) -> Result<(), WorkerStopReason> {
    match result {
        Ok(output) => publish_success(state, limits, phase, navigation, output),
        Err(failure) if failure.kind() == ExecutionFailureKind::Cancelled => {
            enqueue_one(
                state,
                limits,
                phase,
                EngineEventKind::NavigationCancelled { navigation },
            )?;
            clear_cancellation_if_current(state, navigation);
            Ok(())
        }
        Err(failure) => {
            enqueue_one(
                state,
                limits,
                phase,
                EngineEventKind::NavigationFailed {
                    navigation,
                    failure,
                },
            )?;
            clear_cancellation_if_current(state, navigation);
            Ok(())
        }
    }
}

fn is_current(state: &SharedState, navigation: NavigationId) -> bool {
    state
        .contexts
        .get(&navigation.context())
        .is_some_and(|context| context.latest_generation == navigation.generation())
}

fn clear_cancellation_if_current(state: &mut SharedState, navigation: NavigationId) {
    let Some(context) = state.contexts.get_mut(&navigation.context()) else {
        return;
    };
    if context
        .cancellation
        .as_ref()
        .is_some_and(|(active, _)| *active == navigation)
    {
        context.cancellation = None;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NavigationEventPhase {
    Queued,
    Started,
    Committed,
    Terminal,
}

fn transition_event(
    phase: NavigationEventPhase,
    kind: EngineEventKind,
) -> Result<NavigationEventPhase, WorkerStopReason> {
    match (phase, kind) {
        (NavigationEventPhase::Queued, EngineEventKind::NavigationStarted { .. }) => {
            Ok(NavigationEventPhase::Started)
        }
        (
            NavigationEventPhase::Queued | NavigationEventPhase::Started,
            EngineEventKind::NavigationCancelled { .. },
        )
        | (NavigationEventPhase::Started, EngineEventKind::NavigationFailed { .. })
        | (NavigationEventPhase::Committed, EngineEventKind::FrameReady { .. }) => {
            Ok(NavigationEventPhase::Terminal)
        }
        (NavigationEventPhase::Started, EngineEventKind::NavigationCommitted { .. }) => {
            Ok(NavigationEventPhase::Committed)
        }
        _ => Err(WorkerStopReason::EventOrderViolation),
    }
}

fn enqueue_one(
    state: &mut SharedState,
    limits: EngineLimits,
    phase: &mut NavigationEventPhase,
    kind: EngineEventKind,
) -> Result<(), WorkerStopReason> {
    let next_phase = transition_event(*phase, kind)?;
    if state.events.len() >= limits.event_capacity() {
        return Err(WorkerStopReason::EventQueueSaturated);
    }
    let sequence = reserve_event_sequence(state)?;
    state.events.push_back(EngineEvent { sequence, kind });
    *phase = next_phase;
    Ok(())
}

fn publish_success(
    state: &mut SharedState,
    limits: EngineLimits,
    phase: &mut NavigationEventPhase,
    navigation: NavigationId,
    output: ExecutorOutput,
) -> Result<(), WorkerStopReason> {
    if !is_current(state, navigation) {
        return Err(WorkerStopReason::EventOrderViolation);
    }
    let Some(retained_after) =
        retained_after_replacement(state, limits, navigation, &output.frame)?
    else {
        return reject_frame_resource_limit(state, limits, phase, navigation);
    };
    if limits.event_capacity().saturating_sub(state.events.len()) < 2 {
        return Err(WorkerStopReason::EventQueueSaturated);
    }

    let commit_kind = EngineEventKind::NavigationCommitted {
        navigation,
        http_status: output.http_status,
    };
    let committed_phase = transition_event(*phase, commit_kind)?;
    let lease_raw = state
        .next_frame_lease
        .checked_add(1)
        .ok_or(WorkerStopReason::IdentityExhausted)?;
    let lease = FrameLeaseId(
        NonZeroU64::new(state.next_frame_lease).ok_or(WorkerStopReason::IdentityExhausted)?,
    );
    let frame_kind = EngineEventKind::FrameReady {
        navigation,
        lease,
        metadata: output.frame.metadata(),
    };
    let terminal_phase = transition_event(committed_phase, frame_kind)?;
    let sequences = reserve_event_pair(state)?;

    let context = state
        .contexts
        .get_mut(&navigation.context())
        .ok_or(WorkerStopReason::EventOrderViolation)?;
    if context.latest_generation != navigation.generation() {
        return Err(WorkerStopReason::EventOrderViolation);
    }
    context.current_frame = Some(StoredFrame {
        lease,
        navigation,
        frame: output.frame,
    });
    if context
        .cancellation
        .as_ref()
        .is_some_and(|(active, _)| *active == navigation)
    {
        context.cancellation = None;
    }
    state.retained_frame_bytes = retained_after;
    state.next_frame_lease = lease_raw;
    state.events.push_back(EngineEvent {
        sequence: sequences[0],
        kind: commit_kind,
    });
    state.events.push_back(EngineEvent {
        sequence: sequences[1],
        kind: frame_kind,
    });
    *phase = terminal_phase;
    Ok(())
}

fn retained_after_replacement(
    state: &SharedState,
    limits: EngineLimits,
    navigation: NavigationId,
    frame: &EngineFrame,
) -> Result<Option<usize>, WorkerStopReason> {
    let frame_bytes = frame
        .metadata()
        .total_bytes()
        .map_err(|_| WorkerStopReason::IdentityExhausted)?;
    if frame_bytes > limits.max_frame_bytes() {
        return Ok(None);
    }
    let old_bytes = state
        .contexts
        .get(&navigation.context())
        .and_then(|context| context.current_frame.as_ref())
        .map(|stored| stored.frame.metadata().total_bytes())
        .transpose()
        .map_err(|_| WorkerStopReason::IdentityExhausted)?
        .unwrap_or(0);
    let retained_without_old = state
        .retained_frame_bytes
        .checked_sub(old_bytes)
        .ok_or(WorkerStopReason::IdentityExhausted)?;
    let retained_after = retained_without_old
        .checked_add(frame_bytes)
        .ok_or(WorkerStopReason::IdentityExhausted)?;
    if retained_after > limits.max_retained_frame_bytes() {
        return Ok(None);
    }
    Ok(Some(retained_after))
}

fn reject_frame_resource_limit(
    state: &mut SharedState,
    limits: EngineLimits,
    phase: &mut NavigationEventPhase,
    navigation: NavigationId,
) -> Result<(), WorkerStopReason> {
    enqueue_one(
        state,
        limits,
        phase,
        EngineEventKind::NavigationFailed {
            navigation,
            failure: ExecutionFailure::new(
                ExecutionFailureKind::ResourceLimit,
                NavigationStage::Render,
            ),
        },
    )?;
    clear_cancellation_if_current(state, navigation);
    Ok(())
}

fn reserve_event_sequence(state: &mut SharedState) -> Result<EventSequence, WorkerStopReason> {
    let raw = state.next_event_sequence;
    let next = raw
        .checked_add(1)
        .ok_or(WorkerStopReason::IdentityExhausted)?;
    let sequence = EventSequence(NonZeroU64::new(raw).ok_or(WorkerStopReason::IdentityExhausted)?);
    state.next_event_sequence = next;
    Ok(sequence)
}

fn reserve_event_pair(state: &mut SharedState) -> Result<[EventSequence; 2], WorkerStopReason> {
    let first_raw = state.next_event_sequence;
    let second_raw = first_raw
        .checked_add(1)
        .ok_or(WorkerStopReason::IdentityExhausted)?;
    let next = second_raw
        .checked_add(1)
        .ok_or(WorkerStopReason::IdentityExhausted)?;
    let first =
        EventSequence(NonZeroU64::new(first_raw).ok_or(WorkerStopReason::IdentityExhausted)?);
    let second =
        EventSequence(NonZeroU64::new(second_raw).ok_or(WorkerStopReason::IdentityExhausted)?);
    state.next_event_sequence = next;
    Ok([first, second])
}

fn finish_worker(shared: &Shared, status: EngineShutdownStatus) {
    let mut state = lock_unpoisoned(&shared.state);
    if matches!(state.lifecycle, Lifecycle::Stopped(_)) {
        return;
    }
    for context in state.contexts.values_mut() {
        if let Some((_, cancellation)) = context.cancellation.take() {
            cancellation.cancel();
        }
    }
    state.commands.clear();
    state.lifecycle = Lifecycle::Stopped(status);
    if state.receiver_open {
        if let Ok(sequence) = reserve_event_sequence(&mut state) {
            state.terminal_event = Some(EngineEvent {
                sequence,
                kind: EngineEventKind::ShutdownComplete { status },
            });
        }
    } else {
        for context in state.contexts.values_mut() {
            context.current_frame = None;
        }
        state.retained_frame_bytes = 0;
    }
    drop(state);
    shared.event_ready.notify_all();
    shared.command_ready.notify_all();
}

fn force_worker_stopped(shared: &Shared, status: EngineShutdownStatus) {
    let mut state = lock_unpoisoned(&shared.state);
    if !matches!(state.lifecycle, Lifecycle::Stopped(_)) {
        state.lifecycle = Lifecycle::Stopped(status);
    }
    drop(state);
    shared.event_ready.notify_all();
}

struct StaticPipelineExecutor {
    engine: Option<StaticPageEngine>,
}

impl StaticPipelineExecutor {
    fn new(config: StaticPageConfig) -> Result<Self, ExecutionFailure> {
        let engine = StaticPageEngine::new(config).map_err(|error| map_pipeline_error(&error))?;
        Ok(Self {
            engine: Some(engine),
        })
    }
}

impl NavigationExecutor for StaticPipelineExecutor {
    fn execute(
        &mut self,
        _navigation: NavigationId,
        request: &NavigationRequest,
        cancellation: &CancellationToken,
    ) -> Result<ExecutorOutput, ExecutionFailure> {
        let rendered = self
            .engine
            .as_mut()
            .ok_or_else(|| {
                ExecutionFailure::new(ExecutionFailureKind::Internal, NavigationStage::Render)
            })?
            .load(request.url(), cancellation)
            .map_err(|error| map_pipeline_error(&error))?;
        let http_status = rendered.evidence.http_status;
        let frame = EngineFrame::from_rendered(rendered).map_err(|_| {
            ExecutionFailure::new(ExecutionFailureKind::Internal, NavigationStage::Render)
        })?;
        ExecutorOutput::new(http_status, frame)
    }

    fn shutdown(&mut self) -> Result<(), ExecutionFailure> {
        let Some(engine) = self.engine.take() else {
            return Ok(());
        };
        engine.shutdown().map(|_| ()).map_err(|error| {
            let mut failure = map_pipeline_error(&error);
            failure.stage = NavigationStage::Shutdown;
            failure
        })
    }
}

fn map_pipeline_error(error: &PipelineError) -> ExecutionFailure {
    match error {
        PipelineError::Cancelled { stage } => {
            ExecutionFailure::new(ExecutionFailureKind::Cancelled, map_pipeline_stage(*stage))
        }
        PipelineError::DeadlineExceeded { stage } => ExecutionFailure::new(
            ExecutionFailureKind::DeadlineExceeded,
            map_pipeline_stage(*stage),
        ),
        PipelineError::Network(_) => {
            ExecutionFailure::new(ExecutionFailureKind::Network, NavigationStage::Fetch)
        }
        PipelineError::InvalidConfiguration { .. }
        | PipelineError::DeadlineOverflow
        | PipelineError::EvidenceOverflow
        | PipelineError::EpochExhausted => {
            ExecutionFailure::new(ExecutionFailureKind::ResourceLimit, NavigationStage::Render)
        }
        PipelineError::HttpStatus(_) | PipelineError::NonUtf8Html => {
            ExecutionFailure::new(ExecutionFailureKind::Rejected, NavigationStage::Document)
        }
        PipelineError::Html(_) | PipelineError::Dom(_) => {
            ExecutionFailure::new(ExecutionFailureKind::Document, NavigationStage::Document)
        }
        PipelineError::Style(_) => {
            ExecutionFailure::new(ExecutionFailureKind::Document, NavigationStage::Style)
        }
        PipelineError::Layout(_) | PipelineError::Text(_) => {
            ExecutionFailure::new(ExecutionFailureKind::Document, NavigationStage::Layout)
        }
        PipelineError::Scene(_) | PipelineError::Headless(_) => {
            ExecutionFailure::new(ExecutionFailureKind::Rendering, NavigationStage::Render)
        }
    }
}

const fn map_pipeline_stage(stage: PipelineStage) -> NavigationStage {
    match stage {
        PipelineStage::Fetch => NavigationStage::Fetch,
        PipelineStage::Parse | PipelineStage::Snapshot => NavigationStage::Document,
        PipelineStage::Style => NavigationStage::Style,
        PipelineStage::Layout | PipelineStage::TextShaping => NavigationStage::Layout,
        PipelineStage::SceneCompilation
        | PipelineStage::PageRender
        | PipelineStage::TextProofRender => NavigationStage::Render,
    }
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct GatedSuccessExecutor {
        entered: mpsc::Sender<NavigationId>,
        release: mpsc::Receiver<()>,
    }

    impl NavigationExecutor for GatedSuccessExecutor {
        fn execute(
            &mut self,
            navigation: NavigationId,
            _request: &NavigationRequest,
            _cancellation: &CancellationToken,
        ) -> Result<ExecutorOutput, ExecutionFailure> {
            self.entered.send(navigation).unwrap();
            self.release.recv().unwrap();
            ExecutorOutput::new(200, frame(7))
        }

        fn shutdown(&mut self) -> Result<(), ExecutionFailure> {
            Ok(())
        }
    }

    fn context() -> TopLevelContextId {
        TopLevelContextId::new(1).unwrap()
    }

    fn navigation() -> NavigationId {
        NavigationId::new(context(), NavigationGeneration::INITIAL)
    }

    fn frame(marker: u8) -> EngineFrame {
        EngineFrame::from_rgba8(
            PixelSize::new(1, 1).unwrap(),
            vec![marker, 0, 0, 255],
            FrameComposition::Complete,
        )
        .unwrap()
    }

    fn state_with_prior_frame() -> (SharedState, EngineLimits, NavigationId) {
        let limits = EngineLimits::new(1, 3, 1, 4, 4).unwrap();
        let navigation = navigation();
        let cancellation = crate::CancellationSource::new();
        let mut contexts = BTreeMap::new();
        contexts.insert(
            navigation.context(),
            ContextState {
                latest_generation: navigation.generation(),
                cancellation: Some((navigation, cancellation)),
                current_frame: Some(StoredFrame {
                    lease: FrameLeaseId(NonZeroU64::new(1).unwrap()),
                    navigation,
                    frame: frame(1),
                }),
            },
        );
        (
            SharedState {
                lifecycle: Lifecycle::Running,
                receiver_open: true,
                commands: VecDeque::new(),
                events: VecDeque::new(),
                terminal_event: None,
                contexts,
                retained_frame_bytes: 4,
                next_event_sequence: 1,
                next_frame_lease: 2,
            },
            limits,
            navigation,
        )
    }

    fn assert_prior_frame_unchanged(state: &SharedState, navigation: NavigationId) {
        let stored = state
            .contexts
            .get(&navigation.context())
            .unwrap()
            .current_frame
            .as_ref()
            .unwrap();
        assert_eq!(stored.lease.get(), 1);
        assert_eq!(stored.navigation, navigation);
        assert_eq!(stored.frame.page_pixels(), &[1, 0, 0, 255]);
        assert_eq!(state.retained_frame_bytes, 4);
        assert!(state.events.is_empty());
    }

    #[test]
    fn minimum_event_capacity_holds_started_and_atomic_success_without_a_drain() {
        assert_eq!(
            EngineLimits::new(1, 2, 1, 4, 4),
            Err(EngineLimitsError::TooSmall {
                field: "event_capacity",
                actual: 2,
                minimum: 3,
            })
        );
        let limits = EngineLimits::new(1, 3, 1, 4, 4).unwrap();
        let (entered_sender, entered_receiver) = mpsc::channel();
        let (release_sender, release_receiver) = mpsc::channel();
        let (mut engine, mut receiver) = NavigationEngine::spawn_with_executor(limits, move || {
            Ok(GatedSuccessExecutor {
                entered: entered_sender,
                release: release_receiver,
            })
        })
        .unwrap();
        let navigation = engine
            .navigate(context(), NavigationRequest::new("loopback").unwrap())
            .unwrap();
        assert_eq!(entered_receiver.recv().unwrap(), navigation);
        release_sender.send(()).unwrap();

        let mut state = lock_unpoisoned(&engine.shared.state);
        while state.events.len() < 3 {
            assert_eq!(state.lifecycle, Lifecycle::Running);
            state = engine
                .shared
                .event_ready
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        assert_eq!(state.events.len(), 3);
        drop(state);

        assert_eq!(
            receiver.recv().unwrap().kind(),
            EngineEventKind::NavigationStarted { navigation }
        );
        assert_eq!(
            receiver.recv().unwrap().kind(),
            EngineEventKind::NavigationCommitted {
                navigation,
                http_status: 200,
            }
        );
        let ready = receiver.recv().unwrap();
        let EngineEventKind::FrameReady {
            navigation: ready_navigation,
            lease,
            ..
        } = ready.kind()
        else {
            panic!("expected frame-ready event");
        };
        assert_eq!(ready_navigation, navigation);
        assert_eq!(receiver.take_frame(lease).unwrap().navigation(), navigation);
        assert_eq!(engine.shutdown().reason(), WorkerStopReason::Requested);
    }

    #[test]
    fn malformed_event_reordering_is_rejected() {
        let navigation = navigation();
        let failure =
            ExecutionFailure::new(ExecutionFailureKind::Internal, NavigationStage::Document);
        let lease = FrameLeaseId(NonZeroU64::new(1).unwrap());
        let metadata = frame(1).metadata();
        let invalid = [
            (
                NavigationEventPhase::Queued,
                EngineEventKind::NavigationCommitted {
                    navigation,
                    http_status: 200,
                },
            ),
            (
                NavigationEventPhase::Queued,
                EngineEventKind::NavigationFailed {
                    navigation,
                    failure,
                },
            ),
            (
                NavigationEventPhase::Started,
                EngineEventKind::FrameReady {
                    navigation,
                    lease,
                    metadata,
                },
            ),
            (
                NavigationEventPhase::Committed,
                EngineEventKind::NavigationCancelled { navigation },
            ),
            (
                NavigationEventPhase::Terminal,
                EngineEventKind::NavigationStarted { navigation },
            ),
            (
                NavigationEventPhase::Started,
                EngineEventKind::ShutdownComplete {
                    status: EngineShutdownStatus {
                        reason: WorkerStopReason::Requested,
                        executor: ExecutorShutdownStatus::Clean,
                    },
                },
            ),
        ];
        for (phase, event) in invalid {
            assert_eq!(
                transition_event(phase, event),
                Err(WorkerStopReason::EventOrderViolation)
            );
        }

        let started = transition_event(
            NavigationEventPhase::Queued,
            EngineEventKind::NavigationStarted { navigation },
        )
        .unwrap();
        let committed = transition_event(
            started,
            EngineEventKind::NavigationCommitted {
                navigation,
                http_status: 200,
            },
        )
        .unwrap();
        assert_eq!(
            transition_event(
                committed,
                EngineEventKind::FrameReady {
                    navigation,
                    lease,
                    metadata,
                },
            ),
            Ok(NavigationEventPhase::Terminal)
        );
    }

    #[test]
    fn event_sequence_exhaustion_aborts_multi_event_publication_atomically() {
        let (mut state, limits, navigation) = state_with_prior_frame();
        state.next_event_sequence = u64::MAX - 1;
        let mut phase = NavigationEventPhase::Started;
        let result = publish_success(
            &mut state,
            limits,
            &mut phase,
            navigation,
            ExecutorOutput::new(200, frame(2)).unwrap(),
        );

        assert_eq!(result, Err(WorkerStopReason::IdentityExhausted));
        assert_eq!(phase, NavigationEventPhase::Started);
        assert_eq!(state.next_event_sequence, u64::MAX - 1);
        assert_eq!(state.next_frame_lease, 2);
        assert_prior_frame_unchanged(&state, navigation);
    }

    #[test]
    fn frame_lease_exhaustion_aborts_multi_event_publication_atomically() {
        let (mut state, limits, navigation) = state_with_prior_frame();
        state.next_frame_lease = u64::MAX;
        let mut phase = NavigationEventPhase::Started;
        let result = publish_success(
            &mut state,
            limits,
            &mut phase,
            navigation,
            ExecutorOutput::new(200, frame(2)).unwrap(),
        );

        assert_eq!(result, Err(WorkerStopReason::IdentityExhausted));
        assert_eq!(phase, NavigationEventPhase::Started);
        assert_eq!(state.next_event_sequence, 1);
        assert_eq!(state.next_frame_lease, u64::MAX);
        assert_prior_frame_unchanged(&state, navigation);
    }

    #[test]
    fn frame_constructor_rejects_missing_separate_proof() {
        assert_eq!(
            EngineFrame::from_rgba8(
                PixelSize::new(1, 1).unwrap(),
                vec![0, 0, 0, 255],
                FrameComposition::SeparateGlyphProof {
                    pending_page_runs: 1,
                    proof_run_index: 0,
                },
            )
            .unwrap_err(),
            EngineFrameError::CompositionMismatch
        );
    }
}
