//! Typed, transport-neutral, size-bounded IPC envelopes.
//!
//! Decoding validates the complete fixed header, protocol version, flags,
//! message kind, service identities, and declared length before invoking a
//! message payload decoder. The crate performs no I/O and is independent of a
//! socket, pipe, shared-memory, or process implementation.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::num::NonZeroU64;
use wild_buzzard_services::{IdentityDecodeError, WireServiceIdentity};

/// Marker at the start of every Wild Buzzard IPC envelope.
pub const MAGIC: [u8; 4] = *b"WBIP";

/// Size of the version-one fixed wire header.
pub const HEADER_LEN: usize = 96;

/// Absolute payload ceiling enforced even if a caller requests a larger limit.
pub const HARD_MAX_PAYLOAD_LEN: usize = 16 * 1024 * 1024;

const KNOWN_FLAGS: u16 =
    EnvelopeFlags::REQUEST.0 | EnvelopeFlags::RESPONSE.0 | EnvelopeFlags::CONTROL.0;

/// A protocol version with a compatibility-major and additive minor number.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ProtocolVersion {
    major: u16,
    minor: u16,
}

/// A stable non-zero protocol or domain discriminator.
///
/// Message kinds need be unique only within one `ProtocolId`. The checked-in
/// protocol registry owned by the orchestrator is the source of assignments;
/// this crate intentionally has no mutable process-global registry.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ProtocolId(u32);

impl ProtocolId {
    /// Defines a protocol/domain ID.
    ///
    /// # Panics
    ///
    /// Panics if `value` is zero, which is reserved for malformed input.
    #[must_use]
    pub const fn new(value: u32) -> Self {
        assert!(value != 0, "IPC protocol ID zero is reserved");
        Self(value)
    }

    /// Returns the wire value.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl ProtocolVersion {
    /// Creates a non-zero-major protocol version.
    ///
    /// # Panics
    ///
    /// Panics if `major` is zero, which is reserved for malformed input.
    #[must_use]
    pub const fn new(major: u16, minor: u16) -> Self {
        assert!(major != 0, "IPC protocol major zero is reserved");
        Self { major, minor }
    }

    /// Returns the compatibility-major number.
    #[must_use]
    pub const fn major(self) -> u16 {
        self.major
    }

    /// Returns the additive minor number.
    #[must_use]
    pub const fn minor(self) -> u16 {
        self.minor
    }
}

/// A non-zero application message discriminator.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct MessageKind(u32);

impl MessageKind {
    /// Defines a message kind.
    ///
    /// # Panics
    ///
    /// Panics if `value` is zero, which is reserved for malformed input.
    #[must_use]
    pub const fn new(value: u32) -> Self {
        assert!(value != 0, "IPC message kind zero is reserved");
        Self(value)
    }

    /// Returns the wire value.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// Optional routing semantics carried by an envelope.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct EnvelopeFlags(u16);

impl EnvelopeFlags {
    /// No request, response, or control classification.
    pub const NONE: Self = Self(0);
    /// A request expecting a correlated response.
    pub const REQUEST: Self = Self(1 << 0);
    /// A response to an earlier request.
    pub const RESPONSE: Self = Self(1 << 1);
    /// A lifecycle or protocol-control message.
    pub const CONTROL: Self = Self(1 << 2);

    /// Combines non-conflicting flags.
    pub fn union(self, other: Self) -> Result<Self, InvalidFlags> {
        Self::from_bits(self.0 | other.0)
    }

    /// Validates raw flag bits and rejects request/response ambiguity.
    pub fn from_bits(bits: u16) -> Result<Self, InvalidFlags> {
        let unknown = bits & !KNOWN_FLAGS;
        if unknown != 0 {
            return Err(InvalidFlags::UnknownBits { bits: unknown });
        }
        if bits & Self::REQUEST.0 != 0 && bits & Self::RESPONSE.0 != 0 {
            return Err(InvalidFlags::RequestAndResponse);
        }
        Ok(Self(bits))
    }

    /// Returns the validated wire bits.
    #[must_use]
    pub const fn bits(self) -> u16 {
        self.0
    }

    /// Returns whether every bit in `other` is present.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

/// Invalid envelope-flag combinations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvalidFlags {
    /// Bits unknown to this protocol version were set.
    UnknownBits {
        /// Unsupported bits.
        bits: u16,
    },
    /// One envelope cannot simultaneously be a request and a response.
    RequestAndResponse,
}

impl fmt::Display for InvalidFlags {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownBits { bits } => write!(formatter, "unknown IPC flag bits: {bits:#06x}"),
            Self::RequestAndResponse => {
                formatter.write_str("IPC envelope cannot be both request and response")
            }
        }
    }
}

impl Error for InvalidFlags {}

/// A non-zero request/response correlation identity.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct CorrelationId(NonZeroU64);

impl CorrelationId {
    /// Creates an ID, rejecting the reserved zero representation.
    #[must_use]
    pub const fn new(value: u64) -> Option<Self> {
        match NonZeroU64::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    /// Returns the wire value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// A typed message with a bounded, deterministic binary payload codec.
pub trait IpcMessage: Sized {
    /// Protocol/domain containing this message kind.
    const PROTOCOL: ProtocolId;

    /// Message discriminator reserved for this payload type.
    const KIND: MessageKind;

    /// Maximum encoded payload accepted for this specific message type.
    const MAX_PAYLOAD_LEN: usize;

    /// Encodes into a writer that rejects bytes beyond the active limit.
    fn encode_payload(&self, encoder: &mut PayloadEncoder) -> Result<(), PayloadCodecError>;

    /// Decodes from a cursor bounded to the declared payload.
    fn decode_payload(decoder: &mut PayloadDecoder<'_>) -> Result<Self, PayloadCodecError>;
}

/// One typed message and its transport-independent routing metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Envelope<M> {
    /// Typed payload.
    pub message: M,
    /// Sender service identity.
    pub source: WireServiceIdentity,
    /// Receiver service identity.
    pub destination: WireServiceIdentity,
    /// Optional request/response identity.
    pub correlation: Option<CorrelationId>,
    /// Validated routing flags.
    pub flags: EnvelopeFlags,
}

/// Configuration rejected before a codec can be used.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CodecConfigError {
    /// The oldest supported minor cannot exceed the emitted current minor.
    InvalidMinorRange {
        /// Oldest accepted minor.
        minimum: u16,
        /// Current emitted and newest accepted minor.
        current: u16,
    },
    /// The configured limit exceeds the hard project ceiling.
    PayloadLimitExceedsHardMaximum {
        /// Requested maximum.
        requested: usize,
        /// Hard project maximum.
        hard_maximum: usize,
    },
}

impl fmt::Display for CodecConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidMinorRange { minimum, current } => write!(
                formatter,
                "minimum supported IPC minor {minimum} exceeds current minor {current}"
            ),
            Self::PayloadLimitExceedsHardMaximum {
                requested,
                hard_maximum,
            } => write!(
                formatter,
                "IPC payload limit {requested} exceeds hard maximum {hard_maximum}"
            ),
        }
    }
}

impl Error for CodecConfigError {}

/// A configured version and size policy for typed envelopes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EnvelopeCodec {
    protocol: ProtocolId,
    current: ProtocolVersion,
    minimum_minor: u16,
    max_payload_len: usize,
}

impl EnvelopeCodec {
    /// Creates a codec with an explicit compatibility and size policy.
    pub fn new(
        protocol: ProtocolId,
        current: ProtocolVersion,
        minimum_minor: u16,
        max_payload_len: usize,
    ) -> Result<Self, CodecConfigError> {
        if minimum_minor > current.minor {
            return Err(CodecConfigError::InvalidMinorRange {
                minimum: minimum_minor,
                current: current.minor,
            });
        }
        if max_payload_len > HARD_MAX_PAYLOAD_LEN {
            return Err(CodecConfigError::PayloadLimitExceedsHardMaximum {
                requested: max_payload_len,
                hard_maximum: HARD_MAX_PAYLOAD_LEN,
            });
        }
        Ok(Self {
            protocol,
            current,
            minimum_minor,
            max_payload_len,
        })
    }

    /// Returns the protocol/domain accepted by this codec.
    #[must_use]
    pub const fn protocol(self) -> ProtocolId {
        self.protocol
    }

    /// Returns the version emitted by this codec.
    #[must_use]
    pub const fn current_version(self) -> ProtocolVersion {
        self.current
    }

    /// Returns the configured per-envelope payload bound.
    #[must_use]
    pub const fn max_payload_len(self) -> usize {
        self.max_payload_len
    }

    /// Encodes one typed envelope into an owned transport buffer.
    pub fn encode<M: IpcMessage>(&self, envelope: &Envelope<M>) -> Result<Vec<u8>, EncodeError> {
        if M::PROTOCOL != self.protocol {
            return Err(EncodeError::ProtocolMismatch {
                codec: self.protocol,
                message: M::PROTOCOL,
            });
        }
        let active_limit = self.max_payload_len.min(M::MAX_PAYLOAD_LEN);
        let mut payload = PayloadEncoder::new(active_limit);
        envelope
            .message
            .encode_payload(&mut payload)
            .map_err(EncodeError::Payload)?;
        let payload = payload.finish();
        let payload_len =
            u32::try_from(payload.len()).map_err(|_| EncodeError::PayloadTooLarge {
                actual: payload.len(),
                limit: active_limit,
            })?;

        let mut bytes = Vec::with_capacity(HEADER_LEN + payload.len());
        bytes.extend_from_slice(&MAGIC);
        put_u16(&mut bytes, HEADER_LEN as u16);
        put_u16(&mut bytes, self.current.major);
        put_u16(&mut bytes, self.current.minor);
        put_u16(&mut bytes, envelope.flags.bits());
        put_u32(&mut bytes, self.protocol.get());
        put_u32(&mut bytes, M::KIND.get());
        put_u32(&mut bytes, payload_len);
        put_u64(
            &mut bytes,
            envelope.correlation.map_or(0, CorrelationId::get),
        );
        put_identity(&mut bytes, envelope.source);
        put_identity(&mut bytes, envelope.destination);
        debug_assert_eq!(bytes.len(), HEADER_LEN);
        bytes.extend_from_slice(&payload);
        Ok(bytes)
    }

    /// Decodes and validates one exact typed envelope.
    pub fn decode<M: IpcMessage>(&self, bytes: &[u8]) -> Result<Envelope<M>, DecodeError> {
        if bytes.len() < HEADER_LEN {
            return Err(DecodeError::TruncatedHeader {
                actual: bytes.len(),
                required: HEADER_LEN,
            });
        }

        let magic: [u8; 4] = bytes[0..4]
            .try_into()
            .expect("fixed four-byte slice converts to an array");
        if magic != MAGIC {
            return Err(DecodeError::InvalidMagic { actual: magic });
        }

        let header_len = usize::from(read_u16(bytes, 4));
        if header_len != HEADER_LEN {
            return Err(DecodeError::UnsupportedHeaderLength {
                actual: header_len,
                expected: HEADER_LEN,
            });
        }

        let actual_version = ProtocolVersion {
            major: read_u16(bytes, 6),
            minor: read_u16(bytes, 8),
        };
        self.validate_version(actual_version)?;

        let flags =
            EnvelopeFlags::from_bits(read_u16(bytes, 10)).map_err(DecodeError::InvalidFlags)?;
        let actual_protocol = read_u32(bytes, 12);
        if actual_protocol == 0 {
            return Err(DecodeError::ZeroProtocolId);
        }
        if actual_protocol != self.protocol.get() {
            return Err(DecodeError::UnexpectedProtocol {
                expected: self.protocol,
                actual: actual_protocol,
            });
        }
        if M::PROTOCOL != self.protocol {
            return Err(DecodeError::TypedMessageProtocolMismatch {
                codec: self.protocol,
                message: M::PROTOCOL,
            });
        }

        let actual_kind = read_u32(bytes, 16);
        if actual_kind == 0 {
            return Err(DecodeError::ZeroMessageKind);
        }
        if actual_kind != M::KIND.get() {
            return Err(DecodeError::UnexpectedMessageKind {
                expected: M::KIND,
                actual: actual_kind,
            });
        }

        let declared_payload_len = read_u32(bytes, 20) as usize;
        let active_limit = self.max_payload_len.min(M::MAX_PAYLOAD_LEN);
        if declared_payload_len > active_limit {
            return Err(DecodeError::PayloadTooLarge {
                declared: declared_payload_len,
                limit: active_limit,
            });
        }

        let expected_total =
            HEADER_LEN
                .checked_add(declared_payload_len)
                .ok_or(DecodeError::LengthOverflow {
                    declared_payload_len,
                })?;
        if bytes.len() < expected_total {
            return Err(DecodeError::TruncatedPayload {
                declared: declared_payload_len,
                available: bytes.len() - HEADER_LEN,
            });
        }
        if bytes.len() > expected_total {
            return Err(DecodeError::TrailingEnvelopeBytes {
                expected_total,
                actual_total: bytes.len(),
            });
        }

        let correlation_value = read_u64(bytes, 24);
        let correlation = CorrelationId::new(correlation_value);
        let source = read_identity(bytes, 32).map_err(|error| DecodeError::InvalidIdentity {
            endpoint: Endpoint::Source,
            error,
        })?;
        let destination =
            read_identity(bytes, 64).map_err(|error| DecodeError::InvalidIdentity {
                endpoint: Endpoint::Destination,
                error,
            })?;

        let mut decoder = PayloadDecoder::new(&bytes[HEADER_LEN..expected_total]);
        let message = M::decode_payload(&mut decoder).map_err(DecodeError::Payload)?;
        decoder.finish().map_err(DecodeError::Payload)?;

        Ok(Envelope {
            message,
            source,
            destination,
            correlation,
            flags,
        })
    }

    fn validate_version(&self, actual: ProtocolVersion) -> Result<(), DecodeError> {
        if actual.major != self.current.major {
            return Err(DecodeError::MajorVersionMismatch {
                expected: self.current.major,
                actual: actual.major,
            });
        }
        if actual.minor < self.minimum_minor || actual.minor > self.current.minor {
            return Err(DecodeError::UnsupportedMinorVersion {
                minimum: self.minimum_minor,
                maximum: self.current.minor,
                actual: actual.minor,
            });
        }
        Ok(())
    }
}

/// Which endpoint identity failed validation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Endpoint {
    /// Sender identity.
    Source,
    /// Receiver identity.
    Destination,
}

/// An envelope encoding failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EncodeError {
    /// The typed message belongs to a different protocol than the codec.
    ProtocolMismatch {
        /// Protocol configured on the codec.
        codec: ProtocolId,
        /// Protocol declared by the message type.
        message: ProtocolId,
    },
    /// The typed payload codec rejected its input.
    Payload(PayloadCodecError),
    /// The payload exceeded either its message-specific or configured bound.
    PayloadTooLarge {
        /// Encoded byte count.
        actual: usize,
        /// Active byte limit.
        limit: usize,
    },
}

impl fmt::Display for EncodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProtocolMismatch { codec, message } => write!(
                formatter,
                "IPC message protocol {} does not match codec protocol {}",
                message.get(),
                codec.get()
            ),
            Self::Payload(error) => write!(formatter, "IPC payload encode failed: {error}"),
            Self::PayloadTooLarge { actual, limit } => {
                write!(
                    formatter,
                    "IPC payload length {actual} exceeds limit {limit}"
                )
            }
        }
    }
}

impl Error for EncodeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Payload(error) => Some(error),
            Self::ProtocolMismatch { .. } | Self::PayloadTooLarge { .. } => None,
        }
    }
}

/// A structured envelope validation or decoding failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DecodeError {
    /// Fewer bytes than the fixed header were received.
    TruncatedHeader {
        /// Received bytes.
        actual: usize,
        /// Required bytes.
        required: usize,
    },
    /// The stream does not begin with [`MAGIC`].
    InvalidMagic {
        /// Received marker.
        actual: [u8; 4],
    },
    /// This implementation does not understand the declared header layout.
    UnsupportedHeaderLength {
        /// Received length.
        actual: usize,
        /// Required length.
        expected: usize,
    },
    /// Breaking protocol-major mismatch.
    MajorVersionMismatch {
        /// Supported major.
        expected: u16,
        /// Received major.
        actual: u16,
    },
    /// Minor is older or newer than the configured compatibility window.
    UnsupportedMinorVersion {
        /// Oldest supported minor.
        minimum: u16,
        /// Newest supported minor.
        maximum: u16,
        /// Received minor.
        actual: u16,
    },
    /// Flags were unknown or mutually exclusive.
    InvalidFlags(InvalidFlags),
    /// Protocol/domain ID zero is reserved.
    ZeroProtocolId,
    /// The wire envelope belongs to a different protocol/domain.
    UnexpectedProtocol {
        /// Codec protocol.
        expected: ProtocolId,
        /// Received wire value.
        actual: u32,
    },
    /// The requested Rust message type belongs to another protocol/domain.
    TypedMessageProtocolMismatch {
        /// Codec protocol.
        codec: ProtocolId,
        /// Typed message protocol.
        message: ProtocolId,
    },
    /// Message kind zero is reserved.
    ZeroMessageKind,
    /// The caller requested a different typed payload than the wire kind.
    UnexpectedMessageKind {
        /// Typed payload kind.
        expected: MessageKind,
        /// Received kind.
        actual: u32,
    },
    /// Declared payload exceeds the active bound.
    PayloadTooLarge {
        /// Declared byte count.
        declared: usize,
        /// Active byte limit.
        limit: usize,
    },
    /// Header plus payload length overflowed `usize`.
    LengthOverflow {
        /// Declared payload byte count.
        declared_payload_len: usize,
    },
    /// Fewer payload bytes arrived than declared.
    TruncatedPayload {
        /// Declared byte count.
        declared: usize,
        /// Received payload bytes.
        available: usize,
    },
    /// Bytes remain after the exact declared envelope.
    TrailingEnvelopeBytes {
        /// Header plus declared payload.
        expected_total: usize,
        /// Actual buffer length.
        actual_total: usize,
    },
    /// A source or destination service identity was malformed.
    InvalidIdentity {
        /// Identity field that failed.
        endpoint: Endpoint,
        /// Underlying validation failure.
        error: IdentityDecodeError,
    },
    /// The typed payload codec rejected its bounded slice.
    Payload(PayloadCodecError),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TruncatedHeader { actual, required } => {
                write!(
                    formatter,
                    "truncated IPC header: received {actual}, need {required}"
                )
            }
            Self::InvalidMagic { actual } => write!(formatter, "invalid IPC magic: {actual:?}"),
            Self::UnsupportedHeaderLength { actual, expected } => write!(
                formatter,
                "unsupported IPC header length {actual}, expected {expected}"
            ),
            Self::MajorVersionMismatch { expected, actual } => write!(
                formatter,
                "IPC major version mismatch: expected {expected}, received {actual}"
            ),
            Self::UnsupportedMinorVersion {
                minimum,
                maximum,
                actual,
            } => write!(
                formatter,
                "unsupported IPC minor {actual}; supported range is {minimum}..={maximum}"
            ),
            Self::InvalidFlags(error) => write!(formatter, "invalid IPC flags: {error}"),
            Self::ZeroProtocolId => formatter.write_str("IPC protocol ID zero is reserved"),
            Self::UnexpectedProtocol { expected, actual } => write!(
                formatter,
                "unexpected IPC protocol: expected {}, received {actual}",
                expected.get()
            ),
            Self::TypedMessageProtocolMismatch { codec, message } => write!(
                formatter,
                "typed IPC message protocol {} does not match codec protocol {}",
                message.get(),
                codec.get()
            ),
            Self::ZeroMessageKind => formatter.write_str("IPC message kind zero is reserved"),
            Self::UnexpectedMessageKind { expected, actual } => write!(
                formatter,
                "unexpected IPC message kind: expected {}, received {actual}",
                expected.get()
            ),
            Self::PayloadTooLarge { declared, limit } => write!(
                formatter,
                "declared IPC payload length {declared} exceeds limit {limit}"
            ),
            Self::LengthOverflow {
                declared_payload_len,
            } => write!(
                formatter,
                "IPC payload length {declared_payload_len} overflows the envelope length"
            ),
            Self::TruncatedPayload {
                declared,
                available,
            } => write!(
                formatter,
                "truncated IPC payload: declared {declared}, received {available}"
            ),
            Self::TrailingEnvelopeBytes {
                expected_total,
                actual_total,
            } => write!(
                formatter,
                "IPC envelope has trailing bytes: expected {expected_total}, received {actual_total}"
            ),
            Self::InvalidIdentity { endpoint, error } => {
                write!(formatter, "invalid {endpoint:?} service identity: {error}")
            }
            Self::Payload(error) => write!(formatter, "IPC payload decode failed: {error}"),
        }
    }
}

impl Error for DecodeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidFlags(error) => Some(error),
            Self::InvalidIdentity { error, .. } => Some(error),
            Self::Payload(error) => Some(error),
            _ => None,
        }
    }
}

/// A bounded payload serialization or parsing failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PayloadCodecError {
    /// Writing the requested bytes would exceed the active bound.
    LimitExceeded {
        /// Byte count after the requested write.
        attempted: usize,
        /// Active byte limit.
        limit: usize,
    },
    /// A length cannot be represented by the wire's `u32` length field.
    LengthNotRepresentable {
        /// Host length.
        length: usize,
    },
    /// The bounded payload ended before a field was complete.
    UnexpectedEnd {
        /// Bytes required for the field.
        needed: usize,
        /// Bytes remaining.
        remaining: usize,
    },
    /// A field's bit pattern violated its message-level contract.
    InvalidValue {
        /// Static field name.
        field: &'static str,
        /// Static diagnostic reason.
        reason: &'static str,
    },
    /// A decoder did not consume the complete declared payload.
    TrailingBytes {
        /// Unconsumed bytes.
        remaining: usize,
    },
}

impl fmt::Display for PayloadCodecError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LimitExceeded { attempted, limit } => write!(
                formatter,
                "payload write would reach {attempted} bytes, above limit {limit}"
            ),
            Self::LengthNotRepresentable { length } => {
                write!(
                    formatter,
                    "payload length {length} is not representable as u32"
                )
            }
            Self::UnexpectedEnd { needed, remaining } => write!(
                formatter,
                "payload ended early: need {needed} bytes, have {remaining}"
            ),
            Self::InvalidValue { field, reason } => {
                write!(formatter, "invalid payload field {field}: {reason}")
            }
            Self::TrailingBytes { remaining } => {
                write!(formatter, "payload has {remaining} trailing bytes")
            }
        }
    }
}

impl Error for PayloadCodecError {}

/// Failure to add a handler to a caller-owned protocol table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HandlerRegistrationError {
    /// The message type belongs to another protocol/domain.
    ProtocolMismatch {
        /// Registry protocol.
        registry: ProtocolId,
        /// Message protocol.
        message: ProtocolId,
    },
    /// A handler already owns this protocol-local message kind.
    DuplicateKind {
        /// Registry protocol.
        protocol: ProtocolId,
        /// Colliding message kind.
        kind: MessageKind,
    },
}

impl fmt::Display for HandlerRegistrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProtocolMismatch { registry, message } => write!(
                formatter,
                "handler message protocol {} does not match registry protocol {}",
                message.get(),
                registry.get()
            ),
            Self::DuplicateKind { protocol, kind } => write!(
                formatter,
                "duplicate handler for protocol {} message kind {}",
                protocol.get(),
                kind.get()
            ),
        }
    }
}

impl Error for HandlerRegistrationError {}

/// A caller-owned handler table for exactly one protocol/domain.
///
/// This is deliberately an ordinary value rather than mutable global state.
/// Construction rejects duplicate kinds so dispatch cannot depend on insertion
/// order. The handler type is selected by the integrating transport/runtime.
#[derive(Debug)]
pub struct MessageHandlerRegistry<H> {
    protocol: ProtocolId,
    handlers: BTreeMap<MessageKind, H>,
}

impl<H> MessageHandlerRegistry<H> {
    /// Creates an empty table for one protocol/domain.
    #[must_use]
    pub const fn new(protocol: ProtocolId) -> Self {
        Self {
            protocol,
            handlers: BTreeMap::new(),
        }
    }

    /// Adds the handler for a typed message, rejecting mismatch or collision.
    pub fn register<M: IpcMessage>(&mut self, handler: H) -> Result<(), HandlerRegistrationError> {
        if M::PROTOCOL != self.protocol {
            return Err(HandlerRegistrationError::ProtocolMismatch {
                registry: self.protocol,
                message: M::PROTOCOL,
            });
        }
        if self.handlers.contains_key(&M::KIND) {
            return Err(HandlerRegistrationError::DuplicateKind {
                protocol: self.protocol,
                kind: M::KIND,
            });
        }
        self.handlers.insert(M::KIND, handler);
        Ok(())
    }

    /// Returns a registered handler by protocol-local kind.
    #[must_use]
    pub fn get(&self, kind: MessageKind) -> Option<&H> {
        self.handlers.get(&kind)
    }

    /// Returns the number of unique registered handlers.
    #[must_use]
    pub fn len(&self) -> usize {
        self.handlers.len()
    }

    /// Returns whether no handlers are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.handlers.is_empty()
    }
}

/// A growable payload writer that enforces its bound before each write.
#[derive(Debug)]
pub struct PayloadEncoder {
    bytes: Vec<u8>,
    limit: usize,
}

impl PayloadEncoder {
    /// Creates an empty writer with an explicit byte bound.
    #[must_use]
    pub fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
        }
    }

    /// Appends one byte.
    pub fn write_u8(&mut self, value: u8) -> Result<(), PayloadCodecError> {
        self.write_bytes(&[value])
    }

    /// Appends a little-endian `u16`.
    pub fn write_u16(&mut self, value: u16) -> Result<(), PayloadCodecError> {
        self.write_bytes(&value.to_le_bytes())
    }

    /// Appends a little-endian `u32`.
    pub fn write_u32(&mut self, value: u32) -> Result<(), PayloadCodecError> {
        self.write_bytes(&value.to_le_bytes())
    }

    /// Appends a little-endian `u64`.
    pub fn write_u64(&mut self, value: u64) -> Result<(), PayloadCodecError> {
        self.write_bytes(&value.to_le_bytes())
    }

    /// Appends an exact byte slice after checking the active bound.
    pub fn write_bytes(&mut self, value: &[u8]) -> Result<(), PayloadCodecError> {
        let attempted =
            self.bytes
                .len()
                .checked_add(value.len())
                .ok_or(PayloadCodecError::LimitExceeded {
                    attempted: usize::MAX,
                    limit: self.limit,
                })?;
        if attempted > self.limit {
            return Err(PayloadCodecError::LimitExceeded {
                attempted,
                limit: self.limit,
            });
        }
        self.bytes.extend_from_slice(value);
        Ok(())
    }

    /// Appends a `u32` byte length followed by exact bytes.
    pub fn write_length_prefixed_bytes(&mut self, value: &[u8]) -> Result<(), PayloadCodecError> {
        let length =
            u32::try_from(value.len()).map_err(|_| PayloadCodecError::LengthNotRepresentable {
                length: value.len(),
            })?;
        let attempted = self
            .bytes
            .len()
            .checked_add(4)
            .and_then(|length| length.checked_add(value.len()))
            .ok_or(PayloadCodecError::LimitExceeded {
                attempted: usize::MAX,
                limit: self.limit,
            })?;
        if attempted > self.limit {
            return Err(PayloadCodecError::LimitExceeded {
                attempted,
                limit: self.limit,
            });
        }
        self.bytes.extend_from_slice(&length.to_le_bytes());
        self.bytes.extend_from_slice(value);
        Ok(())
    }

    /// Returns the validated payload bytes.
    #[must_use]
    pub fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

/// A cursor restricted to one already size-validated payload slice.
#[derive(Debug)]
pub struct PayloadDecoder<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> PayloadDecoder<'a> {
    /// Creates a cursor over an exact payload slice.
    #[must_use]
    pub const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    /// Reads one byte.
    pub fn read_u8(&mut self) -> Result<u8, PayloadCodecError> {
        Ok(self.read_exact(1)?[0])
    }

    /// Reads a little-endian `u16`.
    pub fn read_u16(&mut self) -> Result<u16, PayloadCodecError> {
        let bytes: [u8; 2] = self
            .read_exact(2)?
            .try_into()
            .expect("fixed two-byte slice converts to an array");
        Ok(u16::from_le_bytes(bytes))
    }

    /// Reads a little-endian `u32`.
    pub fn read_u32(&mut self) -> Result<u32, PayloadCodecError> {
        let bytes: [u8; 4] = self
            .read_exact(4)?
            .try_into()
            .expect("fixed four-byte slice converts to an array");
        Ok(u32::from_le_bytes(bytes))
    }

    /// Reads a little-endian `u64`.
    pub fn read_u64(&mut self) -> Result<u64, PayloadCodecError> {
        let bytes: [u8; 8] = self
            .read_exact(8)?
            .try_into()
            .expect("fixed eight-byte slice converts to an array");
        Ok(u64::from_le_bytes(bytes))
    }

    /// Reads exactly `length` bytes.
    pub fn read_exact(&mut self, length: usize) -> Result<&'a [u8], PayloadCodecError> {
        let remaining = self.remaining();
        if length > remaining {
            return Err(PayloadCodecError::UnexpectedEnd {
                needed: length,
                remaining,
            });
        }
        let end = self.position + length;
        let value = &self.bytes[self.position..end];
        self.position = end;
        Ok(value)
    }

    /// Reads a `u32` byte length and returns the corresponding exact slice.
    pub fn read_length_prefixed_bytes(&mut self) -> Result<&'a [u8], PayloadCodecError> {
        let length = self.read_u32()? as usize;
        self.read_exact(length)
    }

    /// Returns unconsumed bytes.
    #[must_use]
    pub const fn remaining(&self) -> usize {
        self.bytes.len() - self.position
    }

    /// Requires the payload to have been consumed exactly.
    pub fn finish(self) -> Result<(), PayloadCodecError> {
        let remaining = self.remaining();
        if remaining == 0 {
            Ok(())
        } else {
            Err(PayloadCodecError::TrailingBytes { remaining })
        }
    }
}

fn put_u16(bytes: &mut Vec<u8>, value: u16) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn put_u32(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut Vec<u8>, value: u64) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn put_u128(bytes: &mut Vec<u8>, value: u128) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn put_identity(bytes: &mut Vec<u8>, identity: WireServiceIdentity) {
    put_u128(bytes, identity.kind().get());
    put_u64(bytes, identity.namespace().get());
    put_u32(bytes, identity.slot());
    put_u32(bytes, identity.generation());
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(
        bytes[offset..offset + 2]
            .try_into()
            .expect("validated fixed IPC header contains two bytes"),
    )
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(
        bytes[offset..offset + 4]
            .try_into()
            .expect("validated fixed IPC header contains four bytes"),
    )
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(
        bytes[offset..offset + 8]
            .try_into()
            .expect("validated fixed IPC header contains eight bytes"),
    )
}

fn read_u128(bytes: &[u8], offset: usize) -> u128 {
    u128::from_le_bytes(
        bytes[offset..offset + 16]
            .try_into()
            .expect("validated fixed IPC header contains sixteen bytes"),
    )
}

fn read_identity(bytes: &[u8], offset: usize) -> Result<WireServiceIdentity, IdentityDecodeError> {
    WireServiceIdentity::from_parts(
        read_u128(bytes, offset),
        read_u64(bytes, offset + 16),
        read_u32(bytes, offset + 24),
        read_u32(bytes, offset + 28),
    )
}

#[cfg(test)]
mod tests {
    use super::{
        CorrelationId, DecodeError, Endpoint, Envelope, EnvelopeCodec, EnvelopeFlags, HEADER_LEN,
        HandlerRegistrationError, IpcMessage, MAGIC, MessageHandlerRegistry, MessageKind,
        PayloadCodecError, PayloadDecoder, PayloadEncoder, ProtocolId, ProtocolVersion,
    };
    use wild_buzzard_services::{IdentityDecodeError, WireServiceIdentity};

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct Ping {
        sequence: u64,
        label: String,
    }

    impl IpcMessage for Ping {
        const PROTOCOL: ProtocolId = ProtocolId::new(0x10);
        const KIND: MessageKind = MessageKind::new(0x1001);
        const MAX_PAYLOAD_LEN: usize = 64;

        fn encode_payload(&self, encoder: &mut PayloadEncoder) -> Result<(), PayloadCodecError> {
            encoder.write_u64(self.sequence)?;
            encoder.write_length_prefixed_bytes(self.label.as_bytes())
        }

        fn decode_payload(decoder: &mut PayloadDecoder<'_>) -> Result<Self, PayloadCodecError> {
            let sequence = decoder.read_u64()?;
            let label = std::str::from_utf8(decoder.read_length_prefixed_bytes()?)
                .map_err(|_| PayloadCodecError::InvalidValue {
                    field: "label",
                    reason: "not UTF-8",
                })?
                .to_owned();
            Ok(Self { sequence, label })
        }
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct Other;

    impl IpcMessage for Other {
        const PROTOCOL: ProtocolId = Ping::PROTOCOL;
        const KIND: MessageKind = MessageKind::new(0x1002);
        const MAX_PAYLOAD_LEN: usize = 0;

        fn encode_payload(&self, _encoder: &mut PayloadEncoder) -> Result<(), PayloadCodecError> {
            Ok(())
        }

        fn decode_payload(_decoder: &mut PayloadDecoder<'_>) -> Result<Self, PayloadCodecError> {
            Ok(Self)
        }
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct CollidingPing;

    impl IpcMessage for CollidingPing {
        const PROTOCOL: ProtocolId = Ping::PROTOCOL;
        const KIND: MessageKind = Ping::KIND;
        const MAX_PAYLOAD_LEN: usize = 0;

        fn encode_payload(&self, _encoder: &mut PayloadEncoder) -> Result<(), PayloadCodecError> {
            Ok(())
        }

        fn decode_payload(_decoder: &mut PayloadDecoder<'_>) -> Result<Self, PayloadCodecError> {
            Ok(Self)
        }
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct OtherProtocolPing;

    impl IpcMessage for OtherProtocolPing {
        const PROTOCOL: ProtocolId = ProtocolId::new(0x11);
        const KIND: MessageKind = Ping::KIND;
        const MAX_PAYLOAD_LEN: usize = 0;

        fn encode_payload(&self, _encoder: &mut PayloadEncoder) -> Result<(), PayloadCodecError> {
            Ok(())
        }

        fn decode_payload(_decoder: &mut PayloadDecoder<'_>) -> Result<Self, PayloadCodecError> {
            Ok(Self)
        }
    }

    fn endpoint(kind: u128, namespace: u64) -> WireServiceIdentity {
        WireServiceIdentity::from_parts(kind, namespace, 3, 9).unwrap()
    }

    fn codec() -> EnvelopeCodec {
        EnvelopeCodec::new(Ping::PROTOCOL, ProtocolVersion::new(1, 2), 1, 1024).unwrap()
    }

    fn envelope() -> Envelope<Ping> {
        Envelope {
            message: Ping {
                sequence: 42,
                label: String::from("hello"),
            },
            source: endpoint(11, 21),
            destination: endpoint(12, 22),
            correlation: CorrelationId::new(99),
            flags: EnvelopeFlags::REQUEST,
        }
    }

    #[test]
    fn typed_envelope_round_trips_exactly() {
        let envelope = envelope();
        let bytes = codec().encode(&envelope).unwrap();

        assert_eq!(&bytes[0..4], &MAGIC);
        assert_eq!(bytes.len(), HEADER_LEN + 17);
        assert_eq!(codec().decode::<Ping>(&bytes).unwrap(), envelope);
    }

    #[test]
    fn every_truncated_header_is_rejected_without_panicking() {
        let bytes = codec().encode(&envelope()).unwrap();
        for length in 0..HEADER_LEN {
            assert_eq!(
                codec().decode::<Ping>(&bytes[..length]),
                Err(DecodeError::TruncatedHeader {
                    actual: length,
                    required: HEADER_LEN,
                })
            );
        }
    }

    #[test]
    fn malformed_magic_and_flags_are_structured_errors() {
        let mut bytes = codec().encode(&envelope()).unwrap();
        bytes[0] ^= 0xff;
        assert!(matches!(
            codec().decode::<Ping>(&bytes),
            Err(DecodeError::InvalidMagic { .. })
        ));

        let mut bytes = codec().encode(&envelope()).unwrap();
        bytes[10..12].copy_from_slice(&0x8000_u16.to_le_bytes());
        assert!(matches!(
            codec().decode::<Ping>(&bytes),
            Err(DecodeError::InvalidFlags(_))
        ));
    }

    #[test]
    fn major_and_minor_version_mismatches_are_rejected() {
        let mut major = codec().encode(&envelope()).unwrap();
        major[6..8].copy_from_slice(&2_u16.to_le_bytes());
        assert_eq!(
            codec().decode::<Ping>(&major),
            Err(DecodeError::MajorVersionMismatch {
                expected: 1,
                actual: 2,
            })
        );

        let mut minor = codec().encode(&envelope()).unwrap();
        minor[8..10].copy_from_slice(&3_u16.to_le_bytes());
        assert_eq!(
            codec().decode::<Ping>(&minor),
            Err(DecodeError::UnsupportedMinorVersion {
                minimum: 1,
                maximum: 2,
                actual: 3,
            })
        );
    }

    #[test]
    fn oversized_payload_is_rejected_before_payload_decode() {
        let mut bytes = codec().encode(&envelope()).unwrap();
        bytes[20..24].copy_from_slice(&65_u32.to_le_bytes());
        assert_eq!(
            codec().decode::<Ping>(&bytes),
            Err(DecodeError::PayloadTooLarge {
                declared: 65,
                limit: Ping::MAX_PAYLOAD_LEN,
            })
        );
    }

    #[test]
    fn wrong_typed_message_and_trailing_bytes_are_rejected() {
        let bytes = codec().encode(&envelope()).unwrap();
        assert_eq!(
            codec().decode::<Other>(&bytes),
            Err(DecodeError::UnexpectedMessageKind {
                expected: Other::KIND,
                actual: Ping::KIND.get(),
            })
        );

        let mut trailing = bytes;
        trailing.push(0);
        assert!(matches!(
            codec().decode::<Ping>(&trailing),
            Err(DecodeError::TrailingEnvelopeBytes { .. })
        ));
    }

    #[test]
    fn malformed_service_identity_is_rejected() {
        let mut bytes = codec().encode(&envelope()).unwrap();
        bytes[60..64].copy_from_slice(&0_u32.to_le_bytes());
        assert_eq!(
            codec().decode::<Ping>(&bytes),
            Err(DecodeError::InvalidIdentity {
                endpoint: Endpoint::Source,
                error: IdentityDecodeError::ZeroGeneration,
            })
        );
    }

    #[test]
    fn bounded_payload_writer_refuses_growth_before_writing() {
        let mut writer = PayloadEncoder::new(3);
        assert_eq!(
            writer.write_bytes(&[1, 2, 3, 4]),
            Err(PayloadCodecError::LimitExceeded {
                attempted: 4,
                limit: 3,
            })
        );
        assert!(writer.finish().is_empty());
    }

    #[test]
    fn duplicate_handlers_are_rejected_within_one_protocol() {
        let mut registry = MessageHandlerRegistry::new(Ping::PROTOCOL);
        registry.register::<Ping>("ping handler").unwrap();
        assert_eq!(
            registry.register::<CollidingPing>("collision"),
            Err(HandlerRegistrationError::DuplicateKind {
                protocol: Ping::PROTOCOL,
                kind: Ping::KIND,
            })
        );
        assert_eq!(registry.len(), 1);

        let mut other_registry = MessageHandlerRegistry::new(OtherProtocolPing::PROTOCOL);
        other_registry
            .register::<OtherProtocolPing>("same local kind, other protocol")
            .unwrap();
        assert_eq!(other_registry.len(), 1);
    }

    #[test]
    fn handler_registry_rejects_protocol_mismatch() {
        let mut registry = MessageHandlerRegistry::new(Ping::PROTOCOL);
        assert_eq!(
            registry.register::<OtherProtocolPing>("wrong domain"),
            Err(HandlerRegistrationError::ProtocolMismatch {
                registry: Ping::PROTOCOL,
                message: OtherProtocolPing::PROTOCOL,
            })
        );
    }

    #[test]
    #[should_panic(expected = "IPC protocol ID zero is reserved")]
    fn protocol_id_zero_is_reserved() {
        let _ = ProtocolId::new(0);
    }

    #[test]
    #[should_panic(expected = "IPC message kind zero is reserved")]
    fn message_kind_zero_is_reserved() {
        let _ = MessageKind::new(0);
    }
}
