use std::collections::BTreeMap;
use std::fmt;
use std::num::NonZeroU64;
use std::panic::{AssertUnwindSafe, catch_unwind};

use wild_buzzard_engine::{
    CommandErrorKind, DocumentOperationFailure, DocumentOperationId, EngineEventKind,
    EngineEventReceiver, EngineFrameError, EngineLimits, EngineShutdownStatus, EngineStartError,
    EventReceiveError, ExecutionFailure, ExecutorShutdownStatus, FrameLease, FrameLeaseError,
    FrameLeaseId, MutationResultLease, MutationResultLeaseError, MutationResultLeaseId,
    NavigationEngine, NavigationId, NavigationRequest, StaticPageConfig, TopLevelContextId,
    WorkerStopReason,
};
use wild_buzzard_engine::{NavigationExecutor, PixelSize};

// These are adapter-side defense-in-depth bounds. The real engine has tighter
// configurable resource limits, but the browser boundary must stay bounded
// even if it is constructed around a future or hostile implementation.
const MAX_PENDING_FRAME_BINDINGS: usize = 4_096;
const MAX_PENDING_MUTATION_BINDINGS: usize = 4_096;
const MAX_UI_FRAME_BYTES: usize = 256 * 1024 * 1024;

macro_rules! port_id {
    ($name:ident, $description:literal) => {
        #[doc = $description]
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(NonZeroU64);

        impl $name {
            /// Creates a nonzero port-scoped identity.
            #[must_use]
            pub const fn new(raw: u64) -> Option<Self> {
                match NonZeroU64::new(raw) {
                    Some(raw) => Some(Self(raw)),
                    None => None,
                }
            }

            /// Returns the diagnostic integer representation.
            #[must_use]
            pub const fn get(self) -> u64 {
                self.0.get()
            }
        }
    };
}

port_id!(
    EnginePortSequence,
    "Monotonic event identity produced by one engine port incarnation."
);
port_id!(
    EnginePortFrameLeaseId,
    "Opaque one-shot frame identity scoped to one engine port."
);
port_id!(
    EnginePortMutationLeaseId,
    "Opaque one-shot mutation-result identity scoped to one engine port."
);

/// Opaque engine-document identity plus its exact revision.
///
/// Browser chrome can compare this value but cannot use it to reach into the
/// DOM arena.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct EngineDocumentVersion {
    document: u64,
    revision: u64,
}

impl EngineDocumentVersion {
    /// Constructs a value for deterministic port implementations.
    #[must_use]
    pub const fn new(document: u64, revision: u64) -> Self {
        Self { document, revision }
    }

    /// Opaque engine document identity.
    #[must_use]
    pub const fn document(self) -> u64 {
        self.document
    }

    /// Exact document-local revision.
    #[must_use]
    pub const fn revision(self) -> u64 {
        self.revision
    }
}

macro_rules! project_document_version {
    ($version:expr) => {{
        let version = $version;
        EngineDocumentVersion {
            document: version.document_id().get(),
            revision: version.revision(),
        }
    }};
}

/// UI-owned, fixed metadata for top-left row-order RGBA8 pixels.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EngineFrameDescriptor {
    width: u32,
    height: u32,
    stride: usize,
    byte_len: usize,
}

impl EngineFrameDescriptor {
    /// Validates dimensions, stride, and exact RGBA8 byte length.
    ///
    /// # Errors
    ///
    /// Returns [`EngineFrameError`] for invalid dimensions, arithmetic
    /// overflow, a non-exact byte length, or a frame above the hard UI limit.
    pub fn rgba8(width: u32, height: u32, byte_len: usize) -> Result<Self, EngineFrameError> {
        let size = PixelSize::new(width, height)?;
        let expected = usize::try_from(width)
            .ok()
            .and_then(|width| {
                usize::try_from(height)
                    .ok()
                    .and_then(|height| width.checked_mul(height))
            })
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or(EngineFrameError::ByteLengthOverflow)?;
        if expected != byte_len {
            return Err(EngineFrameError::WrongByteLength {
                actual: byte_len,
                expected,
            });
        }
        if expected > MAX_UI_FRAME_BYTES {
            return Err(EngineFrameError::FrameTooLarge {
                actual: expected,
                maximum: MAX_UI_FRAME_BYTES,
            });
        }
        let stride = usize::try_from(size.width())
            .ok()
            .and_then(|width| width.checked_mul(4))
            .ok_or(EngineFrameError::ByteLengthOverflow)?;
        Ok(Self {
            width: size.width(),
            height: size.height(),
            stride,
            byte_len,
        })
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

    /// Bytes between consecutive rows.
    #[must_use]
    pub const fn stride(self) -> usize {
        self.stride
    }

    /// Exact retained pixel byte length.
    #[must_use]
    pub const fn byte_len(self) -> usize {
        self.byte_len
    }
}

enum FrameBacking {
    Navigation(FrameLease),
    Owned(Box<[u8]>),
}

/// Exact-navigation, one-shot frame transferred from an [`EnginePort`].
pub struct EngineFrameLease {
    navigation: NavigationId,
    lease: EnginePortFrameLeaseId,
    descriptor: EngineFrameDescriptor,
    document_version: Option<EngineDocumentVersion>,
    backing: FrameBacking,
}

impl fmt::Debug for EngineFrameLease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EngineFrameLease")
            .field("navigation", &self.navigation)
            .field("lease", &self.lease)
            .field("descriptor", &self.descriptor)
            .field("document_version", &self.document_version)
            .finish_non_exhaustive()
    }
}

impl EngineFrameLease {
    /// Constructs checked owned RGBA8 data for a deterministic engine port.
    ///
    /// # Errors
    ///
    /// Returns [`EngineFrameError`] when the dimensions and pixels do not form
    /// a bounded exact RGBA8 frame.
    pub fn from_owned_rgba8(
        navigation: NavigationId,
        lease: EnginePortFrameLeaseId,
        width: u32,
        height: u32,
        pixels: Vec<u8>,
        document_version: Option<EngineDocumentVersion>,
    ) -> Result<Self, EngineFrameError> {
        let descriptor = EngineFrameDescriptor::rgba8(width, height, pixels.len())?;
        Ok(Self {
            navigation,
            lease,
            descriptor,
            document_version,
            backing: FrameBacking::Owned(pixels.into_boxed_slice()),
        })
    }

    fn from_navigation(
        lease: FrameLease,
        port_lease: EnginePortFrameLeaseId,
    ) -> Result<Self, EnginePortError> {
        let metadata = lease.metadata();
        if metadata.document_version() != lease.document_version() {
            return Err(EnginePortError::ContractViolation(
                "transferred frame document version disagrees with its event metadata",
            ));
        }
        let rgba8 = metadata.rgba8();
        let size = rgba8.size();
        let descriptor =
            EngineFrameDescriptor::rgba8(size.width(), size.height(), rgba8.byte_len()).map_err(
                |_| {
                    EnginePortError::ContractViolation(
                        "engine frame metadata exceeded the bounded contiguous RGBA8 contract",
                    )
                },
            )?;
        if rgba8.stride() != descriptor.stride() || lease.pixels().len() != descriptor.byte_len() {
            return Err(EnginePortError::ContractViolation(
                "engine frame bytes or stride disagree with bounded contiguous RGBA8 metadata",
            ));
        }
        Ok(Self {
            navigation: lease.navigation(),
            lease: port_lease,
            descriptor,
            document_version: metadata
                .document_version()
                .map(|version| project_document_version!(version)),
            backing: FrameBacking::Navigation(lease),
        })
    }

    /// Exact navigation which produced this frame.
    #[must_use]
    pub const fn navigation(&self) -> NavigationId {
        self.navigation
    }

    /// Port-scoped lease identity consumed by this transfer.
    #[must_use]
    pub const fn lease_id(&self) -> EnginePortFrameLeaseId {
        self.lease
    }

    /// Fixed RGBA8 metadata.
    #[must_use]
    pub const fn descriptor(&self) -> EngineFrameDescriptor {
        self.descriptor
    }

    /// Exact live document version represented when available.
    #[must_use]
    pub const fn document_version(&self) -> Option<EngineDocumentVersion> {
        self.document_version
    }

    /// Exact top-left row-order RGBA8 bytes.
    #[must_use]
    pub fn pixels(&self) -> &[u8] {
        match &self.backing {
            FrameBacking::Navigation(lease) => lease.pixels(),
            FrameBacking::Owned(pixels) => pixels,
        }
    }
}

enum MutationBacking {
    Navigation(MutationResultLease),
    Owned(usize),
}

/// Exact-navigation created-node mapping transferred from an engine port.
pub struct EngineMutationResultLease {
    navigation: NavigationId,
    operation: DocumentOperationId,
    live_version: EngineDocumentVersion,
    lease: EnginePortMutationLeaseId,
    backing: MutationBacking,
}

impl fmt::Debug for EngineMutationResultLease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EngineMutationResultLease")
            .field("navigation", &self.navigation)
            .field("operation", &self.operation)
            .field("live_version", &self.live_version)
            .field("lease", &self.lease)
            .field("created_nodes", &self.created_nodes())
            .finish_non_exhaustive()
    }
}

impl EngineMutationResultLease {
    /// Constructs a deterministic port result with an owned node mapping.
    #[must_use]
    pub fn from_owned(
        navigation: NavigationId,
        operation: DocumentOperationId,
        live_version: EngineDocumentVersion,
        lease: EnginePortMutationLeaseId,
        created_nodes: usize,
    ) -> Self {
        Self {
            navigation,
            operation,
            live_version,
            lease,
            backing: MutationBacking::Owned(created_nodes),
        }
    }

    fn from_navigation(lease: MutationResultLease, port_lease: EnginePortMutationLeaseId) -> Self {
        Self {
            navigation: lease.navigation(),
            operation: lease.operation(),
            live_version: project_document_version!(lease.live_version()),
            lease: port_lease,
            backing: MutationBacking::Navigation(lease),
        }
    }

    /// Exact navigation which owns this result.
    #[must_use]
    pub const fn navigation(&self) -> NavigationId {
        self.navigation
    }

    /// Exact document operation which produced this result.
    #[must_use]
    pub const fn operation(&self) -> DocumentOperationId {
        self.operation
    }

    /// Committed live document version.
    #[must_use]
    pub const fn live_version(&self) -> EngineDocumentVersion {
        self.live_version
    }

    /// Port-scoped lease identity consumed by this transfer.
    #[must_use]
    pub const fn lease_id(&self) -> EnginePortMutationLeaseId {
        self.lease
    }

    /// Number of entries in the engine-owned dense created-node mapping.
    #[must_use]
    pub fn created_nodes(&self) -> usize {
        match &self.backing {
            MutationBacking::Navigation(lease) => lease.created_nodes().len(),
            MutationBacking::Owned(nodes) => *nodes,
        }
    }
}

/// Stable engine stop category independent of worker-private ownership.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnginePortStopReason {
    Requested,
    EventReceiverDropped,
    EventQueueSaturated,
    EventOrderViolation,
    IdentityExhausted,
    ExecutorPanicked,
    RendererUnavailable,
    ExecutorContractViolation,
    /// A custom [`EnginePort`] panicked at the browser boundary.
    PortPanicked,
}

/// Stable executor cleanup category.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnginePortExecutorShutdown {
    NotStarted,
    Clean,
    Failed(ExecutionFailure),
    Panicked,
}

/// Repeatable result of closing one engine port.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EnginePortShutdownStatus {
    reason: EnginePortStopReason,
    executor: EnginePortExecutorShutdown,
}

impl EnginePortShutdownStatus {
    /// Constructs a stable status, including for deterministic test ports.
    #[must_use]
    pub const fn new(reason: EnginePortStopReason, executor: EnginePortExecutorShutdown) -> Self {
        Self { reason, executor }
    }

    /// Primary stop reason.
    #[must_use]
    pub const fn reason(self) -> EnginePortStopReason {
        self.reason
    }

    /// Same-thread executor cleanup outcome.
    #[must_use]
    pub const fn executor(self) -> EnginePortExecutorShutdown {
        self.executor
    }

    fn from_navigation(status: EngineShutdownStatus) -> Self {
        Self {
            reason: match status.reason() {
                WorkerStopReason::Requested => EnginePortStopReason::Requested,
                WorkerStopReason::EventReceiverDropped => {
                    EnginePortStopReason::EventReceiverDropped
                }
                WorkerStopReason::EventQueueSaturated => EnginePortStopReason::EventQueueSaturated,
                WorkerStopReason::EventOrderViolation => EnginePortStopReason::EventOrderViolation,
                WorkerStopReason::IdentityExhausted => EnginePortStopReason::IdentityExhausted,
                WorkerStopReason::ExecutorPanicked => EnginePortStopReason::ExecutorPanicked,
                WorkerStopReason::RendererUnavailable => EnginePortStopReason::RendererUnavailable,
                WorkerStopReason::ExecutorContractViolation => {
                    EnginePortStopReason::ExecutorContractViolation
                }
            },
            executor: match status.executor() {
                ExecutorShutdownStatus::NotStarted => EnginePortExecutorShutdown::NotStarted,
                ExecutorShutdownStatus::Clean => EnginePortExecutorShutdown::Clean,
                ExecutorShutdownStatus::Failed(failure) => {
                    EnginePortExecutorShutdown::Failed(failure)
                }
                ExecutorShutdownStatus::Panicked => EnginePortExecutorShutdown::Panicked,
            },
        }
    }

    /// Synthetic result used when a custom port panics during shutdown.
    #[must_use]
    pub const fn port_panicked() -> Self {
        Self {
            reason: EnginePortStopReason::PortPanicked,
            executor: EnginePortExecutorShutdown::Panicked,
        }
    }
}

/// Fixed-size event payload delivered to the browser controller.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnginePortEventKind {
    NavigationStarted {
        navigation: NavigationId,
    },
    NavigationCommitted {
        navigation: NavigationId,
        http_status: u16,
    },
    FrameReady {
        navigation: NavigationId,
        lease: EnginePortFrameLeaseId,
        descriptor: EngineFrameDescriptor,
        document_version: Option<EngineDocumentVersion>,
    },
    NavigationCancelled {
        navigation: NavigationId,
    },
    NavigationFailed {
        navigation: NavigationId,
        failure: ExecutionFailure,
    },
    DocumentMutationRendered {
        navigation: NavigationId,
        operation: DocumentOperationId,
        previous_live_version: EngineDocumentVersion,
        previous_frame_version: EngineDocumentVersion,
        live_version: EngineDocumentVersion,
        result: EnginePortMutationLeaseId,
        created_nodes: usize,
        frame: EnginePortFrameLeaseId,
        descriptor: EngineFrameDescriptor,
    },
    DocumentMutationCommittedWithoutFrame {
        navigation: NavigationId,
        operation: DocumentOperationId,
        previous_live_version: EngineDocumentVersion,
        live_version: EngineDocumentVersion,
        frame_version: EngineDocumentVersion,
        result: EnginePortMutationLeaseId,
        created_nodes: usize,
        failure: DocumentOperationFailure,
    },
    DocumentMutationRejected {
        navigation: NavigationId,
        operation: DocumentOperationId,
        live_version: Option<EngineDocumentVersion>,
        frame_version: Option<EngineDocumentVersion>,
        failure: DocumentOperationFailure,
    },
    DocumentRerendered {
        navigation: NavigationId,
        operation: DocumentOperationId,
        live_version: EngineDocumentVersion,
        previous_frame_version: EngineDocumentVersion,
        frame: EnginePortFrameLeaseId,
        descriptor: EngineFrameDescriptor,
    },
    DocumentRerenderRejected {
        navigation: NavigationId,
        operation: DocumentOperationId,
        live_version: Option<EngineDocumentVersion>,
        frame_version: Option<EngineDocumentVersion>,
        failure: DocumentOperationFailure,
    },
    ContextClosed {
        navigation: NavigationId,
    },
    ShutdownComplete {
        status: EnginePortShutdownStatus,
    },
}

/// One sequenced event from an [`EnginePort`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EnginePortEvent {
    sequence: EnginePortSequence,
    kind: EnginePortEventKind,
}

impl EnginePortEvent {
    /// Constructs a port event; the session independently validates sequence.
    #[must_use]
    pub const fn new(sequence: EnginePortSequence, kind: EnginePortEventKind) -> Self {
        Self { sequence, kind }
    }

    /// Monotonic event identity.
    #[must_use]
    pub const fn sequence(self) -> EnginePortSequence {
        self.sequence
    }

    /// Fixed-size payload.
    #[must_use]
    pub const fn kind(self) -> EnginePortEventKind {
        self.kind
    }
}

/// Browser-facing engine capability; implementations own all engine internals.
pub trait EnginePort: 'static {
    /// Admits one navigation for an exact top-level context.
    ///
    /// # Errors
    ///
    /// Returns [`EnginePortError`] when admission is rejected or the port is
    /// no longer usable.
    fn navigate(
        &mut self,
        context: TopLevelContextId,
        request: NavigationRequest,
    ) -> Result<NavigationId, EnginePortError>;

    /// Requests cancellation of one exact navigation.
    ///
    /// # Errors
    ///
    /// Returns [`EnginePortError`] when the identity is not cancellable or the
    /// port is no longer usable.
    fn cancel_navigation(&mut self, navigation: NavigationId) -> Result<(), EnginePortError>;

    /// Closes the context owned by one exact current navigation.
    ///
    /// # Errors
    ///
    /// Returns [`EnginePortError`] when close admission fails or the port is
    /// no longer usable.
    fn close_context(&mut self, navigation: NavigationId) -> Result<(), EnginePortError>;

    /// Returns one event, `Ok(None)` for a live empty queue, or a typed fault.
    ///
    /// # Errors
    ///
    /// Returns [`EnginePortError`] for receiver closure or a mapping contract
    /// fault.
    fn poll_event(&mut self) -> Result<Option<EnginePortEvent>, EnginePortError>;

    /// Transfers only the lease bound to `navigation` by this port.
    ///
    /// # Errors
    ///
    /// Returns [`EnginePortError`] when the exact binding cannot be transferred
    /// or its payload violates the boundary contract.
    fn take_frame(
        &mut self,
        navigation: NavigationId,
        lease: EnginePortFrameLeaseId,
    ) -> Result<EngineFrameLease, EnginePortError>;

    /// Transfers only the mutation result bound to `navigation` by this port.
    ///
    /// # Errors
    ///
    /// Returns [`EnginePortError`] when the exact binding cannot be transferred
    /// or its payload violates the boundary contract.
    fn take_mutation_result(
        &mut self,
        navigation: NavigationId,
        lease: EnginePortMutationLeaseId,
    ) -> Result<EngineMutationResultLease, EnginePortError>;

    /// Drops an exact stale frame if it remains transferable. A stale token is
    /// harmless; an implementation must never substitute a newer lease.
    ///
    /// # Errors
    ///
    /// Returns [`EnginePortError`] for every fault other than an exact stale
    /// token. `Unknown` is not evidence that this exact stale identity was
    /// harmlessly retired.
    fn discard_frame(
        &mut self,
        navigation: NavigationId,
        lease: EnginePortFrameLeaseId,
    ) -> Result<(), EnginePortError> {
        match self.take_frame(navigation, lease) {
            Ok(frame) => {
                drop(frame);
                Ok(())
            }
            Err(EnginePortError::FrameLease(FrameLeaseError::Stale)) => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Drops one exact stale result without affecting any different identity.
    ///
    /// # Errors
    ///
    /// Returns [`EnginePortError`] for every fault other than an exact stale
    /// token. `Unknown` is never silently accepted.
    fn discard_mutation_result(
        &mut self,
        navigation: NavigationId,
        lease: EnginePortMutationLeaseId,
    ) -> Result<(), EnginePortError> {
        match self.take_mutation_result(navigation, lease) {
            Ok(result) => {
                drop(result);
                Ok(())
            }
            Err(EnginePortError::MutationLease(MutationResultLeaseError::Stale)) => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Releases receiver-owned shared queued events, frame/result leases,
    /// document metadata, and resource accounting before returning, then stops
    /// and joins the engine exactly once; repeated calls are stable. An
    /// executor-owned live page remains on its worker until executor
    /// finalization during that join.
    ///
    /// Joining may block without a deadline when the underlying executor does
    /// not cooperate with shutdown. Implementations must not retain engine or
    /// receiver ownership after a terminal return, including a contained
    /// panic return.
    fn shutdown(&mut self) -> EnginePortShutdownStatus;
}

/// Failure at the narrow browser/engine boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnginePortError {
    Command(CommandErrorKind),
    ReceiverClosed(EnginePortShutdownStatus),
    FrameLease(FrameLeaseError),
    MutationLease(MutationResultLeaseError),
    LeaseNavigationMismatch {
        expected: NavigationId,
        bound: NavigationId,
    },
    ContractViolation(&'static str),
}

impl fmt::Display for EnginePortError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "browser engine port failure: {self:?}")
    }
}

impl std::error::Error for EnginePortError {}

#[derive(Clone, Copy)]
struct BoundFrame {
    navigation: NavigationId,
    lease: FrameLeaseId,
}

#[derive(Clone, Copy)]
struct BoundMutation {
    navigation: NavigationId,
    lease: MutationResultLeaseId,
}

#[derive(Clone, Copy)]
struct PendingFrameBinding {
    port: EnginePortFrameLeaseId,
    bound: BoundFrame,
}

#[derive(Clone, Copy)]
struct PendingMutationBinding {
    port: EnginePortMutationLeaseId,
    bound: BoundMutation,
}

/// Concrete adapter which inseparably owns one engine and its matching receiver.
///
/// The public API deliberately has no constructor from independently supplied
/// parts, so receivers from two engine incarnations cannot be cross-paired:
///
/// ```compile_fail
/// use wild_buzzard_engine::{EngineEventReceiver, NavigationEngine};
/// use wild_buzzard_ui::NavigationEnginePort;
///
/// fn forge_cross_pair(engine: NavigationEngine, receiver: EngineEventReceiver) {
///     let _ = NavigationEnginePort::from_parts(engine, receiver);
/// }
/// ```
pub struct NavigationEnginePort {
    engine: Option<NavigationEngine>,
    receiver: Option<EngineEventReceiver>,
    frames: BTreeMap<EnginePortFrameLeaseId, BoundFrame>,
    mutations: BTreeMap<EnginePortMutationLeaseId, BoundMutation>,
    last_frame_lease: Option<u64>,
    last_mutation_lease: Option<u64>,
    shutdown_status: Option<EnginePortShutdownStatus>,
}

impl NavigationEnginePort {
    fn from_spawned_pair(engine: NavigationEngine, receiver: EngineEventReceiver) -> Self {
        Self {
            engine: Some(engine),
            receiver: Some(receiver),
            frames: BTreeMap::new(),
            mutations: BTreeMap::new(),
            last_frame_lease: None,
            last_mutation_lease: None,
            shutdown_status: None,
        }
    }

    /// Spawns the real bounded page pipeline and wraps its public pair.
    ///
    /// # Errors
    ///
    /// Returns [`NavigationEnginePortStartError`] when the underlying bounded
    /// navigation engine cannot start.
    pub fn spawn(
        config: StaticPageConfig,
        limits: EngineLimits,
    ) -> Result<Self, NavigationEnginePortStartError> {
        let (engine, receiver) = NavigationEngine::spawn(config, limits)
            .map_err(NavigationEnginePortStartError::Engine)?;
        Ok(Self::from_spawned_pair(engine, receiver))
    }

    /// Spawns a deterministic executor while retaining the exact real worker contract.
    ///
    /// # Errors
    ///
    /// Returns [`NavigationEnginePortStartError`] when the underlying bounded
    /// navigation engine or supplied executor cannot start.
    pub fn spawn_with_executor<E, F>(
        limits: EngineLimits,
        factory: F,
    ) -> Result<Self, NavigationEnginePortStartError>
    where
        E: NavigationExecutor,
        F: FnOnce() -> Result<E, ExecutionFailure> + Send + 'static,
    {
        let (engine, receiver) = NavigationEngine::spawn_with_executor(limits, factory)
            .map_err(NavigationEnginePortStartError::Engine)?;
        Ok(Self::from_spawned_pair(engine, receiver))
    }

    fn retire_before_navigation_start(
        &mut self,
        navigation: NavigationId,
    ) -> Result<(), EnginePortError> {
        let context = navigation.context();
        let generation = navigation.generation();
        let frame_order_is_hostile = self.frames.values().any(|bound| {
            bound.navigation.context() == context && bound.navigation.generation() >= generation
        });
        let mutation_order_is_hostile = self.mutations.values().any(|bound| {
            bound.navigation.context() == context && bound.navigation.generation() >= generation
        });
        if frame_order_is_hostile || mutation_order_is_hostile {
            return Err(EnginePortError::ContractViolation(
                "navigation start followed a same- or newer-generation lease binding",
            ));
        }

        // The concrete worker keeps the prior live page until the replacement
        // frame is successfully published. Navigation start therefore proves
        // ordering only; it does not authorize retiring prior-page leases.
        Ok(())
    }

    fn preflight_frame(
        &self,
        navigation: NavigationId,
        lease: FrameLeaseId,
    ) -> Result<PendingFrameBinding, EnginePortError> {
        let port = EnginePortFrameLeaseId::new(lease.get()).ok_or(
            EnginePortError::ContractViolation("engine published a zero frame lease"),
        )?;
        if self.frames.contains_key(&port)
            || self
                .last_frame_lease
                .is_some_and(|last| lease.get() <= last)
        {
            return Err(EnginePortError::ContractViolation(
                "engine reused or reordered a frame lease identity",
            ));
        }
        if self.frames.values().any(|bound| {
            bound.navigation.context() == navigation.context()
                && bound.navigation.generation() > navigation.generation()
        }) {
            return Err(EnginePortError::ContractViolation(
                "frame publication followed a newer same-context binding",
            ));
        }
        let retiring = self
            .frames
            .values()
            .filter(|bound| bound.navigation.context() == navigation.context())
            .count();
        let projected = self
            .frames
            .len()
            .checked_sub(retiring)
            .and_then(|len| len.checked_add(1))
            .ok_or(EnginePortError::ContractViolation(
                "frame binding registry size overflowed",
            ))?;
        if projected > MAX_PENDING_FRAME_BINDINGS {
            return Err(EnginePortError::ContractViolation(
                "frame binding registry reached its hard limit",
            ));
        }
        Ok(PendingFrameBinding {
            port,
            bound: BoundFrame { navigation, lease },
        })
    }

    fn commit_frame(&mut self, pending: PendingFrameBinding) -> EnginePortFrameLeaseId {
        self.frames.retain(|_, bound| {
            bound.navigation.context() != pending.bound.navigation.context()
                || bound.navigation.generation() > pending.bound.navigation.generation()
        });
        // A successfully published frame for a newer generation proves that
        // the engine retired every older result for that context.
        self.mutations.retain(|_, bound| {
            bound.navigation.context() != pending.bound.navigation.context()
                || bound.navigation.generation() >= pending.bound.navigation.generation()
        });
        let replaced = self.frames.insert(pending.port, pending.bound);
        debug_assert!(replaced.is_none());
        self.last_frame_lease = Some(pending.bound.lease.get());
        pending.port
    }

    fn map_frame(
        &mut self,
        navigation: NavigationId,
        lease: FrameLeaseId,
    ) -> Result<EnginePortFrameLeaseId, EnginePortError> {
        let pending = self.preflight_frame(navigation, lease)?;
        Ok(self.commit_frame(pending))
    }

    fn preflight_mutation(
        &self,
        navigation: NavigationId,
        lease: MutationResultLeaseId,
    ) -> Result<PendingMutationBinding, EnginePortError> {
        let port = EnginePortMutationLeaseId::new(lease.get()).ok_or(
            EnginePortError::ContractViolation("engine published a zero result lease"),
        )?;
        if self.mutations.contains_key(&port)
            || self
                .last_mutation_lease
                .is_some_and(|last| lease.get() <= last)
        {
            return Err(EnginePortError::ContractViolation(
                "engine reused or reordered a mutation-result lease identity",
            ));
        }
        let newer_same_context = self.frames.values().any(|bound| {
            bound.navigation.context() == navigation.context()
                && bound.navigation.generation() > navigation.generation()
        }) || self.mutations.values().any(|bound| {
            bound.navigation.context() == navigation.context()
                && bound.navigation.generation() > navigation.generation()
        });
        if newer_same_context {
            return Err(EnginePortError::ContractViolation(
                "mutation publication followed a newer same-context binding",
            ));
        }
        let projected =
            self.mutations
                .len()
                .checked_add(1)
                .ok_or(EnginePortError::ContractViolation(
                    "mutation binding registry size overflowed",
                ))?;
        if projected > MAX_PENDING_MUTATION_BINDINGS {
            return Err(EnginePortError::ContractViolation(
                "mutation binding registry reached its hard limit",
            ));
        }
        Ok(PendingMutationBinding {
            port,
            bound: BoundMutation { navigation, lease },
        })
    }

    fn commit_mutation(&mut self, pending: PendingMutationBinding) -> EnginePortMutationLeaseId {
        let replaced = self.mutations.insert(pending.port, pending.bound);
        debug_assert!(replaced.is_none());
        self.last_mutation_lease = Some(pending.bound.lease.get());
        pending.port
    }

    fn map_mutation(
        &mut self,
        navigation: NavigationId,
        lease: MutationResultLeaseId,
    ) -> Result<EnginePortMutationLeaseId, EnginePortError> {
        let pending = self.preflight_mutation(navigation, lease)?;
        Ok(self.commit_mutation(pending))
    }

    fn map_descriptor(
        metadata: wild_buzzard_engine::FrameMetadata,
    ) -> Result<EngineFrameDescriptor, EnginePortError> {
        let rgba8 = metadata.rgba8();
        let descriptor = EngineFrameDescriptor::rgba8(
            rgba8.size().width(),
            rgba8.size().height(),
            rgba8.byte_len(),
        )
        .map_err(|_| {
            EnginePortError::ContractViolation(
                "engine event metadata exceeded the bounded contiguous RGBA8 contract",
            )
        })?;
        if descriptor.stride() != rgba8.stride() {
            return Err(EnginePortError::ContractViolation(
                "engine event metadata announced a noncontiguous RGBA8 stride",
            ));
        }
        Ok(descriptor)
    }

    #[allow(clippy::too_many_lines)]
    fn map_event(
        &mut self,
        event: wild_buzzard_engine::EngineEvent,
    ) -> Result<EnginePortEvent, EnginePortError> {
        let sequence = EnginePortSequence::new(event.sequence().get()).ok_or(
            EnginePortError::ContractViolation("engine published a zero event sequence"),
        )?;
        let kind = match event.kind() {
            EngineEventKind::NavigationStarted { navigation } => {
                self.retire_before_navigation_start(navigation)?;
                EnginePortEventKind::NavigationStarted { navigation }
            }
            EngineEventKind::NavigationCommitted {
                navigation,
                http_status,
            } => EnginePortEventKind::NavigationCommitted {
                navigation,
                http_status,
            },
            EngineEventKind::FrameReady {
                navigation,
                lease,
                metadata,
            } => {
                let descriptor = Self::map_descriptor(metadata)?;
                EnginePortEventKind::FrameReady {
                    navigation,
                    lease: self.map_frame(navigation, lease)?,
                    descriptor,
                    document_version: metadata
                        .document_version()
                        .map(|version| project_document_version!(version)),
                }
            }
            EngineEventKind::NavigationCancelled { navigation } => {
                EnginePortEventKind::NavigationCancelled { navigation }
            }
            EngineEventKind::NavigationFailed {
                navigation,
                failure,
            } => EnginePortEventKind::NavigationFailed {
                navigation,
                failure,
            },
            EngineEventKind::DocumentMutationRendered {
                navigation,
                operation,
                previous_live_version,
                previous_frame_version,
                live_version,
                result,
                created_nodes,
                frame,
                metadata,
            } => {
                // Both independent one-shot identities are validated before
                // either registry changes, including bounded metadata, so a
                // bad frame cannot strand a newly installed result binding.
                let descriptor = Self::map_descriptor(metadata)?;
                if metadata.document_version() != Some(live_version) {
                    return Err(EnginePortError::ContractViolation(
                        "rendered mutation frame metadata disagreed with its live document version",
                    ));
                }
                let pending_result = self.preflight_mutation(navigation, result)?;
                let pending_frame = self.preflight_frame(navigation, frame)?;
                EnginePortEventKind::DocumentMutationRendered {
                    navigation,
                    operation,
                    previous_live_version: project_document_version!(previous_live_version),
                    previous_frame_version: project_document_version!(previous_frame_version),
                    live_version: project_document_version!(live_version),
                    result: self.commit_mutation(pending_result),
                    created_nodes,
                    frame: self.commit_frame(pending_frame),
                    descriptor,
                }
            }
            EngineEventKind::DocumentMutationCommittedWithoutFrame {
                navigation,
                operation,
                previous_live_version,
                live_version,
                frame_version,
                result,
                created_nodes,
                failure,
            } => EnginePortEventKind::DocumentMutationCommittedWithoutFrame {
                navigation,
                operation,
                previous_live_version: project_document_version!(previous_live_version),
                live_version: project_document_version!(live_version),
                frame_version: project_document_version!(frame_version),
                result: self.map_mutation(navigation, result)?,
                created_nodes,
                failure,
            },
            EngineEventKind::DocumentMutationRejected {
                navigation,
                operation,
                live_version,
                frame_version,
                failure,
            } => EnginePortEventKind::DocumentMutationRejected {
                navigation,
                operation,
                live_version: live_version.map(|version| project_document_version!(version)),
                frame_version: frame_version.map(|version| project_document_version!(version)),
                failure,
            },
            EngineEventKind::DocumentRerendered {
                navigation,
                operation,
                live_version,
                previous_frame_version,
                frame,
                metadata,
            } => {
                let descriptor = Self::map_descriptor(metadata)?;
                if metadata.document_version() != Some(live_version) {
                    return Err(EnginePortError::ContractViolation(
                        "rerendered frame metadata disagreed with its live document version",
                    ));
                }
                EnginePortEventKind::DocumentRerendered {
                    navigation,
                    operation,
                    live_version: project_document_version!(live_version),
                    previous_frame_version: project_document_version!(previous_frame_version),
                    frame: self.map_frame(navigation, frame)?,
                    descriptor,
                }
            }
            EngineEventKind::DocumentRerenderRejected {
                navigation,
                operation,
                live_version,
                frame_version,
                failure,
            } => EnginePortEventKind::DocumentRerenderRejected {
                navigation,
                operation,
                live_version: live_version.map(|version| project_document_version!(version)),
                frame_version: frame_version.map(|version| project_document_version!(version)),
                failure,
            },
            EngineEventKind::ContextClosed { navigation } => {
                self.frames
                    .retain(|_, bound| bound.navigation.context() != navigation.context());
                self.mutations
                    .retain(|_, bound| bound.navigation.context() != navigation.context());
                EnginePortEventKind::ContextClosed { navigation }
            }
            EngineEventKind::ShutdownComplete { status } => {
                self.frames.clear();
                self.mutations.clear();
                EnginePortEventKind::ShutdownComplete {
                    status: EnginePortShutdownStatus::from_navigation(status),
                }
            }
        };
        Ok(EnginePortEvent::new(sequence, kind))
    }
}

impl EnginePort for NavigationEnginePort {
    fn navigate(
        &mut self,
        context: TopLevelContextId,
        request: NavigationRequest,
    ) -> Result<NavigationId, EnginePortError> {
        if let Some(status) = self.shutdown_status {
            return Err(EnginePortError::ReceiverClosed(status));
        }
        self.engine
            .as_ref()
            .ok_or_else(|| {
                EnginePortError::ReceiverClosed(EnginePortShutdownStatus::port_panicked())
            })?
            .navigate(context, request)
            .map_err(|error| EnginePortError::Command(error.kind()))
    }

    fn cancel_navigation(&mut self, navigation: NavigationId) -> Result<(), EnginePortError> {
        if let Some(status) = self.shutdown_status {
            return Err(EnginePortError::ReceiverClosed(status));
        }
        self.engine
            .as_ref()
            .ok_or_else(|| {
                EnginePortError::ReceiverClosed(EnginePortShutdownStatus::port_panicked())
            })?
            .cancel_navigation(navigation)
            .map(|_| ())
            .map_err(|error| EnginePortError::Command(error.kind()))
    }

    fn close_context(&mut self, navigation: NavigationId) -> Result<(), EnginePortError> {
        if let Some(status) = self.shutdown_status {
            return Err(EnginePortError::ReceiverClosed(status));
        }
        self.engine
            .as_ref()
            .ok_or_else(|| {
                EnginePortError::ReceiverClosed(EnginePortShutdownStatus::port_panicked())
            })?
            .close_context(navigation)
            .map(|_| ())
            .map_err(|error| EnginePortError::Command(error.kind()))
    }

    fn poll_event(&mut self) -> Result<Option<EnginePortEvent>, EnginePortError> {
        let closed_status = self
            .shutdown_status
            .unwrap_or_else(EnginePortShutdownStatus::port_panicked);
        let Some(receiver) = self.receiver.as_mut() else {
            return Err(EnginePortError::ReceiverClosed(closed_status));
        };
        match receiver.try_recv() {
            Ok(event) => self.map_event(event).map(Some),
            Err(EventReceiveError::Empty) => Ok(None),
            Err(EventReceiveError::Closed(status)) => Err(EnginePortError::ReceiverClosed(
                EnginePortShutdownStatus::from_navigation(status),
            )),
        }
    }

    fn take_frame(
        &mut self,
        navigation: NavigationId,
        lease: EnginePortFrameLeaseId,
    ) -> Result<EngineFrameLease, EnginePortError> {
        let closed_status = self
            .shutdown_status
            .unwrap_or_else(EnginePortShutdownStatus::port_panicked);
        let Some(receiver) = self.receiver.as_mut() else {
            return Err(EnginePortError::ReceiverClosed(closed_status));
        };
        let bound = self
            .frames
            .get(&lease)
            .copied()
            .ok_or(EnginePortError::FrameLease(FrameLeaseError::Unknown))?;
        if bound.navigation != navigation {
            return Err(EnginePortError::LeaseNavigationMismatch {
                expected: navigation,
                bound: bound.navigation,
            });
        }
        let frame = match receiver.take_frame(bound.lease) {
            Ok(frame) => frame,
            Err(FrameLeaseError::Stale) => {
                // The receiver authoritatively says this exact one-shot token
                // can never succeed. Retiring this key cannot affect the
                // newer differently keyed lease which made it stale.
                self.frames.remove(&lease);
                return Err(EnginePortError::FrameLease(FrameLeaseError::Stale));
            }
            Err(error) => return Err(EnginePortError::FrameLease(error)),
        };
        // The engine transfer is now authoritative and one-shot. Retire this
        // exact adapter binding even if the transferred payload then proves a
        // terminal engine contract fault.
        self.frames.remove(&lease);
        if frame.navigation() != navigation || frame.lease_id() != bound.lease {
            return Err(EnginePortError::ContractViolation(
                "engine transferred a frame other than the exact bound lease",
            ));
        }
        EngineFrameLease::from_navigation(frame, lease)
    }

    fn take_mutation_result(
        &mut self,
        navigation: NavigationId,
        lease: EnginePortMutationLeaseId,
    ) -> Result<EngineMutationResultLease, EnginePortError> {
        let closed_status = self
            .shutdown_status
            .unwrap_or_else(EnginePortShutdownStatus::port_panicked);
        let Some(receiver) = self.receiver.as_mut() else {
            return Err(EnginePortError::ReceiverClosed(closed_status));
        };
        let bound = self
            .mutations
            .get(&lease)
            .copied()
            .ok_or(EnginePortError::MutationLease(
                MutationResultLeaseError::Unknown,
            ))?;
        if bound.navigation != navigation {
            return Err(EnginePortError::LeaseNavigationMismatch {
                expected: navigation,
                bound: bound.navigation,
            });
        }
        let result = match receiver.take_mutation_result(bound.lease) {
            Ok(result) => result,
            Err(MutationResultLeaseError::Stale) => {
                self.mutations.remove(&lease);
                return Err(EnginePortError::MutationLease(
                    MutationResultLeaseError::Stale,
                ));
            }
            Err(error) => return Err(EnginePortError::MutationLease(error)),
        };
        self.mutations.remove(&lease);
        if result.navigation() != navigation || result.lease_id() != bound.lease {
            return Err(EnginePortError::ContractViolation(
                "engine transferred a result other than the exact bound lease",
            ));
        }
        Ok(EngineMutationResultLease::from_navigation(result, lease))
    }

    fn shutdown(&mut self) -> EnginePortShutdownStatus {
        if let Some(status) = self.shutdown_status {
            return status;
        }
        self.frames.clear();
        self.mutations.clear();
        let mut engine = self.engine.take();
        let mut panicked = false;

        if let Some(engine) = engine.as_ref() {
            panicked |= catch_unwind(AssertUnwindSafe(|| {
                let _ = engine.request_shutdown();
            }))
            .is_err();
        }

        // Receiver destruction owns the authoritative release of shared queued
        // events, frame/result leases, retained-document metadata, and charges.
        // The executor-owned live page remains on the worker until executor
        // finalization during the potentially unbounded join.
        panicked |= catch_unwind(AssertUnwindSafe(|| drop(self.receiver.take()))).is_err();

        let joined = engine.as_mut().and_then(|engine| {
            let Ok(status) = catch_unwind(AssertUnwindSafe(|| engine.shutdown())) else {
                panicked = true;
                return None;
            };
            Some(status)
        });
        panicked |= catch_unwind(AssertUnwindSafe(|| drop(engine))).is_err();

        let status = if panicked {
            EnginePortShutdownStatus::port_panicked()
        } else {
            joined.map_or_else(
                EnginePortShutdownStatus::port_panicked,
                EnginePortShutdownStatus::from_navigation,
            )
        };
        self.shutdown_status = Some(status);
        status
    }
}

impl Drop for NavigationEnginePort {
    fn drop(&mut self) {
        let _ = catch_unwind(AssertUnwindSafe(|| self.shutdown()));
    }
}

/// Failure before a concrete navigation-engine port exists.
#[derive(Debug)]
pub enum NavigationEnginePortStartError {
    Engine(EngineStartError),
}

impl fmt::Display for NavigationEnginePortStartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Engine(error) => write!(formatter, "failed to start navigation engine: {error}"),
        }
    }
}

impl std::error::Error for NavigationEnginePortStartError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Engine(error) => Some(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use wild_buzzard_dom::bindings::{
        CreatedNodeToken, ScriptMutationBatch, ScriptMutationCommand, ScriptMutationLimits,
    };
    use wild_buzzard_dom::{Document, DocumentVersion};
    use wild_buzzard_engine::{
        CancellationToken, CommandReceipt, DocumentLoadProof, DocumentMutationCommit, EngineEvent,
        EngineEventKind, EngineFrame, ExecutorDocumentMutation, ExecutorDocumentRerender,
        ExecutorOutput, NavigationGeneration, NavigationRequest,
    };

    use super::*;

    fn limits() -> EngineLimits {
        EngineLimits::new(4, 16, 4, 16, 64)
            .unwrap()
            .with_max_retained_document_nodes(64)
            .unwrap()
            .with_max_retained_mutation_result_nodes(16)
            .unwrap()
    }

    fn frame(marker: u8) -> EngineFrame {
        EngineFrame::from_rgba8(PixelSize::new(1, 1).unwrap(), vec![marker, 2, 3, 255]).unwrap()
    }

    fn native_document_version(frame: &EngineFrameLease) -> DocumentVersion {
        match &frame.backing {
            FrameBacking::Navigation(frame) => frame.document_version().unwrap(),
            FrameBacking::Owned(_) => panic!("expected a navigation-engine frame"),
        }
    }

    struct PixelExecutor;

    impl NavigationExecutor for PixelExecutor {
        fn execute(
            &mut self,
            navigation: NavigationId,
            _request: &NavigationRequest,
            _cancellation: &CancellationToken,
        ) -> Result<ExecutorOutput, ExecutionFailure> {
            ExecutorOutput::new(
                200,
                frame(u8::try_from(navigation.generation().get()).unwrap()),
            )
        }

        fn shutdown(&mut self) -> Result<(), ExecutionFailure> {
            Ok(())
        }
    }

    fn next_raw(port: &mut NavigationEnginePort) -> EngineEvent {
        port.receiver.as_mut().unwrap().recv().unwrap()
    }

    struct BufferedRawPort {
        inner: NavigationEnginePort,
        raw_events: VecDeque<EngineEvent>,
    }

    impl BufferedRawPort {
        fn new(inner: NavigationEnginePort) -> Self {
            Self {
                inner,
                raw_events: VecDeque::new(),
            }
        }

        fn buffer_raw(&mut self, count: usize) {
            for _ in 0..count {
                self.raw_events.push_back(next_raw(&mut self.inner));
            }
        }
    }

    impl EnginePort for BufferedRawPort {
        fn navigate(
            &mut self,
            context: TopLevelContextId,
            request: NavigationRequest,
        ) -> Result<NavigationId, EnginePortError> {
            self.inner.navigate(context, request)
        }

        fn cancel_navigation(&mut self, navigation: NavigationId) -> Result<(), EnginePortError> {
            self.inner.cancel_navigation(navigation)
        }

        fn close_context(&mut self, navigation: NavigationId) -> Result<(), EnginePortError> {
            self.inner.close_context(navigation)
        }

        fn poll_event(&mut self) -> Result<Option<EnginePortEvent>, EnginePortError> {
            match self.raw_events.pop_front() {
                Some(event) => self.inner.map_event(event).map(Some),
                None => self.inner.poll_event(),
            }
        }

        fn take_frame(
            &mut self,
            navigation: NavigationId,
            lease: EnginePortFrameLeaseId,
        ) -> Result<EngineFrameLease, EnginePortError> {
            self.inner.take_frame(navigation, lease)
        }

        fn take_mutation_result(
            &mut self,
            navigation: NavigationId,
            lease: EnginePortMutationLeaseId,
        ) -> Result<EngineMutationResultLease, EnginePortError> {
            self.inner.take_mutation_result(navigation, lease)
        }

        fn shutdown(&mut self) -> EnginePortShutdownStatus {
            self.raw_events.clear();
            self.inner.shutdown()
        }
    }

    fn mapped_initial_frame(
        port: &mut NavigationEnginePort,
        navigation: NavigationId,
    ) -> EnginePortFrameLeaseId {
        let started = next_raw(port);
        assert!(matches!(
            port.map_event(started).unwrap().kind(),
            EnginePortEventKind::NavigationStarted { .. }
        ));
        let committed = next_raw(port);
        assert!(matches!(
            port.map_event(committed).unwrap().kind(),
            EnginePortEventKind::NavigationCommitted { .. }
        ));
        let ready = next_raw(port);
        let mapped = port.map_event(ready).unwrap();
        let EnginePortEventKind::FrameReady {
            navigation: actual,
            lease,
            ..
        } = mapped.kind()
        else {
            panic!("expected mapped frame event");
        };
        assert_eq!(actual, navigation);
        lease
    }

    #[test]
    fn duplicate_preflight_is_non_mutating_and_stale_transfer_retires_only_that_binding() {
        let mut port =
            NavigationEnginePort::spawn_with_executor(limits(), || Ok(PixelExecutor)).unwrap();
        let context = TopLevelContextId::new(1).unwrap();
        let navigation = port
            .navigate(
                context,
                NavigationRequest::new("https://adapter.invalid/").unwrap(),
            )
            .unwrap();
        let lease = mapped_initial_frame(&mut port, navigation);
        let bound = port.frames.get(&lease).copied().unwrap();
        let frames_before = port.frames.len();
        let last_before = port.last_frame_lease;

        assert!(matches!(
            port.preflight_frame(navigation, bound.lease),
            Err(EnginePortError::ContractViolation(_))
        ));
        assert_eq!(port.frames.len(), frames_before);
        assert_eq!(port.last_frame_lease, last_before);
        assert_eq!(port.frames.get(&lease).unwrap().navigation, navigation);

        let stolen = port
            .receiver
            .as_mut()
            .unwrap()
            .take_frame(bound.lease)
            .unwrap();
        assert_eq!(stolen.navigation(), navigation);
        assert!(matches!(
            port.take_frame(navigation, lease),
            Err(EnginePortError::FrameLease(FrameLeaseError::Stale))
        ));
        assert!(!port.frames.contains_key(&lease));
        let _ = port.shutdown();
    }

    #[test]
    fn concrete_shutdown_releases_receiver_and_engine_owners_before_return() {
        let mut port =
            NavigationEnginePort::spawn_with_executor(limits(), || Ok(PixelExecutor)).unwrap();
        let context = TopLevelContextId::new(1).unwrap();
        let navigation = port
            .navigate(
                context,
                NavigationRequest::new("https://shutdown.invalid/").unwrap(),
            )
            .unwrap();
        let status = port.shutdown();
        assert_eq!(status.reason(), EnginePortStopReason::Requested);
        assert!(port.receiver.is_none());
        assert!(port.engine.is_none());
        assert!(port.frames.is_empty());
        assert!(port.mutations.is_empty());
        assert_eq!(port.shutdown(), status);
        assert_eq!(
            port.poll_event(),
            Err(EnginePortError::ReceiverClosed(status))
        );
        assert_eq!(
            port.cancel_navigation(navigation),
            Err(EnginePortError::ReceiverClosed(status))
        );
    }

    #[test]
    fn ui_frame_descriptor_is_fallible_bounded_and_matches_real_transfer() {
        assert!(matches!(
            EngineFrameDescriptor::rgba8(1, 1, 3),
            Err(EngineFrameError::WrongByteLength { .. })
        ));
        assert!(EngineFrameDescriptor::rgba8(0, 1, 0).is_err());
        let oversized_bytes = 8_193_usize * 8_192 * 4;
        assert!(matches!(
            EngineFrameDescriptor::rgba8(8_193, 8_192, oversized_bytes),
            Err(EngineFrameError::FrameTooLarge { .. })
        ));

        let mut port = NavigationEnginePort::spawn_with_executor(limits(), || {
            Ok(DocumentExecutor { document: None })
        })
        .unwrap();
        let navigation = port
            .navigate(
                TopLevelContextId::new(1).unwrap(),
                NavigationRequest::new("https://descriptor.invalid/").unwrap(),
            )
            .unwrap();
        let lease = mapped_initial_frame(&mut port, navigation);
        let frame = port.take_frame(navigation, lease).unwrap();
        assert_eq!(
            frame.descriptor(),
            EngineFrameDescriptor::rgba8(1, 1, 4).unwrap()
        );
        assert_eq!(frame.pixels().len(), frame.descriptor().byte_len());
        assert_eq!(frame.descriptor().stride(), 4);
        assert!(frame.document_version().is_some());
        let _ = port.shutdown();
    }

    struct DocumentExecutor {
        document: Option<Document>,
    }

    impl NavigationExecutor for DocumentExecutor {
        fn execute(
            &mut self,
            navigation: NavigationId,
            _request: &NavigationRequest,
            _cancellation: &CancellationToken,
        ) -> Result<ExecutorOutput, ExecutionFailure> {
            let document = Document::new();
            let version = document.version();
            let proof = DocumentLoadProof::from_snapshot(&document.snapshot().unwrap());
            let frame = EngineFrame::from_rgba8_for_document(
                PixelSize::new(1, 1).unwrap(),
                vec![
                    u8::try_from(navigation.generation().get()).unwrap(),
                    2,
                    3,
                    255,
                ],
                version,
            )
            .unwrap();
            self.document = Some(document);
            ExecutorOutput::new_document(200, frame, proof)
        }

        fn mutate_document(
            &mut self,
            _navigation: NavigationId,
            batch: ScriptMutationBatch,
            _cancellation: &CancellationToken,
        ) -> ExecutorDocumentMutation {
            let document = self.document.as_mut().unwrap();
            let previous_live_version = document.version();
            let commit = document
                .apply_script_mutations(batch, ScriptMutationLimits::DEFAULT)
                .unwrap();
            let live_version = commit.version();
            let commit = DocumentMutationCommit::from_script_commit(commit);
            let frame = EngineFrame::from_rgba8_for_document(
                PixelSize::new(2, 1).unwrap(),
                vec![77, 2, 3, 255, 88, 5, 6, 255],
                live_version,
            )
            .unwrap();
            ExecutorDocumentMutation::Rendered {
                previous_live_version,
                previous_frame_version: previous_live_version,
                commit,
                frame,
            }
        }

        fn shutdown(&mut self) -> Result<(), ExecutionFailure> {
            Ok(())
        }
    }

    struct FailThirdDocumentExecutor {
        document: Option<Document>,
    }

    struct FailReplacementDocumentExecutor {
        document: Option<Document>,
    }

    impl NavigationExecutor for FailReplacementDocumentExecutor {
        fn execute(
            &mut self,
            navigation: NavigationId,
            _request: &NavigationRequest,
            _cancellation: &CancellationToken,
        ) -> Result<ExecutorOutput, ExecutionFailure> {
            if navigation.generation().get() == 2 {
                return Err(ExecutionFailure::new(
                    crate::ExecutionFailureKind::Network,
                    crate::NavigationStage::Fetch,
                ));
            }
            let document = Document::new();
            let version = document.version();
            let proof = DocumentLoadProof::from_snapshot(&document.snapshot().unwrap());
            let frame = EngineFrame::from_rgba8_for_document(
                PixelSize::new(1, 1).unwrap(),
                vec![1, 2, 3, 255],
                version,
            )
            .unwrap();
            self.document = Some(document);
            ExecutorOutput::new_document(200, frame, proof)
        }

        fn mutate_document(
            &mut self,
            _navigation: NavigationId,
            batch: ScriptMutationBatch,
            _cancellation: &CancellationToken,
        ) -> ExecutorDocumentMutation {
            let document = self.document.as_mut().unwrap();
            let previous_live_version = document.version();
            let commit = document
                .apply_script_mutations(batch, ScriptMutationLimits::DEFAULT)
                .unwrap();
            ExecutorDocumentMutation::CommittedWithoutFrame {
                previous_live_version,
                frame_version: previous_live_version,
                commit: DocumentMutationCommit::from_script_commit(commit),
                failure: DocumentOperationFailure::ResourceLimit,
            }
        }

        fn shutdown(&mut self) -> Result<(), ExecutionFailure> {
            Ok(())
        }
    }

    impl NavigationExecutor for FailThirdDocumentExecutor {
        fn execute(
            &mut self,
            navigation: NavigationId,
            _request: &NavigationRequest,
            _cancellation: &CancellationToken,
        ) -> Result<ExecutorOutput, ExecutionFailure> {
            if navigation.generation().get() == 3 {
                return Err(ExecutionFailure::new(
                    crate::ExecutionFailureKind::Network,
                    crate::NavigationStage::Fetch,
                ));
            }
            let document = Document::new();
            let version = document.version();
            let proof = DocumentLoadProof::from_snapshot(&document.snapshot().unwrap());
            let frame = EngineFrame::from_rgba8_for_document(
                PixelSize::new(1, 1).unwrap(),
                vec![
                    u8::try_from(navigation.generation().get()).unwrap(),
                    2,
                    3,
                    255,
                ],
                version,
            )
            .unwrap();
            self.document = Some(document);
            ExecutorOutput::new_document(200, frame, proof)
        }

        fn shutdown(&mut self) -> Result<(), ExecutionFailure> {
            Ok(())
        }
    }

    struct NoFrameDocumentExecutor {
        document: Option<Document>,
    }

    struct RejectingDocumentExecutor {
        document: Option<Document>,
    }

    impl NavigationExecutor for RejectingDocumentExecutor {
        fn execute(
            &mut self,
            navigation: NavigationId,
            _request: &NavigationRequest,
            _cancellation: &CancellationToken,
        ) -> Result<ExecutorOutput, ExecutionFailure> {
            let document = Document::new();
            let version = document.version();
            let proof = DocumentLoadProof::from_snapshot(&document.snapshot().unwrap());
            let frame = EngineFrame::from_rgba8_for_document(
                PixelSize::new(1, 1).unwrap(),
                vec![
                    u8::try_from(navigation.generation().get()).unwrap(),
                    2,
                    3,
                    255,
                ],
                version,
            )
            .unwrap();
            self.document = Some(document);
            ExecutorOutput::new_document(200, frame, proof)
        }

        fn mutate_document(
            &mut self,
            _navigation: NavigationId,
            _batch: ScriptMutationBatch,
            _cancellation: &CancellationToken,
        ) -> ExecutorDocumentMutation {
            let version = self.document.as_ref().unwrap().version();
            ExecutorDocumentMutation::Rejected {
                live_version: Some(version),
                frame_version: Some(version),
                failure: DocumentOperationFailure::MutationRejected,
            }
        }

        fn shutdown(&mut self) -> Result<(), ExecutionFailure> {
            Ok(())
        }
    }

    struct RerenderDocumentExecutor {
        document: Option<Document>,
        reject: bool,
    }

    impl NavigationExecutor for RerenderDocumentExecutor {
        fn execute(
            &mut self,
            navigation: NavigationId,
            _request: &NavigationRequest,
            _cancellation: &CancellationToken,
        ) -> Result<ExecutorOutput, ExecutionFailure> {
            let document = Document::new();
            let version = document.version();
            let proof = DocumentLoadProof::from_snapshot(&document.snapshot().unwrap());
            let frame = EngineFrame::from_rgba8_for_document(
                PixelSize::new(1, 1).unwrap(),
                vec![
                    u8::try_from(navigation.generation().get()).unwrap(),
                    2,
                    3,
                    255,
                ],
                version,
            )
            .unwrap();
            self.document = Some(document);
            ExecutorOutput::new_document(200, frame, proof)
        }

        fn rerender_document(
            &mut self,
            _navigation: NavigationId,
            expected_live_version: DocumentVersion,
            _cancellation: &CancellationToken,
        ) -> ExecutorDocumentRerender {
            let version = self.document.as_ref().unwrap().version();
            assert_eq!(version, expected_live_version);
            if self.reject {
                ExecutorDocumentRerender::Rejected {
                    live_version: Some(version),
                    frame_version: Some(version),
                    failure: DocumentOperationFailure::Rendering,
                }
            } else {
                ExecutorDocumentRerender::Rendered {
                    live_version: version,
                    previous_frame_version: version,
                    frame: EngineFrame::from_rgba8_for_document(
                        PixelSize::new(1, 1).unwrap(),
                        vec![55, 66, 77, 255],
                        version,
                    )
                    .unwrap(),
                }
            }
        }

        fn shutdown(&mut self) -> Result<(), ExecutionFailure> {
            Ok(())
        }
    }

    impl NavigationExecutor for NoFrameDocumentExecutor {
        fn execute(
            &mut self,
            navigation: NavigationId,
            _request: &NavigationRequest,
            _cancellation: &CancellationToken,
        ) -> Result<ExecutorOutput, ExecutionFailure> {
            let document = Document::new();
            let version = document.version();
            let proof = DocumentLoadProof::from_snapshot(&document.snapshot().unwrap());
            let frame = EngineFrame::from_rgba8_for_document(
                PixelSize::new(1, 1).unwrap(),
                vec![
                    u8::try_from(navigation.generation().get()).unwrap(),
                    2,
                    3,
                    255,
                ],
                version,
            )
            .unwrap();
            self.document = Some(document);
            ExecutorOutput::new_document(200, frame, proof)
        }

        fn mutate_document(
            &mut self,
            _navigation: NavigationId,
            batch: ScriptMutationBatch,
            _cancellation: &CancellationToken,
        ) -> ExecutorDocumentMutation {
            let document = self.document.as_mut().unwrap();
            let previous_live_version = document.version();
            let commit = document
                .apply_script_mutations(batch, ScriptMutationLimits::DEFAULT)
                .unwrap();
            ExecutorDocumentMutation::CommittedWithoutFrame {
                previous_live_version,
                frame_version: previous_live_version,
                commit: DocumentMutationCommit::from_script_commit(commit),
                failure: DocumentOperationFailure::ResourceLimit,
            }
        }

        fn shutdown(&mut self) -> Result<(), ExecutionFailure> {
            Ok(())
        }
    }

    #[derive(Clone, Copy, Debug)]
    enum MutationResultMismatch {
        Navigation,
        Lease,
        Operation,
        LiveVersion,
        CreatedNodes,
    }

    #[derive(Clone, Copy, Debug)]
    enum MutationEventVersionMismatch {
        RenderedPreviousLive,
        RenderedPreviousFrame,
        RenderedLive,
        RenderedLiveRollback,
        RenderedLiveForeignDocument,
        RenderedLiveZeroDocument,
        CommittedPreviousLive,
        CommittedLive,
        CommittedLiveRollback,
        CommittedLiveForeignDocument,
        CommittedLiveZeroDocument,
        CommittedFrame,
        RejectedLiveMissing,
        RejectedLiveSkip,
        RejectedLiveForeignDocument,
        RejectedLiveZeroDocument,
        RejectedFrameAhead,
        RerenderLiveSkip,
        RerenderLiveForeignDocument,
        RerenderLiveZeroDocument,
        RerenderPreviousFrameAhead,
        RerenderRejectedLiveMissing,
        RerenderRejectedLiveSkip,
        RerenderRejectedLiveForeignDocument,
        RerenderRejectedLiveZeroDocument,
        RerenderRejectedFrameAhead,
    }

    struct HostileMutationResultPort {
        inner: NavigationEnginePort,
        result_mismatch: Option<MutationResultMismatch>,
        event_mismatch: Option<MutationEventVersionMismatch>,
        frame_take: Option<LeaseTakeBehavior>,
        result_take: Option<LeaseTakeBehavior>,
        foreign_operation: DocumentOperationId,
    }

    #[derive(Clone, Copy, Debug)]
    enum LeaseTakeBehavior {
        Stale,
        Unknown,
        Panic,
    }

    fn next_projected_version(version: EngineDocumentVersion) -> EngineDocumentVersion {
        EngineDocumentVersion::new(
            version.document(),
            version
                .revision()
                .checked_add(1)
                .expect("test revision has room"),
        )
    }

    impl EnginePort for HostileMutationResultPort {
        fn navigate(
            &mut self,
            context: TopLevelContextId,
            request: NavigationRequest,
        ) -> Result<NavigationId, EnginePortError> {
            self.inner.navigate(context, request)
        }

        fn cancel_navigation(&mut self, navigation: NavigationId) -> Result<(), EnginePortError> {
            self.inner.cancel_navigation(navigation)
        }

        fn close_context(&mut self, navigation: NavigationId) -> Result<(), EnginePortError> {
            self.inner.close_context(navigation)
        }

        #[allow(clippy::too_many_lines)]
        fn poll_event(&mut self) -> Result<Option<EnginePortEvent>, EnginePortError> {
            let Some(event) = self.inner.poll_event()? else {
                return Ok(None);
            };
            let Some(mismatch) = self.event_mismatch else {
                return Ok(Some(event));
            };
            let kind = match (mismatch, event.kind()) {
                (
                    MutationEventVersionMismatch::RenderedPreviousLive,
                    EnginePortEventKind::DocumentMutationRendered {
                        navigation,
                        operation,
                        previous_live_version,
                        previous_frame_version,
                        live_version,
                        result,
                        created_nodes,
                        frame,
                        descriptor,
                    },
                ) => EnginePortEventKind::DocumentMutationRendered {
                    navigation,
                    operation,
                    previous_live_version: next_projected_version(previous_live_version),
                    previous_frame_version,
                    live_version,
                    result,
                    created_nodes,
                    frame,
                    descriptor,
                },
                (
                    MutationEventVersionMismatch::RenderedPreviousFrame,
                    EnginePortEventKind::DocumentMutationRendered {
                        navigation,
                        operation,
                        previous_live_version,
                        previous_frame_version,
                        live_version,
                        result,
                        created_nodes,
                        frame,
                        descriptor,
                    },
                ) => EnginePortEventKind::DocumentMutationRendered {
                    navigation,
                    operation,
                    previous_live_version,
                    previous_frame_version: next_projected_version(previous_frame_version),
                    live_version,
                    result,
                    created_nodes,
                    frame,
                    descriptor,
                },
                (
                    mismatch @ (MutationEventVersionMismatch::RenderedLive
                    | MutationEventVersionMismatch::RenderedLiveRollback
                    | MutationEventVersionMismatch::RenderedLiveForeignDocument
                    | MutationEventVersionMismatch::RenderedLiveZeroDocument),
                    EnginePortEventKind::DocumentMutationRendered {
                        navigation,
                        operation,
                        previous_live_version,
                        previous_frame_version,
                        live_version,
                        result,
                        created_nodes,
                        frame,
                        descriptor,
                    },
                ) => {
                    let live_version = match mismatch {
                        MutationEventVersionMismatch::RenderedLive => {
                            next_projected_version(live_version)
                        }
                        MutationEventVersionMismatch::RenderedLiveRollback => previous_live_version,
                        MutationEventVersionMismatch::RenderedLiveForeignDocument => {
                            EngineDocumentVersion::new(
                                live_version.document().checked_add(10_000).unwrap(),
                                live_version.revision(),
                            )
                        }
                        MutationEventVersionMismatch::RenderedLiveZeroDocument => {
                            EngineDocumentVersion::new(0, live_version.revision())
                        }
                        _ => unreachable!(),
                    };
                    EnginePortEventKind::DocumentMutationRendered {
                        navigation,
                        operation,
                        previous_live_version,
                        previous_frame_version,
                        live_version,
                        result,
                        created_nodes,
                        frame,
                        descriptor,
                    }
                }
                (
                    MutationEventVersionMismatch::CommittedPreviousLive,
                    EnginePortEventKind::DocumentMutationCommittedWithoutFrame {
                        navigation,
                        operation,
                        previous_live_version,
                        live_version,
                        frame_version,
                        result,
                        created_nodes,
                        failure,
                    },
                ) => EnginePortEventKind::DocumentMutationCommittedWithoutFrame {
                    navigation,
                    operation,
                    previous_live_version: next_projected_version(previous_live_version),
                    live_version,
                    frame_version,
                    result,
                    created_nodes,
                    failure,
                },
                (
                    mismatch @ (MutationEventVersionMismatch::CommittedLive
                    | MutationEventVersionMismatch::CommittedLiveRollback
                    | MutationEventVersionMismatch::CommittedLiveForeignDocument
                    | MutationEventVersionMismatch::CommittedLiveZeroDocument),
                    EnginePortEventKind::DocumentMutationCommittedWithoutFrame {
                        navigation,
                        operation,
                        previous_live_version,
                        live_version,
                        frame_version,
                        result,
                        created_nodes,
                        failure,
                    },
                ) => {
                    let live_version = match mismatch {
                        MutationEventVersionMismatch::CommittedLive => {
                            next_projected_version(live_version)
                        }
                        MutationEventVersionMismatch::CommittedLiveRollback => {
                            previous_live_version
                        }
                        MutationEventVersionMismatch::CommittedLiveForeignDocument => {
                            EngineDocumentVersion::new(
                                live_version.document().checked_add(10_000).unwrap(),
                                live_version.revision(),
                            )
                        }
                        MutationEventVersionMismatch::CommittedLiveZeroDocument => {
                            EngineDocumentVersion::new(0, live_version.revision())
                        }
                        _ => unreachable!(),
                    };
                    EnginePortEventKind::DocumentMutationCommittedWithoutFrame {
                        navigation,
                        operation,
                        previous_live_version,
                        live_version,
                        frame_version,
                        result,
                        created_nodes,
                        failure,
                    }
                }
                (
                    MutationEventVersionMismatch::CommittedFrame,
                    EnginePortEventKind::DocumentMutationCommittedWithoutFrame {
                        navigation,
                        operation,
                        previous_live_version,
                        live_version,
                        frame_version,
                        result,
                        created_nodes,
                        failure,
                    },
                ) => EnginePortEventKind::DocumentMutationCommittedWithoutFrame {
                    navigation,
                    operation,
                    previous_live_version,
                    live_version,
                    frame_version: next_projected_version(frame_version),
                    result,
                    created_nodes,
                    failure,
                },
                (
                    mismatch @ (MutationEventVersionMismatch::RejectedLiveMissing
                    | MutationEventVersionMismatch::RejectedLiveSkip
                    | MutationEventVersionMismatch::RejectedLiveForeignDocument
                    | MutationEventVersionMismatch::RejectedLiveZeroDocument
                    | MutationEventVersionMismatch::RejectedFrameAhead),
                    EnginePortEventKind::DocumentMutationRejected {
                        navigation,
                        operation,
                        live_version,
                        frame_version,
                        failure,
                    },
                ) => {
                    let (live_version, frame_version) = match mismatch {
                        MutationEventVersionMismatch::RejectedLiveMissing => (None, frame_version),
                        MutationEventVersionMismatch::RejectedLiveSkip => {
                            (live_version.map(next_projected_version), frame_version)
                        }
                        MutationEventVersionMismatch::RejectedLiveForeignDocument => (
                            live_version.map(|version| {
                                EngineDocumentVersion::new(
                                    version.document().checked_add(10_000).unwrap(),
                                    version.revision(),
                                )
                            }),
                            frame_version,
                        ),
                        MutationEventVersionMismatch::RejectedLiveZeroDocument => (
                            live_version
                                .map(|version| EngineDocumentVersion::new(0, version.revision())),
                            frame_version,
                        ),
                        MutationEventVersionMismatch::RejectedFrameAhead => {
                            (live_version, frame_version.map(next_projected_version))
                        }
                        _ => unreachable!(),
                    };
                    EnginePortEventKind::DocumentMutationRejected {
                        navigation,
                        operation,
                        live_version,
                        frame_version,
                        failure,
                    }
                }
                (
                    mismatch @ (MutationEventVersionMismatch::RerenderLiveSkip
                    | MutationEventVersionMismatch::RerenderLiveForeignDocument
                    | MutationEventVersionMismatch::RerenderLiveZeroDocument
                    | MutationEventVersionMismatch::RerenderPreviousFrameAhead),
                    EnginePortEventKind::DocumentRerendered {
                        navigation,
                        operation,
                        live_version,
                        previous_frame_version,
                        frame,
                        descriptor,
                    },
                ) => {
                    let (live_version, previous_frame_version) = match mismatch {
                        MutationEventVersionMismatch::RerenderLiveSkip => {
                            (next_projected_version(live_version), previous_frame_version)
                        }
                        MutationEventVersionMismatch::RerenderLiveForeignDocument => (
                            EngineDocumentVersion::new(
                                live_version.document().checked_add(10_000).unwrap(),
                                live_version.revision(),
                            ),
                            previous_frame_version,
                        ),
                        MutationEventVersionMismatch::RerenderLiveZeroDocument => (
                            EngineDocumentVersion::new(0, live_version.revision()),
                            previous_frame_version,
                        ),
                        MutationEventVersionMismatch::RerenderPreviousFrameAhead => {
                            (live_version, next_projected_version(previous_frame_version))
                        }
                        _ => unreachable!(),
                    };
                    EnginePortEventKind::DocumentRerendered {
                        navigation,
                        operation,
                        live_version,
                        previous_frame_version,
                        frame,
                        descriptor,
                    }
                }
                (
                    mismatch @ (MutationEventVersionMismatch::RerenderRejectedLiveMissing
                    | MutationEventVersionMismatch::RerenderRejectedLiveSkip
                    | MutationEventVersionMismatch::RerenderRejectedLiveForeignDocument
                    | MutationEventVersionMismatch::RerenderRejectedLiveZeroDocument
                    | MutationEventVersionMismatch::RerenderRejectedFrameAhead),
                    EnginePortEventKind::DocumentRerenderRejected {
                        navigation,
                        operation,
                        live_version,
                        frame_version,
                        failure,
                    },
                ) => {
                    let (live_version, frame_version) = match mismatch {
                        MutationEventVersionMismatch::RerenderRejectedLiveMissing => {
                            (None, frame_version)
                        }
                        MutationEventVersionMismatch::RerenderRejectedLiveSkip => {
                            (live_version.map(next_projected_version), frame_version)
                        }
                        MutationEventVersionMismatch::RerenderRejectedLiveForeignDocument => (
                            live_version.map(|version| {
                                EngineDocumentVersion::new(
                                    version.document().checked_add(10_000).unwrap(),
                                    version.revision(),
                                )
                            }),
                            frame_version,
                        ),
                        MutationEventVersionMismatch::RerenderRejectedLiveZeroDocument => (
                            live_version
                                .map(|version| EngineDocumentVersion::new(0, version.revision())),
                            frame_version,
                        ),
                        MutationEventVersionMismatch::RerenderRejectedFrameAhead => {
                            (live_version, frame_version.map(next_projected_version))
                        }
                        _ => unreachable!(),
                    };
                    EnginePortEventKind::DocumentRerenderRejected {
                        navigation,
                        operation,
                        live_version,
                        frame_version,
                        failure,
                    }
                }
                _ => return Ok(Some(event)),
            };
            self.event_mismatch = None;
            Ok(Some(EnginePortEvent::new(event.sequence(), kind)))
        }

        fn take_frame(
            &mut self,
            navigation: NavigationId,
            lease: EnginePortFrameLeaseId,
        ) -> Result<EngineFrameLease, EnginePortError> {
            match self.frame_take.take() {
                Some(LeaseTakeBehavior::Stale) => {
                    let bound = self.inner.frames.get(&lease).copied().unwrap();
                    let _ = self
                        .inner
                        .receiver
                        .as_mut()
                        .unwrap()
                        .take_frame(bound.lease)
                        .unwrap();
                    return self.inner.take_frame(navigation, lease);
                }
                Some(LeaseTakeBehavior::Unknown) => {
                    return Err(EnginePortError::FrameLease(FrameLeaseError::Unknown));
                }
                Some(LeaseTakeBehavior::Panic) => panic!("injected frame transfer panic"),
                None => {}
            }
            self.inner.take_frame(navigation, lease)
        }

        fn take_mutation_result(
            &mut self,
            navigation: NavigationId,
            lease: EnginePortMutationLeaseId,
        ) -> Result<EngineMutationResultLease, EnginePortError> {
            match self.result_take.take() {
                Some(LeaseTakeBehavior::Stale) => {
                    let bound = self.inner.mutations.get(&lease).copied().unwrap();
                    let _ = self
                        .inner
                        .receiver
                        .as_mut()
                        .unwrap()
                        .take_mutation_result(bound.lease)
                        .unwrap();
                    return self.inner.take_mutation_result(navigation, lease);
                }
                Some(LeaseTakeBehavior::Unknown) => {
                    return Err(EnginePortError::MutationLease(
                        MutationResultLeaseError::Unknown,
                    ));
                }
                Some(LeaseTakeBehavior::Panic) => {
                    panic!("injected mutation-result transfer panic")
                }
                None => {}
            }
            let mut result = self.inner.take_mutation_result(navigation, lease)?;
            match self.result_mismatch.take() {
                Some(MutationResultMismatch::Navigation) => {
                    result.navigation = NavigationId::new(
                        TopLevelContextId::new(4_000).unwrap(),
                        navigation.generation(),
                    );
                }
                Some(MutationResultMismatch::Lease) => {
                    result.lease = EnginePortMutationLeaseId::new(
                        lease.get().checked_add(1).expect("test lease has room"),
                    )
                    .unwrap();
                }
                Some(MutationResultMismatch::Operation) => {
                    result.operation = self.foreign_operation;
                }
                Some(MutationResultMismatch::LiveVersion) => {
                    result.live_version = EngineDocumentVersion::new(
                        result.live_version.document(),
                        result
                            .live_version
                            .revision()
                            .checked_add(1)
                            .expect("test revision has room"),
                    );
                }
                Some(MutationResultMismatch::CreatedNodes) => {
                    result.backing = MutationBacking::Owned(
                        result
                            .created_nodes()
                            .checked_add(1)
                            .expect("test node count has room"),
                    );
                }
                None => {}
            }
            Ok(result)
        }

        fn shutdown(&mut self) -> EnginePortShutdownStatus {
            self.inner.shutdown()
        }
    }

    #[derive(Clone, Copy, Debug)]
    enum MutationPublicationPath {
        Rendered,
        RenderedFrameSuppressed,
        CommittedWithoutFrame,
        Rejected,
        Rerendered,
        RerenderRejected,
    }

    fn foreign_operation() -> DocumentOperationId {
        let mut port = NavigationEnginePort::spawn_with_executor(limits(), || {
            Ok(DocumentExecutor { document: None })
        })
        .unwrap();
        let navigation = port
            .navigate(
                TopLevelContextId::new(1).unwrap(),
                NavigationRequest::new("https://foreign-operation.invalid/").unwrap(),
            )
            .unwrap();
        let lease = mapped_initial_frame(&mut port, navigation);
        let frame = port.take_frame(navigation, lease).unwrap();
        let batch = ScriptMutationBatch::new(
            native_document_version(&frame),
            vec![ScriptMutationCommand::CreateText {
                token: CreatedNodeToken::from_index(0),
                data: "foreign".into(),
            }],
        );
        let receipt = port
            .engine
            .as_ref()
            .unwrap()
            .mutate_document(navigation, batch)
            .unwrap();
        let CommandReceipt::DocumentMutationQueued { operation, .. } = receipt else {
            panic!("expected mutation admission");
        };
        let _ = port.shutdown();
        operation
    }

    fn hostile_port(
        path: MutationPublicationPath,
        mismatch: MutationResultMismatch,
        foreign_operation: DocumentOperationId,
    ) -> HostileMutationResultPort {
        let inner = match path {
            MutationPublicationPath::Rendered
            | MutationPublicationPath::RenderedFrameSuppressed => {
                NavigationEnginePort::spawn_with_executor(limits(), || {
                    Ok(DocumentExecutor { document: None })
                })
                .unwrap()
            }
            MutationPublicationPath::CommittedWithoutFrame => {
                NavigationEnginePort::spawn_with_executor(limits(), || {
                    Ok(NoFrameDocumentExecutor { document: None })
                })
                .unwrap()
            }
            MutationPublicationPath::Rejected => {
                NavigationEnginePort::spawn_with_executor(limits(), || {
                    Ok(RejectingDocumentExecutor { document: None })
                })
                .unwrap()
            }
            MutationPublicationPath::Rerendered => {
                NavigationEnginePort::spawn_with_executor(limits(), || {
                    Ok(RerenderDocumentExecutor {
                        document: None,
                        reject: false,
                    })
                })
                .unwrap()
            }
            MutationPublicationPath::RerenderRejected => {
                NavigationEnginePort::spawn_with_executor(limits(), || {
                    Ok(RerenderDocumentExecutor {
                        document: None,
                        reject: true,
                    })
                })
                .unwrap()
            }
        };
        HostileMutationResultPort {
            inner,
            result_mismatch: Some(mismatch),
            event_mismatch: None,
            frame_take: None,
            result_take: None,
            foreign_operation,
        }
    }

    fn hostile_event_port(
        path: MutationPublicationPath,
        mismatch: MutationEventVersionMismatch,
        foreign_operation: DocumentOperationId,
    ) -> HostileMutationResultPort {
        let mut port = hostile_port(path, MutationResultMismatch::Navigation, foreign_operation);
        port.result_mismatch = None;
        port.event_mismatch = Some(mismatch);
        port
    }

    fn lease_behavior_port(
        path: MutationPublicationPath,
        foreign_operation: DocumentOperationId,
    ) -> HostileMutationResultPort {
        let mut port = hostile_port(path, MutationResultMismatch::Navigation, foreign_operation);
        port.result_mismatch = None;
        port
    }

    fn wait_for_initial_session_frame<E: EnginePort>(
        session: &mut crate::BrowserSession<E>,
        tab: crate::BrowserTabId,
    ) {
        for _ in 0..100_000 {
            match session.poll_engine_once() {
                Ok(_) if session.frame(tab).unwrap().is_some() => return,
                Ok(crate::EnginePumpOutcome::Empty) => std::thread::yield_now(),
                Ok(_) => {}
                Err(error) => panic!("initial document failed: {error:?}"),
            }
        }
        panic!("initial document frame did not arrive");
    }

    fn wait_for_session_document_navigation<E: EnginePort>(
        session: &mut crate::BrowserSession<E>,
        tab: crate::BrowserTabId,
        navigation: NavigationId,
    ) {
        for _ in 0..100_000 {
            if session
                .tab_snapshot(tab)
                .unwrap()
                .engine_document_navigation
                == Some(navigation)
            {
                return;
            }
            match session.poll_engine_once() {
                Ok(crate::EnginePumpOutcome::Empty) => std::thread::yield_now(),
                Ok(_) => {}
                Err(error) => panic!("document navigation failed: {error:?}"),
            }
        }
        panic!("document version state did not reach {navigation:?}");
    }

    fn queued_navigation(outcome: crate::BrowserCommandOutcome) -> NavigationId {
        match outcome {
            crate::BrowserCommandOutcome::NavigationQueued { navigation, .. } => navigation,
            other => panic!("expected queued navigation, got {other:?}"),
        }
    }

    #[test]
    fn real_queued_b_promotes_before_queued_c_failure_without_losing_live_state() {
        let inner = NavigationEnginePort::spawn_with_executor(limits(), || {
            Ok(FailThirdDocumentExecutor { document: None })
        })
        .unwrap();
        let port = BufferedRawPort::new(inner);
        let session_limits =
            crate::SessionLimits::new(2, 4, 8, 4, 8, 4_096, 4_096, 4_096, 8).unwrap();
        let mut session = crate::BrowserSession::new(port, session_limits).unwrap();
        let tab = crate::BrowserTabId::new(1).unwrap();

        let navigation_a = queued_navigation(
            session
                .navigate_new(tab, "https://a-queued.invalid/")
                .unwrap(),
        );
        wait_for_initial_session_frame(&mut session, tab);
        assert_eq!(
            session.tab_snapshot(tab).unwrap().live_navigation,
            Some(navigation_a)
        );

        let navigation_b = queued_navigation(
            session
                .navigate_new(tab, "https://b-queued.invalid/")
                .unwrap(),
        );
        session.engine_mut_for_tests().buffer_raw(3);
        let navigation_c = queued_navigation(
            session
                .navigate_new(tab, "https://c-fails.invalid/")
                .unwrap(),
        );
        session.engine_mut_for_tests().buffer_raw(2);

        assert_eq!(
            session.poll_engine_once().unwrap(),
            crate::EnginePumpOutcome::Applied
        );
        assert_eq!(
            session.poll_engine_once().unwrap(),
            crate::EnginePumpOutcome::Applied
        );
        assert_eq!(
            session.poll_engine_once().unwrap(),
            crate::EnginePumpOutcome::Applied
        );
        let b = session.tab_snapshot(tab).unwrap();
        assert_eq!(b.live_navigation, Some(navigation_b));
        assert_eq!(b.latest_navigation, Some(navigation_c));
        assert!(b.loading);
        assert_eq!(
            b.latest_navigation_phase,
            Some(crate::NavigationPhase::Requested)
        );
        let b_frame = b.frame;
        let b_live = b.engine_live_version;
        let b_frame_version = b.engine_frame_version;
        let b_result = b.mutation_result;
        assert!(b_frame.is_some());
        assert!(b_live.is_some());

        assert_eq!(
            session.poll_engine_once().unwrap(),
            crate::EnginePumpOutcome::Applied
        );
        assert_eq!(
            session.tab_snapshot(tab).unwrap().latest_navigation_phase,
            Some(crate::NavigationPhase::Started)
        );
        assert_eq!(
            session.poll_engine_once().unwrap(),
            crate::EnginePumpOutcome::Applied
        );
        let after_failure = session.tab_snapshot(tab).unwrap();
        assert_eq!(after_failure.live_navigation, Some(navigation_b));
        assert_eq!(after_failure.latest_navigation, Some(navigation_c));
        assert_eq!(
            after_failure.latest_navigation_phase,
            Some(crate::NavigationPhase::Failed)
        );
        assert!(!after_failure.loading);
        assert_eq!(after_failure.frame, b_frame);
        assert_eq!(after_failure.engine_live_version, b_live);
        assert_eq!(after_failure.engine_frame_version, b_frame_version);
        assert_eq!(after_failure.mutation_result, b_result);
        assert_eq!(
            session.frame(tab).unwrap().unwrap().navigation(),
            navigation_b
        );
        let _ = session.shutdown();
    }

    #[test]
    fn queued_old_live_document_event_routes_during_pending_failed_replacement() {
        let inner = NavigationEnginePort::spawn_with_executor(limits(), || {
            Ok(FailReplacementDocumentExecutor { document: None })
        })
        .unwrap();
        let port = BufferedRawPort::new(inner);
        let session_limits =
            crate::SessionLimits::new(2, 4, 8, 4, 8, 4_096, 4_096, 4_096, 8).unwrap();
        let mut session = crate::BrowserSession::new(port, session_limits).unwrap();
        let tab = crate::BrowserTabId::new(1).unwrap();
        let live_navigation = queued_navigation(
            session
                .navigate_new(tab, "https://live-before-replacement.invalid/")
                .unwrap(),
        );
        wait_for_initial_session_frame(&mut session, tab);
        let version = native_document_version(session.frame(tab).unwrap().unwrap());
        let batch = ScriptMutationBatch::new(
            version,
            vec![ScriptMutationCommand::CreateText {
                token: CreatedNodeToken::from_index(0),
                data: "queued before replacement".into(),
            }],
        );
        assert!(matches!(
            session
                .engine_mut_for_tests()
                .inner
                .engine
                .as_ref()
                .unwrap()
                .mutate_document(live_navigation, batch)
                .unwrap(),
            CommandReceipt::DocumentMutationQueued { .. }
        ));
        session.engine_mut_for_tests().buffer_raw(1);
        let replacement = queued_navigation(
            session
                .navigate_new(tab, "https://replacement-fails.invalid/")
                .unwrap(),
        );
        session.engine_mut_for_tests().buffer_raw(2);

        assert_eq!(
            session.poll_engine_once().unwrap(),
            crate::EnginePumpOutcome::Applied
        );
        let mutated = session.tab_snapshot(tab).unwrap();
        assert_eq!(mutated.live_navigation, Some(live_navigation));
        assert_eq!(mutated.latest_navigation, Some(replacement));
        assert!(mutated.loading);
        assert!(mutated.mutation_result.is_some());
        assert_eq!(
            mutated.engine_live_version.unwrap().revision(),
            version.revision() + 1
        );
        let retained_result = mutated.mutation_result;
        let retained_live = mutated.engine_live_version;
        let retained_frame = mutated.engine_frame_version;
        let retained_pixels = mutated.frame;

        assert_eq!(
            session.poll_engine_once().unwrap(),
            crate::EnginePumpOutcome::Applied
        );
        assert_eq!(
            session.poll_engine_once().unwrap(),
            crate::EnginePumpOutcome::Applied
        );
        let after_failure = session.tab_snapshot(tab).unwrap();
        assert_eq!(after_failure.live_navigation, Some(live_navigation));
        assert_eq!(after_failure.mutation_result, retained_result);
        assert_eq!(after_failure.engine_live_version, retained_live);
        assert_eq!(after_failure.engine_frame_version, retained_frame);
        assert_eq!(after_failure.frame, retained_pixels);
        assert!(!after_failure.loading);
        let _ = session.shutdown();
    }

    #[test]
    fn real_stale_initial_frame_uses_event_document_metadata_before_later_publication() {
        let inner = NavigationEnginePort::spawn_with_executor(limits(), || {
            Ok(DocumentExecutor { document: None })
        })
        .unwrap();
        let port = BufferedRawPort::new(inner);
        let session_limits =
            crate::SessionLimits::new(2, 4, 8, 4, 8, 4_096, 4_096, 4_096, 8).unwrap();
        let mut session = crate::BrowserSession::new(port, session_limits).unwrap();
        let tab = crate::BrowserTabId::new(1).unwrap();

        let navigation_b = queued_navigation(
            session
                .navigate_new(tab, "https://stale-b.invalid/")
                .unwrap(),
        );
        session.engine_mut_for_tests().buffer_raw(3);
        let b_version = match session
            .engine_mut_for_tests()
            .raw_events
            .get(2)
            .expect("buffered B frame")
            .kind()
        {
            EngineEventKind::FrameReady { metadata, .. } => metadata
                .document_version()
                .map(|version| project_document_version!(version))
                .expect("real frame carries a document version"),
            other => panic!("expected B frame event, got {other:?}"),
        };
        let navigation_c = queued_navigation(
            session
                .navigate_new(tab, "https://newer-c.invalid/")
                .unwrap(),
        );
        session.engine_mut_for_tests().buffer_raw(3);

        assert_eq!(
            session.poll_engine_once().unwrap(),
            crate::EnginePumpOutcome::Applied
        );
        assert_eq!(
            session.poll_engine_once().unwrap(),
            crate::EnginePumpOutcome::Applied
        );
        assert_eq!(
            session.poll_engine_once().unwrap(),
            crate::EnginePumpOutcome::StaleSuppressed
        );
        let stale_b = session.tab_snapshot(tab).unwrap();
        assert_eq!(stale_b.live_navigation, Some(navigation_b));
        assert_eq!(stale_b.engine_live_version, Some(b_version));
        assert_eq!(stale_b.engine_frame_version, Some(b_version));
        assert_eq!(stale_b.latest_navigation, Some(navigation_c));
        assert!(stale_b.loading);
        assert!(session.frame(tab).unwrap().is_none());

        assert_eq!(
            session.poll_engine_once().unwrap(),
            crate::EnginePumpOutcome::Applied
        );
        assert_eq!(
            session.poll_engine_once().unwrap(),
            crate::EnginePumpOutcome::Applied
        );
        assert_eq!(
            session.poll_engine_once().unwrap(),
            crate::EnginePumpOutcome::Applied
        );
        assert_eq!(
            session.tab_snapshot(tab).unwrap().live_navigation,
            Some(navigation_c)
        );
        assert_eq!(
            session.frame(tab).unwrap().unwrap().navigation(),
            navigation_c
        );
        let _ = session.shutdown();
    }

    #[test]
    fn real_stale_rerender_advances_frame_semantics_while_suppressing_pixels() {
        let inner = NavigationEnginePort::spawn_with_executor(limits(), || {
            Ok(RerenderDocumentExecutor {
                document: None,
                reject: false,
            })
        })
        .unwrap();
        let port = BufferedRawPort::new(inner);
        let session_limits =
            crate::SessionLimits::new(2, 4, 8, 4, 8, 4_096, 4_096, 4_096, 8).unwrap();
        let mut session = crate::BrowserSession::new(port, session_limits).unwrap();
        let tab = crate::BrowserTabId::new(1).unwrap();
        let navigation = queued_navigation(
            session
                .navigate_new(tab, "https://stale-rerender.invalid/")
                .unwrap(),
        );
        wait_for_initial_session_frame(&mut session, tab);
        let version = native_document_version(session.frame(tab).unwrap().unwrap());
        for _ in 0..2 {
            assert!(matches!(
                session
                    .engine_mut_for_tests()
                    .inner
                    .engine
                    .as_ref()
                    .unwrap()
                    .rerender_document(navigation, version)
                    .unwrap(),
                CommandReceipt::DocumentRerenderQueued { .. }
            ));
            session.engine_mut_for_tests().buffer_raw(1);
        }

        assert_eq!(
            session.poll_engine_once().unwrap(),
            crate::EnginePumpOutcome::StaleSuppressed
        );
        let stale = session.tab_snapshot(tab).unwrap();
        let projected = EngineDocumentVersion::new(version.document_id().get(), version.revision());
        assert_eq!(stale.engine_live_version, Some(projected));
        assert_eq!(stale.engine_frame_version, Some(projected));
        assert!(session.frame(tab).unwrap().is_none());
        assert_eq!(session.retained_frame_bytes(), 0);

        assert_eq!(
            session.poll_engine_once().unwrap(),
            crate::EnginePumpOutcome::Applied
        );
        assert_eq!(
            session.frame(tab).unwrap().unwrap().pixels(),
            &[55, 66, 77, 255]
        );
        let _ = session.shutdown();
    }

    fn assert_hostile_mutation_is_terminal(
        port: HostileMutationResultPort,
        frame_limit: usize,
        label: impl fmt::Debug,
    ) {
        let session_limits =
            crate::SessionLimits::new(2, 4, 4, 4, 4, 4_096, frame_limit, 4_096, 8).unwrap();
        let mut session = crate::BrowserSession::new(port, session_limits).unwrap();
        let tab = crate::BrowserTabId::new(1).unwrap();
        let navigation = match session
            .navigate_new(tab, "https://hostile-result.invalid/")
            .unwrap()
        {
            crate::BrowserCommandOutcome::NavigationQueued { navigation, .. } => navigation,
            other => panic!("unexpected navigation outcome: {other:?}"),
        };
        wait_for_initial_session_frame(&mut session, tab);
        let version = native_document_version(session.frame(tab).unwrap().expect("initial frame"));
        let batch = ScriptMutationBatch::new(
            version,
            vec![ScriptMutationCommand::CreateText {
                token: CreatedNodeToken::from_index(0),
                data: "hostile".into(),
            }],
        );
        let receipt = session
            .engine_mut_for_tests()
            .inner
            .engine
            .as_ref()
            .unwrap()
            .mutate_document(navigation, batch)
            .unwrap();
        assert!(matches!(
            receipt,
            CommandReceipt::DocumentMutationQueued { .. }
        ));

        let terminal = 'poll: {
            for _ in 0..100_000 {
                match session.poll_engine_once() {
                    Err(error) => break 'poll error,
                    Ok(crate::EnginePumpOutcome::Empty) => std::thread::yield_now(),
                    Ok(_) => {}
                }
            }
            panic!("hostile {label:?} mutation was not rejected");
        };
        assert!(
            matches!(
                terminal,
                crate::SessionError::Terminal(crate::SessionFailure::EngineContract { .. })
            ),
            "unexpected {label:?} outcome: {terminal:?}",
        );
        assert_eq!(session.window_count(), 0);
        assert_eq!(session.tab_count(), 0);
        assert_eq!(session.closing_context_count(), 0);
        assert_eq!(session.retained_history_bytes(), 0);
        assert_eq!(session.retained_frame_bytes(), 0);
        assert!(matches!(
            session.lifecycle(),
            crate::SessionLifecycle::Failed {
                failure: crate::SessionFailure::EngineContract { .. },
                ..
            }
        ));
    }

    fn apply_suppressed_mutation(
        session: &mut crate::BrowserSession<NavigationEnginePort>,
        tab: crate::BrowserTabId,
        navigation: NavigationId,
        text: &str,
    ) -> DocumentOperationId {
        let version = native_document_version(session.frame(tab).unwrap().expect("current frame"));
        let batch = ScriptMutationBatch::new(
            version,
            vec![ScriptMutationCommand::CreateText {
                token: CreatedNodeToken::from_index(0),
                data: text.into(),
            }],
        );
        let receipt = session
            .engine_mut_for_tests()
            .engine
            .as_ref()
            .unwrap()
            .mutate_document(navigation, batch)
            .unwrap();
        let CommandReceipt::DocumentMutationQueued { operation, .. } = receipt else {
            panic!("expected mutation admission");
        };
        for _ in 0..100_000 {
            match session.poll_engine_once().unwrap() {
                crate::EnginePumpOutcome::MutationAppliedFrameSuppressed {
                    navigation: actual_navigation,
                    operation: actual_operation,
                } => {
                    assert_eq!(actual_navigation, navigation);
                    assert_eq!(actual_operation, operation);
                    return operation;
                }
                crate::EnginePumpOutcome::Empty => std::thread::yield_now(),
                _ => {}
            }
        }
        panic!("suppressed mutation did not commit");
    }

    fn live_hostile_session(
        port: HostileMutationResultPort,
        frame_limit: usize,
    ) -> (
        crate::BrowserSession<HostileMutationResultPort>,
        crate::BrowserTabId,
        NavigationId,
        DocumentVersion,
    ) {
        let session_limits =
            crate::SessionLimits::new(2, 4, 8, 4, 8, 4_096, frame_limit, 4_096, 8).unwrap();
        let mut session = crate::BrowserSession::new(port, session_limits).unwrap();
        let tab = crate::BrowserTabId::new(1).unwrap();
        let navigation = queued_navigation(
            session
                .navigate_new(tab, "https://lease-behavior.invalid/")
                .unwrap(),
        );
        wait_for_initial_session_frame(&mut session, tab);
        let version = native_document_version(session.frame(tab).unwrap().unwrap());
        (session, tab, navigation, version)
    }

    fn queue_hostile_mutation(
        session: &mut crate::BrowserSession<HostileMutationResultPort>,
        navigation: NavigationId,
        version: DocumentVersion,
    ) -> DocumentOperationId {
        let batch = ScriptMutationBatch::new(
            version,
            vec![ScriptMutationCommand::CreateText {
                token: CreatedNodeToken::from_index(0),
                data: "lease behavior".into(),
            }],
        );
        let receipt = session
            .engine_mut_for_tests()
            .inner
            .engine
            .as_ref()
            .unwrap()
            .mutate_document(navigation, batch)
            .unwrap();
        let CommandReceipt::DocumentMutationQueued { operation, .. } = receipt else {
            panic!("expected mutation admission");
        };
        operation
    }

    fn poll_until_nonempty<E: EnginePort>(
        session: &mut crate::BrowserSession<E>,
    ) -> Result<crate::EnginePumpOutcome, crate::SessionError> {
        for _ in 0..100_000 {
            match session.poll_engine_once() {
                Ok(crate::EnginePumpOutcome::Empty) => std::thread::yield_now(),
                outcome => return outcome,
            }
        }
        panic!("engine event did not arrive");
    }

    #[test]
    fn exact_stale_result_and_frame_combinations_preserve_atomic_document_semantics() {
        let foreign_operation = foreign_operation();

        let port = lease_behavior_port(MutationPublicationPath::Rendered, foreign_operation);
        let (mut session, tab, navigation, version) = live_hostile_session(port, 16);
        let before = session.tab_snapshot(tab).unwrap();
        session.engine_mut_for_tests().result_take = Some(LeaseTakeBehavior::Stale);
        let _ = queue_hostile_mutation(&mut session, navigation, version);
        assert_eq!(
            poll_until_nonempty(&mut session).unwrap(),
            crate::EnginePumpOutcome::StaleSuppressed
        );
        let after = session.tab_snapshot(tab).unwrap();
        assert_eq!(after.engine_live_version, before.engine_live_version);
        assert_eq!(after.engine_frame_version, before.engine_frame_version);
        assert_eq!(after.frame, before.frame);
        assert_eq!(after.mutation_result, before.mutation_result);
        assert!(session.engine_mut_for_tests().inner.frames.is_empty());
        let _ = session.shutdown();

        let port = lease_behavior_port(MutationPublicationPath::Rendered, foreign_operation);
        let (mut session, tab, navigation, version) = live_hostile_session(port, 16);
        session.engine_mut_for_tests().frame_take = Some(LeaseTakeBehavior::Stale);
        let operation = queue_hostile_mutation(&mut session, navigation, version);
        assert_eq!(
            poll_until_nonempty(&mut session).unwrap(),
            crate::EnginePumpOutcome::StaleSuppressed
        );
        let after = session.tab_snapshot(tab).unwrap();
        assert_eq!(
            after.engine_live_version.unwrap().revision(),
            version.revision() + 1
        );
        assert_eq!(after.engine_live_version, after.engine_frame_version);
        assert!(after.mutation_result.is_some());
        assert!(session.frame(tab).unwrap().is_none());
        assert_eq!(session.retained_frame_bytes(), 0);
        assert_eq!(
            session
                .take_mutation_result(tab)
                .unwrap()
                .unwrap()
                .operation(),
            operation
        );
        let _ = session.shutdown();

        let port = lease_behavior_port(
            MutationPublicationPath::CommittedWithoutFrame,
            foreign_operation,
        );
        let (mut session, tab, navigation, version) = live_hostile_session(port, 16);
        let before = session.tab_snapshot(tab).unwrap();
        session.engine_mut_for_tests().result_take = Some(LeaseTakeBehavior::Stale);
        let _ = queue_hostile_mutation(&mut session, navigation, version);
        assert_eq!(
            poll_until_nonempty(&mut session).unwrap(),
            crate::EnginePumpOutcome::StaleSuppressed
        );
        let after = session.tab_snapshot(tab).unwrap();
        assert_eq!(after.engine_live_version, before.engine_live_version);
        assert_eq!(after.engine_frame_version, before.engine_frame_version);
        assert_eq!(after.frame, before.frame);
        assert_eq!(after.mutation_result, before.mutation_result);
        let _ = session.shutdown();
    }

    #[test]
    fn unexpected_composite_take_or_discard_faults_and_panics_are_terminal() {
        let foreign_operation = foreign_operation();
        for behavior in [LeaseTakeBehavior::Unknown, LeaseTakeBehavior::Panic] {
            let port = lease_behavior_port(MutationPublicationPath::Rendered, foreign_operation);
            let (mut session, _tab, navigation, version) = live_hostile_session(port, 16);
            session.engine_mut_for_tests().result_take = Some(behavior);
            let _ = queue_hostile_mutation(&mut session, navigation, version);
            assert!(poll_until_nonempty(&mut session).is_err());
            assert_eq!(session.window_count(), 0);
            assert_eq!(session.tab_count(), 0);

            let port = lease_behavior_port(MutationPublicationPath::Rendered, foreign_operation);
            let (mut session, _tab, navigation, version) = live_hostile_session(port, 16);
            session.engine_mut_for_tests().frame_take = Some(behavior);
            let _ = queue_hostile_mutation(&mut session, navigation, version);
            assert!(poll_until_nonempty(&mut session).is_err());
            assert_eq!(session.window_count(), 0);
            assert_eq!(session.tab_count(), 0);

            let port = lease_behavior_port(MutationPublicationPath::Rendered, foreign_operation);
            let (mut session, _tab, navigation, version) = live_hostile_session(port, 16);
            session.engine_mut_for_tests().result_take = Some(LeaseTakeBehavior::Stale);
            session.engine_mut_for_tests().frame_take = Some(behavior);
            let _ = queue_hostile_mutation(&mut session, navigation, version);
            assert!(poll_until_nonempty(&mut session).is_err());
            assert_eq!(session.window_count(), 0);
            assert_eq!(session.tab_count(), 0);
        }
    }

    #[test]
    fn composite_publication_preflights_atomically_and_stale_faults_retire_exact_bindings() {
        let mut port = NavigationEnginePort::spawn_with_executor(limits(), || {
            Ok(DocumentExecutor { document: None })
        })
        .unwrap();
        let navigation = port
            .navigate(
                TopLevelContextId::new(1).unwrap(),
                NavigationRequest::new("https://document.invalid/").unwrap(),
            )
            .unwrap();
        assert!(matches!(
            next_raw(&mut port).kind(),
            EngineEventKind::NavigationStarted { .. }
        ));
        assert!(matches!(
            next_raw(&mut port).kind(),
            EngineEventKind::NavigationCommitted { .. }
        ));
        let initial = next_raw(&mut port);
        let EngineEventKind::FrameReady { lease, .. } = initial.kind() else {
            panic!("expected initial frame");
        };
        let version = port
            .receiver
            .as_mut()
            .unwrap()
            .take_frame(lease)
            .unwrap()
            .document_version()
            .unwrap();
        let batch = ScriptMutationBatch::new(
            version,
            vec![ScriptMutationCommand::CreateText {
                token: CreatedNodeToken::from_index(0),
                data: "atomic".into(),
            }],
        );
        port.engine
            .as_ref()
            .unwrap()
            .mutate_document(navigation, batch)
            .unwrap();
        let raw = next_raw(&mut port);
        let EngineEventKind::DocumentMutationRendered {
            result: _, frame, ..
        } = raw.kind()
        else {
            panic!("expected rendered mutation");
        };

        // Make only the frame half fail preflight. The fresh result half must
        // remain entirely uncommitted.
        port.last_frame_lease = Some(frame.get());
        let mutation_count = port.mutations.len();
        let last_mutation = port.last_mutation_lease;
        assert!(matches!(
            port.map_event(raw),
            Err(EnginePortError::ContractViolation(_))
        ));
        assert_eq!(port.mutations.len(), mutation_count);
        assert_eq!(port.last_mutation_lease, last_mutation);

        // Retry after removing the injected hostile high-water mark, then
        // steal both native leases to force transfer errors. Neither adapter
        // binding is removed before the receiver reports success.
        port.last_frame_lease = None;
        let mapped = port.map_event(raw).unwrap();
        let EnginePortEventKind::DocumentMutationRendered {
            result: mapped_result,
            frame: mapped_frame,
            ..
        } = mapped.kind()
        else {
            panic!("expected mapped rendered mutation");
        };
        let native_result = port.mutations.get(&mapped_result).copied().unwrap();
        let native_frame = port.frames.get(&mapped_frame).copied().unwrap();
        let retired_result = port.mutations.remove(&mapped_result).unwrap();
        assert!(matches!(
            port.preflight_mutation(navigation, retired_result.lease),
            Err(EnginePortError::ContractViolation(_))
        ));
        port.mutations.insert(mapped_result, retired_result);
        port.receiver
            .as_mut()
            .unwrap()
            .take_mutation_result(native_result.lease)
            .unwrap();
        port.receiver
            .as_mut()
            .unwrap()
            .take_frame(native_frame.lease)
            .unwrap();
        assert!(matches!(
            port.take_mutation_result(navigation, mapped_result),
            Err(EnginePortError::MutationLease(
                MutationResultLeaseError::Stale,
            ))
        ));
        assert!(matches!(
            port.take_frame(navigation, mapped_frame),
            Err(EnginePortError::FrameLease(FrameLeaseError::Stale))
        ));
        assert!(!port.mutations.contains_key(&mapped_result));
        assert!(!port.frames.contains_key(&mapped_frame));
        let _ = port.shutdown();
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn newer_navigation_retains_old_bindings_until_valid_frame_publication() {
        let mut port = NavigationEnginePort::spawn_with_executor(limits(), || {
            Ok(DocumentExecutor { document: None })
        })
        .unwrap();
        let context = TopLevelContextId::new(1).unwrap();
        let old_navigation = port
            .navigate(
                context,
                NavigationRequest::new("https://old-document.invalid/").unwrap(),
            )
            .unwrap();
        for expected in ["started", "committed"] {
            let raw = next_raw(&mut port);
            let mapped = port.map_event(raw).unwrap();
            assert!(
                matches!(
                    (expected, mapped.kind()),
                    ("started", EnginePortEventKind::NavigationStarted { .. })
                        | ("committed", EnginePortEventKind::NavigationCommitted { .. })
                ),
                "expected {expected} event",
            );
        }
        let raw = next_raw(&mut port);
        let mapped = port.map_event(raw).unwrap();
        let EnginePortEventKind::FrameReady {
            lease: initial_frame,
            ..
        } = mapped.kind()
        else {
            panic!("expected initial frame");
        };
        let initial_frame = port.take_frame(old_navigation, initial_frame).unwrap();
        let version = native_document_version(&initial_frame);
        let batch = ScriptMutationBatch::new(
            version,
            vec![ScriptMutationCommand::CreateText {
                token: CreatedNodeToken::from_index(0),
                data: "old mapping".into(),
            }],
        );
        port.engine
            .as_ref()
            .unwrap()
            .mutate_document(old_navigation, batch)
            .unwrap();
        let raw = next_raw(&mut port);
        let mapped = port.map_event(raw).unwrap();
        let EnginePortEventKind::DocumentMutationRendered {
            result: old_result,
            frame: old_frame,
            ..
        } = mapped.kind()
        else {
            panic!("expected old rendered mutation");
        };
        let old_result_bound = port.mutations.get(&old_result).copied().unwrap();

        // A same-generation start is hostile. Preflight rejects it without
        // partially changing either exact-binding registry.
        assert!(matches!(
            port.retire_before_navigation_start(old_navigation),
            Err(EnginePortError::ContractViolation(_))
        ));
        assert!(port.frames.contains_key(&old_frame));
        assert!(port.mutations.contains_key(&old_result));

        // Model the hard-limit state which delayed transfers can legally
        // create across many old document operations. Every binding is for
        // the generation which the next admitted navigation makes stale.
        let mut candidate = 1_u64;
        while port.mutations.len() < MAX_PENDING_MUTATION_BINDINGS {
            let port_lease = EnginePortMutationLeaseId::new(candidate).unwrap();
            port.mutations.entry(port_lease).or_insert(old_result_bound);
            candidate = candidate.checked_add(1).unwrap();
        }

        let new_navigation = port
            .navigate(
                context,
                NavigationRequest::new("https://new-document.invalid/").unwrap(),
            )
            .unwrap();
        let raw = next_raw(&mut port);
        let mapped = port.map_event(raw).unwrap();
        assert!(matches!(
            mapped.kind(),
            EnginePortEventKind::NavigationStarted {
                navigation
            } if navigation == new_navigation
        ));
        assert!(port.frames.contains_key(&old_frame));
        assert!(port.mutations.contains_key(&old_result));
        assert_eq!(port.mutations.len(), MAX_PENDING_MUTATION_BINDINGS);

        let raw = next_raw(&mut port);
        assert!(matches!(
            port.map_event(raw).unwrap().kind(),
            EnginePortEventKind::NavigationCommitted {
                navigation,
                ..
            } if navigation == new_navigation
        ));
        assert!(port.frames.contains_key(&old_frame));
        assert!(port.mutations.contains_key(&old_result));
        let raw = next_raw(&mut port);
        let mapped = port.map_event(raw).unwrap();
        let EnginePortEventKind::FrameReady {
            lease: new_initial_frame,
            ..
        } = mapped.kind()
        else {
            panic!("expected new initial frame");
        };
        assert!(!port.frames.contains_key(&old_frame));
        assert!(!port.mutations.contains_key(&old_result));
        assert_eq!(port.frames.len(), 1);
        assert!(port.mutations.is_empty());

        // Only the successful new frame publication retires old bindings.
        // Looking up the retired old token cannot consume or remove the
        // differently keyed lease for the new generation.
        assert!(matches!(
            port.take_frame(old_navigation, old_frame),
            Err(EnginePortError::FrameLease(FrameLeaseError::Unknown))
        ));
        assert!(port.frames.contains_key(&new_initial_frame));
        let new_initial_frame = port.take_frame(new_navigation, new_initial_frame).unwrap();
        let version = native_document_version(&new_initial_frame);

        let batch = ScriptMutationBatch::new(
            version,
            vec![ScriptMutationCommand::CreateText {
                token: CreatedNodeToken::from_index(0),
                data: "new mapping".into(),
            }],
        );
        port.engine
            .as_ref()
            .unwrap()
            .mutate_document(new_navigation, batch)
            .unwrap();
        let raw = next_raw(&mut port);
        let mapped = port
            .map_event(raw)
            .expect("old saturated registry was retired before valid publication");
        let EnginePortEventKind::DocumentMutationRendered {
            result: new_result,
            frame: new_frame,
            ..
        } = mapped.kind()
        else {
            panic!("expected new rendered mutation");
        };
        assert!(matches!(
            port.take_mutation_result(old_navigation, old_result),
            Err(EnginePortError::MutationLease(
                MutationResultLeaseError::Unknown,
            ))
        ));
        assert!(port.mutations.contains_key(&new_result));
        assert!(port.frames.contains_key(&new_frame));
        assert_eq!(
            port.take_mutation_result(new_navigation, new_result)
                .unwrap()
                .navigation(),
            new_navigation
        );
        assert_eq!(
            port.take_frame(new_navigation, new_frame)
                .unwrap()
                .navigation(),
            new_navigation
        );
        let _ = port.shutdown();
    }

    #[test]
    fn high_watermarks_reject_reuse_after_active_binding_retirement() {
        let mut port =
            NavigationEnginePort::spawn_with_executor(limits(), || Ok(PixelExecutor)).unwrap();
        let context = TopLevelContextId::new(1).unwrap();
        let navigation = port
            .navigate(
                context,
                NavigationRequest::new("https://first.invalid/").unwrap(),
            )
            .unwrap();
        let lease = mapped_initial_frame(&mut port, navigation);
        let bound = port.frames.get(&lease).copied().unwrap();
        let _transferred = port.take_frame(navigation, lease).unwrap();
        assert!(port.frames.is_empty());
        assert!(matches!(
            port.preflight_frame(navigation, bound.lease),
            Err(EnginePortError::ContractViolation(_))
        ));
        assert_eq!(port.last_frame_lease, Some(bound.lease.get()));
        assert_eq!(navigation.generation(), NavigationGeneration::INITIAL);
        let _ = port.shutdown();
    }

    #[test]
    fn every_mutation_result_field_is_exact_on_every_publication_path() {
        let foreign_operation = foreign_operation();
        let paths = [
            MutationPublicationPath::Rendered,
            MutationPublicationPath::RenderedFrameSuppressed,
            MutationPublicationPath::CommittedWithoutFrame,
        ];
        let mismatches = [
            MutationResultMismatch::Navigation,
            MutationResultMismatch::Lease,
            MutationResultMismatch::Operation,
            MutationResultMismatch::LiveVersion,
            MutationResultMismatch::CreatedNodes,
        ];

        for path in paths {
            for mismatch in mismatches {
                let port = hostile_port(path, mismatch, foreign_operation);
                let frame_limit = match path {
                    MutationPublicationPath::RenderedFrameSuppressed
                    | MutationPublicationPath::CommittedWithoutFrame => 4,
                    MutationPublicationPath::Rendered
                    | MutationPublicationPath::Rejected
                    | MutationPublicationPath::Rerendered
                    | MutationPublicationPath::RerenderRejected => 16,
                };
                assert_hostile_mutation_is_terminal(port, frame_limit, (path, mismatch));
            }
        }
    }

    #[test]
    fn mutation_events_must_continue_exact_live_and_frame_versions() {
        let foreign_operation = foreign_operation();
        let cases = [
            (
                MutationPublicationPath::Rendered,
                MutationEventVersionMismatch::RenderedPreviousLive,
            ),
            (
                MutationPublicationPath::Rendered,
                MutationEventVersionMismatch::RenderedPreviousFrame,
            ),
            (
                MutationPublicationPath::Rendered,
                MutationEventVersionMismatch::RenderedLive,
            ),
            (
                MutationPublicationPath::Rendered,
                MutationEventVersionMismatch::RenderedLiveRollback,
            ),
            (
                MutationPublicationPath::Rendered,
                MutationEventVersionMismatch::RenderedLiveForeignDocument,
            ),
            (
                MutationPublicationPath::Rendered,
                MutationEventVersionMismatch::RenderedLiveZeroDocument,
            ),
            (
                MutationPublicationPath::RenderedFrameSuppressed,
                MutationEventVersionMismatch::RenderedLive,
            ),
            (
                MutationPublicationPath::RenderedFrameSuppressed,
                MutationEventVersionMismatch::RenderedLiveRollback,
            ),
            (
                MutationPublicationPath::RenderedFrameSuppressed,
                MutationEventVersionMismatch::RenderedLiveForeignDocument,
            ),
            (
                MutationPublicationPath::RenderedFrameSuppressed,
                MutationEventVersionMismatch::RenderedLiveZeroDocument,
            ),
            (
                MutationPublicationPath::CommittedWithoutFrame,
                MutationEventVersionMismatch::CommittedPreviousLive,
            ),
            (
                MutationPublicationPath::CommittedWithoutFrame,
                MutationEventVersionMismatch::CommittedLive,
            ),
            (
                MutationPublicationPath::CommittedWithoutFrame,
                MutationEventVersionMismatch::CommittedLiveRollback,
            ),
            (
                MutationPublicationPath::CommittedWithoutFrame,
                MutationEventVersionMismatch::CommittedLiveForeignDocument,
            ),
            (
                MutationPublicationPath::CommittedWithoutFrame,
                MutationEventVersionMismatch::CommittedLiveZeroDocument,
            ),
            (
                MutationPublicationPath::CommittedWithoutFrame,
                MutationEventVersionMismatch::CommittedFrame,
            ),
            (
                MutationPublicationPath::Rejected,
                MutationEventVersionMismatch::RejectedLiveMissing,
            ),
            (
                MutationPublicationPath::Rejected,
                MutationEventVersionMismatch::RejectedLiveSkip,
            ),
            (
                MutationPublicationPath::Rejected,
                MutationEventVersionMismatch::RejectedLiveForeignDocument,
            ),
            (
                MutationPublicationPath::Rejected,
                MutationEventVersionMismatch::RejectedLiveZeroDocument,
            ),
            (
                MutationPublicationPath::Rejected,
                MutationEventVersionMismatch::RejectedFrameAhead,
            ),
        ];

        for (path, mismatch) in cases {
            let port = hostile_event_port(path, mismatch, foreign_operation);
            let frame_limit = match path {
                MutationPublicationPath::RenderedFrameSuppressed
                | MutationPublicationPath::CommittedWithoutFrame => 4,
                MutationPublicationPath::Rendered
                | MutationPublicationPath::Rejected
                | MutationPublicationPath::Rerendered
                | MutationPublicationPath::RerenderRejected => 16,
            };
            assert_hostile_mutation_is_terminal(port, frame_limit, (path, mismatch));
        }
    }

    fn assert_hostile_rerender_is_terminal(
        path: MutationPublicationPath,
        mismatch: MutationEventVersionMismatch,
        foreign_operation: DocumentOperationId,
    ) {
        let port = hostile_event_port(path, mismatch, foreign_operation);
        let session_limits = crate::SessionLimits::new(2, 4, 8, 4, 8, 4_096, 16, 4_096, 8).unwrap();
        let mut session = crate::BrowserSession::new(port, session_limits).unwrap();
        let tab = crate::BrowserTabId::new(1).unwrap();
        let navigation = queued_navigation(
            session
                .navigate_new(tab, "https://hostile-rerender.invalid/")
                .unwrap(),
        );
        wait_for_initial_session_frame(&mut session, tab);
        let version = native_document_version(session.frame(tab).unwrap().unwrap());
        assert!(matches!(
            session
                .engine_mut_for_tests()
                .inner
                .engine
                .as_ref()
                .unwrap()
                .rerender_document(navigation, version)
                .unwrap(),
            CommandReceipt::DocumentRerenderQueued { .. }
        ));
        let terminal = poll_until_nonempty(&mut session).unwrap_err();
        assert!(matches!(
            terminal,
            crate::SessionError::Terminal(crate::SessionFailure::EngineContract { .. })
        ));
        assert_eq!(session.window_count(), 0);
        assert_eq!(session.tab_count(), 0);
    }

    #[test]
    fn rerender_events_require_exact_nonzero_same_document_versions() {
        let foreign_operation = foreign_operation();
        let cases = [
            (
                MutationPublicationPath::Rerendered,
                MutationEventVersionMismatch::RerenderLiveSkip,
            ),
            (
                MutationPublicationPath::Rerendered,
                MutationEventVersionMismatch::RerenderLiveForeignDocument,
            ),
            (
                MutationPublicationPath::Rerendered,
                MutationEventVersionMismatch::RerenderLiveZeroDocument,
            ),
            (
                MutationPublicationPath::Rerendered,
                MutationEventVersionMismatch::RerenderPreviousFrameAhead,
            ),
            (
                MutationPublicationPath::RerenderRejected,
                MutationEventVersionMismatch::RerenderRejectedLiveMissing,
            ),
            (
                MutationPublicationPath::RerenderRejected,
                MutationEventVersionMismatch::RerenderRejectedLiveSkip,
            ),
            (
                MutationPublicationPath::RerenderRejected,
                MutationEventVersionMismatch::RerenderRejectedLiveForeignDocument,
            ),
            (
                MutationPublicationPath::RerenderRejected,
                MutationEventVersionMismatch::RerenderRejectedLiveZeroDocument,
            ),
            (
                MutationPublicationPath::RerenderRejected,
                MutationEventVersionMismatch::RerenderRejectedFrameAhead,
            ),
        ];
        for (path, mismatch) in cases {
            assert_hostile_rerender_is_terminal(path, mismatch, foreign_operation);
        }
    }

    #[test]
    fn real_mutation_rejection_and_rerender_success_and_rejection_are_applied_exactly() {
        let port = NavigationEnginePort::spawn_with_executor(limits(), || {
            Ok(RejectingDocumentExecutor { document: None })
        })
        .unwrap();
        let session_limits = crate::SessionLimits::new(2, 4, 8, 4, 8, 4_096, 16, 4_096, 8).unwrap();
        let mut session = crate::BrowserSession::new(port, session_limits).unwrap();
        let tab = crate::BrowserTabId::new(1).unwrap();
        let navigation = queued_navigation(
            session
                .navigate_new(tab, "https://reject-mutation.invalid/")
                .unwrap(),
        );
        wait_for_initial_session_frame(&mut session, tab);
        let version = native_document_version(session.frame(tab).unwrap().unwrap());
        let before = session.tab_snapshot(tab).unwrap();
        let batch = ScriptMutationBatch::new(
            version,
            vec![ScriptMutationCommand::CreateText {
                token: CreatedNodeToken::from_index(0),
                data: "rejected".into(),
            }],
        );
        assert!(matches!(
            session
                .engine_mut_for_tests()
                .engine
                .as_ref()
                .unwrap()
                .mutate_document(navigation, batch)
                .unwrap(),
            CommandReceipt::DocumentMutationQueued { .. }
        ));
        assert_eq!(
            poll_until_nonempty(&mut session).unwrap(),
            crate::EnginePumpOutcome::Applied
        );
        let rejected = session.tab_snapshot(tab).unwrap();
        assert_eq!(rejected.engine_live_version, before.engine_live_version);
        assert_eq!(rejected.engine_frame_version, before.engine_frame_version);
        assert_eq!(rejected.frame, before.frame);
        assert_eq!(rejected.mutation_result, None);
        assert_eq!(
            rejected.last_document_failure,
            Some(DocumentOperationFailure::MutationRejected)
        );
        let _ = session.shutdown();

        for reject in [false, true] {
            let port = NavigationEnginePort::spawn_with_executor(limits(), move || {
                Ok(RerenderDocumentExecutor {
                    document: None,
                    reject,
                })
            })
            .unwrap();
            let mut session = crate::BrowserSession::new(port, session_limits).unwrap();
            let navigation = queued_navigation(
                session
                    .navigate_new(tab, "https://rerender-outcome.invalid/")
                    .unwrap(),
            );
            wait_for_initial_session_frame(&mut session, tab);
            let version = native_document_version(session.frame(tab).unwrap().unwrap());
            let before = session.tab_snapshot(tab).unwrap();
            assert!(matches!(
                session
                    .engine_mut_for_tests()
                    .engine
                    .as_ref()
                    .unwrap()
                    .rerender_document(navigation, version)
                    .unwrap(),
                CommandReceipt::DocumentRerenderQueued { .. }
            ));
            assert_eq!(
                poll_until_nonempty(&mut session).unwrap(),
                crate::EnginePumpOutcome::Applied
            );
            let after = session.tab_snapshot(tab).unwrap();
            assert_eq!(after.engine_live_version, before.engine_live_version);
            assert_eq!(after.engine_frame_version, before.engine_live_version);
            if reject {
                assert_eq!(after.frame, before.frame);
                assert_eq!(
                    after.last_document_failure,
                    Some(DocumentOperationFailure::Rendering)
                );
            } else {
                assert_ne!(after.frame, before.frame);
                assert_eq!(
                    session.frame(tab).unwrap().unwrap().pixels(),
                    &[55, 66, 77, 255]
                );
                assert_eq!(after.last_document_failure, None);
            }
            let _ = session.shutdown();
        }
    }

    #[test]
    fn prior_live_document_survives_admission_until_replacement_publication() {
        let port = NavigationEnginePort::spawn_with_executor(limits(), || {
            Ok(DocumentExecutor { document: None })
        })
        .unwrap();
        let session_limits = crate::SessionLimits::new(2, 4, 4, 4, 4, 4_096, 4, 4_096, 8).unwrap();
        let mut session = crate::BrowserSession::new(port, session_limits).unwrap();
        let tab = crate::BrowserTabId::new(1).unwrap();
        let navigation = match session
            .navigate_new(tab, "https://mutation-budget.invalid/")
            .unwrap()
        {
            crate::BrowserCommandOutcome::NavigationQueued { navigation, .. } => navigation,
            other => panic!("unexpected navigation outcome: {other:?}"),
        };
        wait_for_session_document_navigation(&mut session, tab, navigation);
        let _operation =
            apply_suppressed_mutation(&mut session, tab, navigation, "retained mapping");
        let snapshot = session.tab_snapshot(tab).unwrap();
        assert!(snapshot.mutation_result.is_some());
        assert_eq!(
            snapshot.engine_document_navigation,
            Some(navigation),
            "suppressed UI frame must not suppress engine document state",
        );
        assert_eq!(
            snapshot.last_document_failure,
            Some(DocumentOperationFailure::ResourceLimit)
        );
        assert!(session.frame(tab).unwrap().is_none());
        assert_eq!(session.retained_frame_bytes(), 0);

        let navigation_b = match session
            .navigate_new(tab, "https://replacement.invalid/")
            .unwrap()
        {
            crate::BrowserCommandOutcome::NavigationQueued { navigation, .. } => navigation,
            other => panic!("unexpected navigation outcome: {other:?}"),
        };
        let snapshot = session.tab_snapshot(tab).unwrap();
        assert!(snapshot.mutation_result.is_some());
        assert_eq!(snapshot.engine_document_navigation, Some(navigation));
        assert!(snapshot.engine_live_version.is_some());
        assert!(snapshot.engine_frame_version.is_some());
        assert!(session.frame(tab).unwrap().is_none());
        wait_for_session_document_navigation(&mut session, tab, navigation_b);
        assert_eq!(session.tab_snapshot(tab).unwrap().mutation_result, None);
        let _ =
            apply_suppressed_mutation(&mut session, tab, navigation_b, "before reload replacement");
        assert!(session.tab_snapshot(tab).unwrap().mutation_result.is_some());

        let reload_navigation = match session.reload(tab).unwrap() {
            crate::BrowserCommandOutcome::NavigationQueued { navigation, .. } => navigation,
            other => panic!("unexpected reload outcome: {other:?}"),
        };
        let snapshot = session.tab_snapshot(tab).unwrap();
        assert!(snapshot.mutation_result.is_some());
        assert_eq!(snapshot.engine_document_navigation, Some(navigation_b));
        assert!(session.frame(tab).unwrap().is_none());
        wait_for_session_document_navigation(&mut session, tab, reload_navigation);
        let _ = apply_suppressed_mutation(
            &mut session,
            tab,
            reload_navigation,
            "before history replacement",
        );
        assert!(session.tab_snapshot(tab).unwrap().mutation_result.is_some());

        let history_navigation = match session
            .dispatch(crate::BrowserCommand::Back { tab })
            .unwrap()
        {
            crate::BrowserCommandOutcome::NavigationQueued { navigation, .. } => navigation,
            other => panic!("unexpected history outcome: {other:?}"),
        };
        let snapshot = session.tab_snapshot(tab).unwrap();
        assert!(snapshot.mutation_result.is_some());
        assert_eq!(snapshot.engine_document_navigation, Some(reload_navigation));
        assert!(session.frame(tab).unwrap().is_none());
        wait_for_session_document_navigation(&mut session, tab, history_navigation);
        assert_eq!(
            session
                .tab_snapshot(tab)
                .unwrap()
                .engine_document_navigation,
            Some(history_navigation),
        );
        let _ = session.shutdown();
    }
}
