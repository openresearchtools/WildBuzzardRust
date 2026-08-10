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
    /// Bytes in a serialized general-web URL.
    UrlBytes,
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
    /// Number of IP-address candidates returned by DNS.
    DnsCandidates,
    /// Number of TCP candidates attempted for one request.
    ConnectionAttempts,
    /// Aggregate TLS handshake bytes read and written.
    TlsHandshakeBytes,
}

/// A network operation associated with an I/O failure or timeout.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum Operation {
    /// Resolving a DNS host into bounded address candidates.
    ResolveDns,
    /// Establishing a TCP connection to an admitted address.
    Connect,
    /// Writing the HTTP request.
    WriteRequest,
    /// Reading the response status and headers.
    ReadHead,
    /// Reading the response body.
    ReadBody,
    /// Authenticating and negotiating a TLS connection.
    TlsHandshake,
}

/// Stable DNS failure classifications exposed by the general-web client.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum DnsFailure {
    /// The system resolver configuration could not be loaded.
    Configuration,
    /// The supplied normalized host could not be represented as a DNS name.
    InvalidName,
    /// The name does not exist or has no usable A/AAAA records.
    NoRecords,
    /// The resolver reported a protocol, I/O, or internal failure.
    Lookup,
    /// The private resolver runtime could not be created or entered.
    Runtime,
    /// Another operation panicked while holding the resolver runtime.
    RuntimePoisoned,
}

/// Stable certificate-validation failures from the authenticated TLS path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum CertificateFailure {
    /// The certificate is not correctly encoded.
    BadEncoding,
    /// The certificate is no longer valid at the current time.
    Expired,
    /// The certificate is not valid yet at the current time.
    NotValidYet,
    /// The chain does not terminate at a configured trust anchor.
    UnknownIssuer,
    /// The leaf certificate does not authenticate the requested host.
    NotValidForName,
    /// The certificate has been reported revoked by configured verifier data.
    Revoked,
    /// The certificate's signature is invalid.
    BadSignature,
    /// The certificate is not valid for TLS server authentication.
    InvalidPurpose,
    /// The certificate contains an unsupported critical extension.
    UnhandledCriticalExtension,
    /// The certificate uses an unsupported signature algorithm.
    UnsupportedSignatureAlgorithm,
    /// Certificate validation failed for another fail-closed reason.
    Other,
}

/// Stable TLS setup, authentication, and protocol failure classifications.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum TlsFailure {
    /// The TLS configuration could not be constructed from secure defaults.
    Configuration,
    /// The requested host could not be represented as a TLS server name.
    InvalidServerName,
    /// The peer certificate chain failed verification.
    InvalidCertificate(CertificateFailure),
    /// The peer did not present a certificate chain.
    NoCertificatesPresented,
    /// The peer does not support the required TLS version or feature set.
    PeerIncompatible,
    /// The peer violated the TLS protocol.
    PeerMisbehaved,
    /// TLS record processing or cryptography failed closed.
    Protocol,
    /// The handshake did not publish TLS 1.2 or TLS 1.3.
    UnsupportedVersion,
    /// The peer selected an application protocol other than HTTP/1.1.
    UnsupportedApplicationProtocol,
}

/// Stable trust-store construction failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum TrustStoreFailure {
    /// An explicitly added DER trust anchor was malformed or unsupported.
    InvalidCertificate,
    /// The resulting trust store contained no usable anchors.
    Empty,
}

/// Structured failures produced while validating or transporting HTTP.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum Error {
    /// The supplied URL could not be parsed as a WHATWG URL.
    InvalidUrl(String),
    /// The URL scheme is unsupported by the selected transport.
    UnsupportedScheme(String),
    /// Credentials are not accepted at the transport boundary.
    CredentialsNotAllowed,
    /// URL fragments must be removed by the caller before transport.
    FragmentNotAllowed,
    /// The target was not a numeric loopback IP address.
    NonLoopbackTarget,
    /// The target URL did not contain a usable TCP port.
    MissingPort,
    /// The target URL did not contain a host.
    MissingHost,
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
    /// DNS resolution failed before any connection attempt.
    Dns(DnsFailure),
    /// Every admitted TCP address candidate failed.
    ConnectAttemptsExhausted {
        /// Number of candidates actually attempted.
        attempted: usize,
        /// Stable final socket failure classification, if one was available.
        last_kind: Option<io::ErrorKind>,
    },
    /// Trust-store construction failed closed.
    TrustStore(TrustStoreFailure),
    /// TLS setup, authentication, or protocol processing failed closed.
    Tls(TlsFailure),
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
        if let Some(error) = error
            .get_ref()
            .and_then(|source| source.downcast_ref::<rustls::Error>())
        {
            return Self::Tls(classify_rustls_error(error));
        }
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

pub(crate) fn classify_rustls_error(error: &rustls::Error) -> TlsFailure {
    use rustls::Error as RustlsError;

    match error {
        RustlsError::InvalidCertificate(error) => {
            TlsFailure::InvalidCertificate(classify_certificate_error(error))
        }
        RustlsError::NoCertificatesPresented => TlsFailure::NoCertificatesPresented,
        RustlsError::PeerIncompatible(_) => TlsFailure::PeerIncompatible,
        RustlsError::PeerMisbehaved(_) => TlsFailure::PeerMisbehaved,
        RustlsError::UnsupportedNameType => TlsFailure::InvalidServerName,
        RustlsError::NoApplicationProtocol => TlsFailure::UnsupportedApplicationProtocol,
        RustlsError::AlertReceived(rustls::AlertDescription::NoApplicationProtocol) => {
            TlsFailure::UnsupportedApplicationProtocol
        }
        RustlsError::AlertReceived(rustls::AlertDescription::ProtocolVersion) => {
            TlsFailure::UnsupportedVersion
        }
        _ => TlsFailure::Protocol,
    }
}

fn classify_certificate_error(error: &rustls::CertificateError) -> CertificateFailure {
    use rustls::CertificateError as RustlsCertificateError;

    match error {
        RustlsCertificateError::BadEncoding => CertificateFailure::BadEncoding,
        RustlsCertificateError::Expired | RustlsCertificateError::ExpiredContext { .. } => {
            CertificateFailure::Expired
        }
        RustlsCertificateError::NotValidYet | RustlsCertificateError::NotValidYetContext { .. } => {
            CertificateFailure::NotValidYet
        }
        RustlsCertificateError::UnknownIssuer => CertificateFailure::UnknownIssuer,
        RustlsCertificateError::NotValidForName
        | RustlsCertificateError::NotValidForNameContext { .. } => {
            CertificateFailure::NotValidForName
        }
        RustlsCertificateError::Revoked => CertificateFailure::Revoked,
        RustlsCertificateError::BadSignature => CertificateFailure::BadSignature,
        RustlsCertificateError::InvalidPurpose
        | RustlsCertificateError::InvalidPurposeContext { .. } => {
            CertificateFailure::InvalidPurpose
        }
        RustlsCertificateError::UnhandledCriticalExtension => {
            CertificateFailure::UnhandledCriticalExtension
        }
        RustlsCertificateError::UnsupportedSignatureAlgorithmContext { .. }
        | RustlsCertificateError::UnsupportedSignatureAlgorithmForPublicKeyContext { .. } => {
            CertificateFailure::UnsupportedSignatureAlgorithm
        }
        _ => CertificateFailure::Other,
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
            Self::MissingHost => formatter.write_str("target URL has no host"),
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
            Self::Dns(failure) => write!(formatter, "DNS resolution failed: {failure:?}"),
            Self::ConnectAttemptsExhausted {
                attempted,
                last_kind,
            } => write!(
                formatter,
                "all {attempted} connection attempts failed (last error: {last_kind:?})"
            ),
            Self::TrustStore(failure) => {
                write!(formatter, "trust-store construction failed: {failure:?}")
            }
            Self::Tls(failure) => write!(formatter, "TLS failed: {failure:?}"),
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
