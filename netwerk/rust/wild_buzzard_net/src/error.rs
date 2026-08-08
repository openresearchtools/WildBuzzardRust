// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::{fmt, io};

/// Result type used by this crate.
pub type Result<T> = std::result::Result<T, Error>;

/// The bounded resource whose configured limit was exceeded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum LimitKind {
    /// Bytes in the response status line and header section.
    HeaderBytes,
    /// Number of response header fields.
    HeaderCount,
    /// Total decoded response-body bytes.
    BodyBytes,
    /// Bytes in one chunk-size line.
    ChunkLineBytes,
    /// Number of informational response heads before the final response.
    InformationalResponses,
    /// Bytes in the fully serialized outgoing request head.
    RequestHeadBytes,
    /// Number of caller-supplied outgoing request header fields.
    RequestHeaderCount,
    /// Bytes in an outgoing request body.
    RequestBodyBytes,
}

/// A network operation associated with an I/O failure or timeout.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum Operation {
    /// Establishing the loopback TCP connection.
    Connect,
    /// Writing the HTTP request.
    WriteRequest,
    /// Reading the response status and headers.
    ReadHead,
    /// Reading the response body.
    ReadBody,
}

/// Structured failures produced while validating or transporting HTTP.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum Error {
    /// The supplied URL could not be parsed as a WHATWG URL.
    InvalidUrl(String),
    /// Only cleartext `http` is implemented by this bounded transport.
    UnsupportedScheme(String),
    /// Credentials are not accepted at the transport boundary.
    CredentialsNotAllowed,
    /// URL fragments must be removed by the caller before transport.
    FragmentNotAllowed,
    /// The target was not a numeric loopback IP address.
    NonLoopbackTarget,
    /// The target URL did not contain a usable TCP port.
    MissingPort,
    /// A request target was not valid HTTP origin-form.
    InvalidRequestTarget,
    /// A method was empty or contained a non-token byte.
    InvalidMethod,
    /// A header field name was empty or contained a non-token byte.
    InvalidHeaderName,
    /// A header field value contained a prohibited control byte.
    InvalidHeaderValue,
    /// The caller tried to set a transport-owned request header.
    ReservedRequestHeader(String),
    /// A configured resource limit was exceeded.
    LimitExceeded {
        /// Resource that exceeded its limit.
        kind: LimitKind,
        /// Configured maximum.
        limit: usize,
    },
    /// The cancellation token was signalled.
    Cancelled,
    /// A deadline or inactivity timeout expired.
    Timeout(Operation),
    /// A socket operation failed.
    Io {
        /// Operation that failed.
        operation: Operation,
        /// Stable `std::io` error classification.
        kind: io::ErrorKind,
    },
    /// A response line ended with bare LF or otherwise violated CRLF framing.
    InvalidLineEnding,
    /// The response status line was malformed or unsupported.
    MalformedStatusLine,
    /// A response header field was malformed.
    MalformedHeader,
    /// Obsolete folded response headers are rejected.
    ObsoleteLineFolding,
    /// Multiple Content-Length values disagreed.
    ConflictingContentLength,
    /// Transfer-Encoding and Content-Length appeared together.
    AmbiguousBodyFraming,
    /// A Content-Length value was syntactically invalid or overflowed.
    InvalidContentLength,
    /// A transfer coding other than one strict `chunked` coding was received.
    UnsupportedTransferCoding(String),
    /// A content coding requiring a decoder was received.
    UnsupportedContentCoding(String),
    /// The chunk-size line or a chunk extension was invalid.
    MalformedChunkSize,
    /// A prohibited field appeared in the trailer section.
    ProhibitedTrailer(String),
    /// The peer closed before a declared HTTP unit was complete.
    PrematureEof,
    /// Protocol switching is outside this transport's scope.
    ProtocolSwitchUnsupported,
    /// The configured policy rejects exposing redirect responses.
    RedirectRejected(u16),
}

impl Error {
    pub(crate) fn io(operation: Operation, error: &io::Error) -> Self {
        if matches!(
            error.kind(),
            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
        ) {
            Self::Timeout(operation)
        } else {
            Self::Io {
                operation,
                kind: error.kind(),
            }
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidUrl(message) => write!(formatter, "invalid URL: {message}"),
            Self::UnsupportedScheme(scheme) => {
                write!(formatter, "unsupported URL scheme: {scheme}")
            }
            Self::CredentialsNotAllowed => formatter.write_str("URL credentials are not allowed"),
            Self::FragmentNotAllowed => formatter.write_str("URL fragments are not transport data"),
            Self::NonLoopbackTarget => {
                formatter.write_str("target is not a numeric loopback address")
            }
            Self::MissingPort => formatter.write_str("target has no usable TCP port"),
            Self::InvalidRequestTarget => formatter.write_str("invalid HTTP request target"),
            Self::InvalidMethod => formatter.write_str("invalid HTTP method"),
            Self::InvalidHeaderName => formatter.write_str("invalid HTTP header name"),
            Self::InvalidHeaderValue => formatter.write_str("invalid HTTP header value"),
            Self::ReservedRequestHeader(name) => {
                write!(
                    formatter,
                    "request header is owned by the transport: {name}"
                )
            }
            Self::LimitExceeded { kind, limit } => {
                write!(formatter, "{kind:?} limit exceeded (maximum {limit})")
            }
            Self::Cancelled => formatter.write_str("operation cancelled"),
            Self::Timeout(operation) => write!(formatter, "{operation:?} timed out"),
            Self::Io { operation, kind } => {
                write!(formatter, "{operation:?} failed with {kind:?}")
            }
            Self::InvalidLineEnding => formatter.write_str("invalid HTTP line ending"),
            Self::MalformedStatusLine => formatter.write_str("malformed HTTP status line"),
            Self::MalformedHeader => formatter.write_str("malformed HTTP response header"),
            Self::ObsoleteLineFolding => {
                formatter.write_str("obsolete folded response header rejected")
            }
            Self::ConflictingContentLength => {
                formatter.write_str("conflicting Content-Length values")
            }
            Self::AmbiguousBodyFraming => {
                formatter.write_str("response contains Transfer-Encoding and Content-Length")
            }
            Self::InvalidContentLength => formatter.write_str("invalid Content-Length value"),
            Self::UnsupportedTransferCoding(value) => {
                write!(formatter, "unsupported Transfer-Encoding: {value}")
            }
            Self::UnsupportedContentCoding(value) => {
                write!(formatter, "unsupported Content-Encoding: {value}")
            }
            Self::MalformedChunkSize => formatter.write_str("malformed chunk-size line"),
            Self::ProhibitedTrailer(name) => write!(formatter, "prohibited trailer field: {name}"),
            Self::PrematureEof => {
                formatter.write_str("peer closed before the message was complete")
            }
            Self::ProtocolSwitchUnsupported => {
                formatter.write_str("HTTP protocol switching is unsupported")
            }
            Self::RedirectRejected(status) => {
                write!(formatter, "redirect response rejected by policy: {status}")
            }
        }
    }
}

impl std::error::Error for Error {}
