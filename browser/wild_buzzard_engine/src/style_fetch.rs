// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Bounded transport and response admission for external stylesheet plans.
//!
//! This owner is deliberately separate from [`crate::StyleResourcePlan`]. The
//! plan remains immutable capability-free data; this module owns the explicit
//! general-web capability and returns only bounded response data. It does not
//! parse CSS, mutate a document, publish a frame, attach credentials or
//! cookies, perform CORS or SRI, or send CSP reports.

use std::{
    fmt,
    net::{IpAddr, Ipv6Addr},
    time::{Duration, Instant},
};

use wild_buzzard_dom::{DocumentVersion, NodeId};
use wild_buzzard_net::{
    Body, BodyFraming, CancellationToken, ClientConfig, ConnectionSecurity, Error as NetworkError,
    GeneralWebClient, GeneralWebConfig, GeneralWebExecutionError, GeneralWebNetworkAccess,
    GeneralWebPolicyError, GeneralWebRequest, GeneralWebResponse, GeneralWebTarget,
    GeneralWebTransportFailure, Headers, LimitKind, LocalNetworkAccessPermissions, RedirectPolicy,
    WebHost, WebOrigin, WebScheme,
};

use crate::navigation::{StyleDocumentAccessError, StyleDocumentFetchCapability};
use crate::{
    MAX_STYLE_RESOURCE_URL_BYTES, NavigationCommitMetadata, NavigationId,
    StyleResourceCandidateStatus, StyleResourcePlan, StyleResourceRequestIdentity,
};

/// Maximum final stylesheet responses retained by one fetch transaction.
pub const MAX_STYLE_FETCH_RESPONSES: usize = 64;
/// Maximum decoded bytes retained for one stylesheet response.
pub const MAX_STYLE_FETCH_RESPONSE_BODY_BYTES: usize = 2 * 1024 * 1024;
/// Maximum decoded stylesheet bytes retained by one fetch transaction.
pub const MAX_STYLE_FETCH_AGGREGATE_BODY_BYTES: usize = 16 * 1024 * 1024;
/// Maximum redirects followed for one admitted stylesheet identity.
pub const MAX_STYLE_FETCH_REDIRECTS: usize = 8;
/// Maximum HTTP exchanges, including redirect responses, in one transaction.
pub const MAX_STYLE_FETCH_HTTP_EXCHANGES: usize = 256;
/// Maximum redacted diagnostics retained by one transaction or failure.
pub const MAX_STYLE_FETCH_DIAGNOSTICS: usize = 1024;
/// Maximum bytes retained from one comma-merged `Content-Type` value.
pub const MAX_STYLE_FETCH_CONTENT_TYPE_BYTES: usize = 4 * 1024;
/// Maximum retained `Content-Type` bytes across one transaction.
pub const MAX_STYLE_FETCH_AGGREGATE_HEADER_BYTES: usize = 256 * 1024;
/// Maximum wall-clock horizon accepted from a caller for one transaction.
pub const MAX_STYLE_FETCH_DURATION: Duration = Duration::from_secs(30);
/// Maximum accepted HTTP chunk-size line, excluding its terminating CRLF.
pub const MAX_STYLE_FETCH_CHUNK_LINE_BYTES: usize = 8 * 1024;

const MAX_STYLE_FETCH_WIRE_HEADER_BYTES: usize = 64 * 1024;
const MAX_STYLE_FETCH_WIRE_HEADER_FIELDS: usize = 256;
const MAX_STYLE_FETCH_WIRE_BODY_BYTES: usize = 8 * 1024 * 1024;
const MAX_STYLE_FETCH_REQUEST_HEAD_BYTES: usize = 64 * 1024;
const MAX_STYLE_FETCH_REQUEST_HEADER_FIELDS: usize = 0;
const MAX_STYLE_FETCH_REQUEST_BODY_BYTES: usize = 0;
const MAX_STYLE_FETCH_INFORMATIONAL_RESPONSES: usize = 8;
const MAX_STYLE_FETCH_DNS_CANDIDATES: usize = 32;
const MAX_STYLE_FETCH_CONNECTION_ATTEMPTS: usize = 16;
const MAX_STYLE_FETCH_TLS_HANDSHAKE_BYTES: usize = 1024 * 1024;
const STYLE_FETCH_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const STYLE_FETCH_READ_TIMEOUT: Duration = Duration::from_secs(5);
const STYLE_FETCH_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const STYLE_FETCH_DNS_TIMEOUT: Duration = Duration::from_secs(5);
const STYLE_FETCH_TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const BODY_READ_CHUNK_BYTES: usize = 16 * 1024;

/// Sealed HTTP transport policy accepted by [`StyleFetchOwner`].
///
/// The owner does not accept an ambient [`GeneralWebConfig`], because the
/// network API does not expose every `ClientConfig` parser limit for
/// validation. All fields are instead constructed here at owner-controlled
/// values. The chunk-line bound is the sole caller-narrowable transport field.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StyleFetchTransportPolicy {
    max_chunk_line_bytes: usize,
}

impl StyleFetchTransportPolicy {
    /// Creates a transport policy with a lower-or-equal chunk-line bound.
    ///
    /// # Errors
    ///
    /// Returns a typed error when `max_chunk_line_bytes` would exceed the
    /// owner's hard parser bound.
    pub const fn new(max_chunk_line_bytes: usize) -> Result<Self, StyleFetchOwnerError> {
        if max_chunk_line_bytes > MAX_STYLE_FETCH_CHUNK_LINE_BYTES {
            return Err(StyleFetchOwnerError::LimitWouldEnlarge(
                StyleFetchLimit::WireChunkLineBytes,
            ));
        }
        Ok(Self {
            max_chunk_line_bytes,
        })
    }

    /// Returns the maximum chunk-size line bytes, excluding CRLF.
    #[must_use]
    pub const fn max_chunk_line_bytes(self) -> usize {
        self.max_chunk_line_bytes
    }
}

impl Default for StyleFetchTransportPolicy {
    fn default() -> Self {
        Self {
            max_chunk_line_bytes: MAX_STYLE_FETCH_CHUNK_LINE_BYTES,
        }
    }
}

/// Caller-selectable limits which may only narrow the owner hard bounds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StyleFetchLimits {
    responses: usize,
    response_body_bytes: usize,
    aggregate_body_bytes: usize,
    redirects: usize,
    http_exchanges: usize,
    diagnostics: usize,
}

impl StyleFetchLimits {
    /// Creates a lower-or-equal bounded policy.
    ///
    /// # Errors
    ///
    /// Returns a typed error if any value would enlarge an owner hard bound.
    pub const fn new(
        max_responses: usize,
        max_response_body_bytes: usize,
        max_aggregate_body_bytes: usize,
        max_redirects: usize,
        max_http_exchanges: usize,
        max_diagnostics: usize,
    ) -> Result<Self, StyleFetchOwnerError> {
        if max_responses > MAX_STYLE_FETCH_RESPONSES {
            return Err(StyleFetchOwnerError::LimitWouldEnlarge(
                StyleFetchLimit::Responses,
            ));
        }
        if max_response_body_bytes > MAX_STYLE_FETCH_RESPONSE_BODY_BYTES {
            return Err(StyleFetchOwnerError::LimitWouldEnlarge(
                StyleFetchLimit::ResponseBodyBytes,
            ));
        }
        if max_aggregate_body_bytes > MAX_STYLE_FETCH_AGGREGATE_BODY_BYTES {
            return Err(StyleFetchOwnerError::LimitWouldEnlarge(
                StyleFetchLimit::AggregateBodyBytes,
            ));
        }
        if max_redirects > MAX_STYLE_FETCH_REDIRECTS {
            return Err(StyleFetchOwnerError::LimitWouldEnlarge(
                StyleFetchLimit::Redirects,
            ));
        }
        if max_http_exchanges > MAX_STYLE_FETCH_HTTP_EXCHANGES {
            return Err(StyleFetchOwnerError::LimitWouldEnlarge(
                StyleFetchLimit::HttpExchanges,
            ));
        }
        if max_diagnostics > MAX_STYLE_FETCH_DIAGNOSTICS {
            return Err(StyleFetchOwnerError::LimitWouldEnlarge(
                StyleFetchLimit::Diagnostics,
            ));
        }
        Ok(Self {
            responses: max_responses,
            response_body_bytes: max_response_body_bytes,
            aggregate_body_bytes: max_aggregate_body_bytes,
            redirects: max_redirects,
            http_exchanges: max_http_exchanges,
            diagnostics: max_diagnostics,
        })
    }

    /// Maximum retained final responses.
    #[must_use]
    pub const fn max_responses(self) -> usize {
        self.responses
    }

    /// Maximum decoded bytes in one retained body.
    #[must_use]
    pub const fn max_response_body_bytes(self) -> usize {
        self.response_body_bytes
    }

    /// Maximum aggregate decoded retained body bytes.
    #[must_use]
    pub const fn max_aggregate_body_bytes(self) -> usize {
        self.aggregate_body_bytes
    }

    /// Maximum redirects per request identity.
    #[must_use]
    pub const fn max_redirects(self) -> usize {
        self.redirects
    }

    /// Maximum aggregate HTTP exchanges.
    #[must_use]
    pub const fn max_http_exchanges(self) -> usize {
        self.http_exchanges
    }

    /// Maximum retained diagnostics.
    #[must_use]
    pub const fn max_diagnostics(self) -> usize {
        self.diagnostics
    }
}

impl Default for StyleFetchLimits {
    fn default() -> Self {
        Self {
            responses: MAX_STYLE_FETCH_RESPONSES,
            response_body_bytes: MAX_STYLE_FETCH_RESPONSE_BODY_BYTES,
            aggregate_body_bytes: MAX_STYLE_FETCH_AGGREGATE_BODY_BYTES,
            redirects: MAX_STYLE_FETCH_REDIRECTS,
            http_exchanges: MAX_STYLE_FETCH_HTTP_EXCHANGES,
            diagnostics: MAX_STYLE_FETCH_DIAGNOSTICS,
        }
    }
}

/// Bounded owner or transport resource.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StyleFetchLimit {
    /// Final retained responses.
    Responses,
    /// Decoded bytes in one response body.
    ResponseBodyBytes,
    /// Aggregate decoded body bytes.
    AggregateBodyBytes,
    /// Redirects followed for one request.
    Redirects,
    /// Aggregate HTTP exchanges.
    HttpExchanges,
    /// Retained redacted diagnostics.
    Diagnostics,
    /// Bytes retained from one `Content-Type` field.
    ContentTypeBytes,
    /// Aggregate retained response-header bytes.
    AggregateHeaderBytes,
    /// Transport response-header bytes.
    WireHeaderBytes,
    /// Transport response-header fields.
    WireHeaderFields,
    /// Transport body-parser bytes.
    WireBodyBytes,
    /// Transport chunk-size line bytes.
    WireChunkLineBytes,
    /// DNS candidates.
    DnsCandidates,
    /// Connection attempts.
    ConnectionAttempts,
    /// TLS handshake bytes.
    TlsHandshakeBytes,
}

/// Failure to construct the bounded network owner.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StyleFetchOwnerError {
    /// A configurable value attempted to enlarge a hard bound.
    LimitWouldEnlarge(StyleFetchLimit),
    /// The fixed transport profile violated its versioned network bounds.
    TransportPolicyInvalid,
    /// No exact committed-document response authority could be delegated.
    AuthorityUnavailable,
    /// The exact live document already issued its sole stylesheet authority.
    AuthorityAlreadyIssued,
    /// Product authority requires an exact worker-bound navigation identity.
    ProductNavigationRequired,
    /// Non-product authority cannot be issued for a worker-bound document.
    NonProductDocumentRequired,
}

impl fmt::Display for StyleFetchOwnerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LimitWouldEnlarge(limit) => {
                write!(
                    formatter,
                    "stylesheet fetch {limit:?} limit would enlarge hard policy"
                )
            }
            Self::TransportPolicyInvalid => {
                formatter.write_str("stylesheet fetch transport policy is invalid")
            }
            Self::AuthorityUnavailable => {
                formatter.write_str("stylesheet fetch document authority is unavailable")
            }
            Self::AuthorityAlreadyIssued => {
                formatter.write_str("stylesheet fetch document authority was already issued")
            }
            Self::ProductNavigationRequired => {
                formatter.write_str("stylesheet fetch product navigation authority is required")
            }
            Self::NonProductDocumentRequired => {
                formatter.write_str("stylesheet fetch non-product document authority is required")
            }
        }
    }
}

impl std::error::Error for StyleFetchOwnerError {}

/// Retained response MIME classification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StyleFetchMime {
    /// A syntactically valid `text/css` essence, ASCII-case-insensitively.
    Css,
    /// Missing, empty, or syntactically invalid MIME metadata admitted only
    /// when `nosniff` was absent, matching Firefox's unknown-type path.
    Unknown,
}

/// Typed response headers required by a later CSS decoder/parser handoff.
pub struct StyleFetchResponseHeaders {
    content_type: Option<Vec<u8>>,
    charset: Option<Vec<u8>>,
    mime: StyleFetchMime,
    nosniff: bool,
}

impl StyleFetchResponseHeaders {
    /// Exact comma-merged `Content-Type` field bytes, when any were present.
    #[must_use]
    pub fn content_type(&self) -> Option<&[u8]> {
        self.content_type.as_deref()
    }

    /// Charset selected by Fetch MIME extraction, when one was selected.
    #[must_use]
    pub fn charset(&self) -> Option<&[u8]> {
        self.charset.as_deref()
    }

    /// Admitted MIME classification.
    #[must_use]
    pub const fn mime(&self) -> StyleFetchMime {
        self.mime
    }

    /// Whether the first comma-delimited merged XCTO value was `nosniff`.
    #[must_use]
    pub const fn nosniff(&self) -> bool {
        self.nosniff
    }

    /// Bytes retained for a later decoder/parser handoff.
    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        self.content_type
            .as_ref()
            .map_or(0, Vec::len)
            .saturating_add(self.charset.as_ref().map_or(0, Vec::len))
    }

    fn checked_retained_bytes(&self) -> Result<usize, StyleFetchRejection> {
        self.content_type
            .as_ref()
            .map_or(0, Vec::len)
            .checked_add(self.charset.as_ref().map_or(0, Vec::len))
            .ok_or(StyleFetchRejection::CounterOverflow)
    }
}

impl fmt::Debug for StyleFetchResponseHeaders {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StyleFetchResponseHeaders")
            .field(
                "content_type_bytes",
                &self.content_type.as_ref().map_or(0, Vec::len),
            )
            .field("charset_bytes", &self.charset.as_ref().map_or(0, Vec::len))
            .field("mime", &self.mime)
            .field("nosniff", &self.nosniff)
            .finish_non_exhaustive()
    }
}

/// CSSOM visibility of one admitted stylesheet response.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StyleFetchOriginCleanliness {
    /// The document origin subsumes the final response and every redirect hop
    /// preserved the previous origin.
    Clean,
    /// CSSOM rules must treat the sheet as cross-origin.
    Tainted,
}

impl StyleFetchOriginCleanliness {
    /// Whether CSSOM may expose rules from this response.
    #[must_use]
    pub const fn is_clean(self) -> bool {
        matches!(self, Self::Clean)
    }
}

/// One fully admitted external stylesheet response in document order.
pub struct StyleFetchResponse {
    document_version: DocumentVersion,
    owner: NodeId,
    request_index: usize,
    final_url: String,
    redirect_count: usize,
    origin_cleanliness: StyleFetchOriginCleanliness,
    status: u16,
    security: ConnectionSecurity,
    headers: StyleFetchResponseHeaders,
    body: Vec<u8>,
}

impl StyleFetchResponse {
    /// Exact immutable document revision which owns this response.
    #[must_use]
    pub const fn document_version(&self) -> DocumentVersion {
        self.document_version
    }

    /// Exact owning HTML link node.
    #[must_use]
    pub const fn owner(&self) -> NodeId {
        self.owner
    }

    /// Exact index in the source plan's request slice.
    #[must_use]
    pub const fn request_index(&self) -> usize {
        self.request_index
    }

    /// Canonical credential-free fragment-free final HTTP(S) identity.
    #[must_use]
    pub fn final_url(&self) -> &str {
        &self.final_url
    }

    /// Number of admitted network redirects.
    #[must_use]
    pub const fn redirect_count(&self) -> usize {
        self.redirect_count
    }

    /// Typed CSSOM origin-clean or tainted result for this exact chain.
    #[must_use]
    pub const fn origin_cleanliness(&self) -> StyleFetchOriginCleanliness {
        self.origin_cleanliness
    }

    /// Final successful HTTP status.
    #[must_use]
    pub const fn status(&self) -> u16 {
        self.status
    }

    /// Exact cleartext or authenticated-TLS evidence for the final response.
    #[must_use]
    pub const fn security(&self) -> ConnectionSecurity {
        self.security
    }

    /// Bounded response headers needed by a later CSS decoder/parser.
    #[must_use]
    pub const fn headers(&self) -> &StyleFetchResponseHeaders {
        &self.headers
    }

    /// Complete bounded decoded response body.
    #[must_use]
    pub fn body(&self) -> &[u8] {
        &self.body
    }
}

impl fmt::Debug for StyleFetchResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StyleFetchResponse")
            .field("document_version", &self.document_version)
            .field("owner", &self.owner)
            .field("request_index", &self.request_index)
            .field("final_url_bytes", &self.final_url.len())
            .field("redirect_count", &self.redirect_count)
            .field("origin_cleanliness", &self.origin_cleanliness)
            .field("status", &self.status)
            .field("security", &self.security)
            .field("headers", &self.headers)
            .field("body_bytes", &self.body.len())
            .finish_non_exhaustive()
    }
}

/// Redacted diagnostic category retained in exact operation order.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StyleFetchDiagnosticKind {
    /// The plan's report-only policy would block the initial request.
    ReportOnlyWouldBlock {
        /// Number of report-only policies which would block.
        policies: usize,
    },
    /// One network redirect passed all pre-connection gates.
    RedirectFollowed {
        /// Redirect response status.
        status: u16,
    },
    /// Missing, empty, or malformed MIME metadata was accepted without nosniff.
    UnknownMimeAccepted,
    /// The transaction failed; the category contains no peer text.
    Rejected(StyleFetchRejection),
}

/// One privacy-safe diagnostic bound to an exact plan request and hop.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StyleFetchDiagnostic {
    document_version: DocumentVersion,
    owner: NodeId,
    request_index: usize,
    redirect_index: usize,
    kind: StyleFetchDiagnosticKind,
}

impl StyleFetchDiagnostic {
    /// Exact owning document revision.
    #[must_use]
    pub const fn document_version(self) -> DocumentVersion {
        self.document_version
    }

    /// Exact owning link node. Plan-level failures have no diagnostic record.
    #[must_use]
    pub const fn owner(self) -> NodeId {
        self.owner
    }

    /// Exact source-plan request index.
    #[must_use]
    pub const fn request_index(self) -> usize {
        self.request_index
    }

    /// Number of redirects admitted before this observation.
    #[must_use]
    pub const fn redirect_index(self) -> usize {
        self.redirect_index
    }

    /// Redacted diagnostic category.
    #[must_use]
    pub const fn kind(self) -> StyleFetchDiagnosticKind {
        self.kind
    }
}

/// Stable redacted rejection reason.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StyleFetchRejection {
    /// The plan's document/request topology was contradictory.
    PlanOwnership,
    /// The owning response document is no longer the exact current document.
    DocumentNotCurrent,
    /// The document's sole stylesheet transaction was already consumed.
    TransactionConsumed,
    /// The document's sole stylesheet transaction is already active.
    TransactionInProgress,
    /// Cancellation was observed at a safe checkpoint.
    Cancelled,
    /// The absolute operation deadline elapsed.
    DeadlineExceeded,
    /// The caller requested a deadline beyond the hard operation horizon.
    DeadlineTooFar,
    /// The transport failed without exposing peer-controlled detail.
    Network(StyleFetchNetworkFailure),
    /// Response connection evidence contradicted the requested scheme.
    TransportSecurityMismatch,
    /// A redirect omitted `Location`.
    RedirectLocationMissing,
    /// Repeated `Location` fields differed after HTTP whitespace trimming.
    RedirectLocationConflict,
    /// A redirect location was not UTF-8.
    RedirectLocationNonUtf8,
    /// A redirect location could not form an HTTP(S) target.
    RedirectLocationInvalid,
    /// A redirect selected a non-HTTP(S) scheme.
    RedirectScheme,
    /// A redirect target contained credentials.
    RedirectCredentials,
    /// A redirect target exceeded the URL bound.
    RedirectUrlBytes,
    /// A redirect target repeated an already visited transport identity.
    RedirectLoop,
    /// A redirect target was mixed content for the owning HTTPS document.
    MixedContent,
    /// The immutable plan cannot prove policy continuity across origins.
    CrossOriginPolicyUnproven,
    /// A 3xx response was not a Fetch redirect status.
    UnsupportedRedirectStatus,
    /// A final response was not successful.
    HttpStatus,
    /// Explicit response MIME metadata was not CSS.
    MimeNotCss,
    /// `nosniff` rejected missing or invalid CSS MIME metadata.
    NoSniff,
    /// A fixed count or byte limit was exceeded.
    Limit(StyleFetchLimit),
    /// Checked accounting overflowed.
    CounterOverflow,
    /// A retained allocation could not be reserved.
    AllocationFailed,
}

/// Privacy-safe transport failure family.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StyleFetchNetworkFailure {
    /// The exact target used a Fetch/Firefox restricted port.
    RestrictedPort,
    /// The exact committed-document address-space evidence was invalid.
    InitiatorEvidence,
    /// Local Network Access policy denied a more-private candidate.
    LocalNetworkAccess,
    /// DNS failed.
    Dns,
    /// Connection establishment or socket I/O failed.
    Connection,
    /// TLS setup, authentication, or protocol processing failed.
    Tls,
    /// The HTTP parser or framing layer rejected the response.
    HttpProtocol,
    /// A transport-owned resource limit was reached.
    ResourceLimit,
    /// Another fail-closed transport error occurred.
    Other,
}

/// Exact redacted failure point for a fetch transaction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StyleFetchError {
    document_version: DocumentVersion,
    owner: Option<NodeId>,
    request_index: usize,
    redirect_index: usize,
    rejection: StyleFetchRejection,
}

impl StyleFetchError {
    /// Exact owning document revision.
    #[must_use]
    pub const fn document_version(self) -> DocumentVersion {
        self.document_version
    }

    /// Exact owning link node.
    #[must_use]
    pub const fn owner(self) -> Option<NodeId> {
        self.owner
    }

    /// Exact source-plan request index.
    #[must_use]
    pub const fn request_index(self) -> usize {
        self.request_index
    }

    /// Redirect count completed before failure.
    #[must_use]
    pub const fn redirect_index(self) -> usize {
        self.redirect_index
    }

    /// Stable redacted rejection category.
    #[must_use]
    pub const fn rejection(self) -> StyleFetchRejection {
        self.rejection
    }
}

impl fmt::Display for StyleFetchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "stylesheet fetch request {} hop {} rejected: {:?}",
            self.request_index, self.redirect_index, self.rejection
        )
    }
}

impl std::error::Error for StyleFetchError {}

/// Failed transaction containing diagnostics but never response state.
pub struct StyleFetchFailure {
    error: StyleFetchError,
    diagnostics: Vec<StyleFetchDiagnostic>,
}

impl StyleFetchFailure {
    /// Exact redacted terminal failure.
    #[must_use]
    pub const fn error(&self) -> StyleFetchError {
        self.error
    }

    /// Bounded redacted observations made before terminal failure.
    #[must_use]
    pub fn diagnostics(&self) -> &[StyleFetchDiagnostic] {
        &self.diagnostics
    }
}

impl fmt::Debug for StyleFetchFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StyleFetchFailure")
            .field("error", &self.error)
            .field("diagnostics", &self.diagnostics)
            .finish()
    }
}

impl fmt::Display for StyleFetchFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.error.fmt(formatter)
    }
}

impl std::error::Error for StyleFetchFailure {}

/// Complete all-or-nothing response set in deterministic document order.
pub struct StyleFetchSet {
    document_version: DocumentVersion,
    navigation_commit: NavigationCommitMetadata,
    responses: Vec<StyleFetchResponse>,
    diagnostics: Vec<StyleFetchDiagnostic>,
    aggregate_body_bytes: usize,
    aggregate_header_bytes: usize,
    http_exchanges: usize,
}

impl StyleFetchSet {
    /// Exact immutable document revision represented by every response.
    #[must_use]
    pub const fn document_version(&self) -> DocumentVersion {
        self.document_version
    }

    /// Exact document response commitment from the source plan.
    #[must_use]
    pub const fn navigation_commit(&self) -> &NavigationCommitMetadata {
        &self.navigation_commit
    }

    /// Fully admitted responses in source-plan/document order.
    #[must_use]
    pub fn responses(&self) -> &[StyleFetchResponse] {
        &self.responses
    }

    /// Bounded redacted observations in operation order.
    ///
    /// Report-only policy observations are non-authoritative and may be
    /// omitted once the diagnostic limit is exhausted.
    #[must_use]
    pub fn diagnostics(&self) -> &[StyleFetchDiagnostic] {
        &self.diagnostics
    }

    /// Aggregate decoded body bytes retained by this set.
    #[must_use]
    pub const fn aggregate_body_bytes(&self) -> usize {
        self.aggregate_body_bytes
    }

    /// Aggregate response-header bytes retained by this set.
    #[must_use]
    pub const fn aggregate_header_bytes(&self) -> usize {
        self.aggregate_header_bytes
    }

    /// HTTP exchanges, including redirect responses.
    #[must_use]
    pub const fn http_exchanges(&self) -> usize {
        self.http_exchanges
    }
}

impl fmt::Debug for StyleFetchSet {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StyleFetchSet")
            .field("document_version", &self.document_version)
            .field("redirect_count", &self.navigation_commit.redirect_count())
            .field("responses", &self.responses)
            .field("diagnostics", &self.diagnostics)
            .field("aggregate_body_bytes", &self.aggregate_body_bytes)
            .field("aggregate_header_bytes", &self.aggregate_header_bytes)
            .field("http_exchanges", &self.http_exchanges)
            .finish_non_exhaustive()
    }
}

struct StyleFetchAuthorityCore {
    client: GeneralWebClient,
    network_access: GeneralWebNetworkAccess,
    document_version: DocumentVersion,
    navigation_commit: NavigationCommitMetadata,
    lifecycle: StyleDocumentFetchCapability,
}

#[derive(Clone, Copy)]
enum StyleFetchScope {
    Product(NavigationId),
    NonProduct,
}

/// Product stylesheet authority for one exact live worker navigation.
///
/// Only an exact response/document commitment already bound to a non-optional
/// [`NavigationId`] can issue this value. It retains a child transport with the
/// original navigation client's unforgeable identity and the document's sole
/// revocable stylesheet ledger issuance. It is neither [`Clone`] nor [`Copy`].
///
/// ```compile_fail
/// use wild_buzzard_engine::StyleFetchAuthority;
///
/// fn cannot_copy(authority: StyleFetchAuthority) {
///     let first = authority;
///     let second = authority;
///     drop((first, second));
/// }
/// ```
pub struct StyleFetchAuthority(StyleFetchAuthorityCore);

impl StyleFetchAuthority {
    pub(crate) fn from_committed_document(
        source_client: &GeneralWebClient,
        navigation_commit: &NavigationCommitMetadata,
        navigation: NavigationId,
        document_version: DocumentVersion,
        transport_policy: StyleFetchTransportPolicy,
    ) -> Result<Self, StyleFetchOwnerError> {
        prepare_style_fetch_authority(
            source_client,
            navigation_commit,
            document_version,
            transport_policy,
            StyleFetchScope::Product(navigation),
        )
        .map(Self)
    }
}

impl fmt::Debug for StyleFetchAuthority {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StyleFetchAuthority")
            .field("document_version", &self.0.document_version)
            .finish_non_exhaustive()
    }
}

/// Explicit non-product authority for deterministic direct pipeline tests.
///
/// This type can be issued only while the exact direct document has no bound
/// [`NavigationId`]. It cannot be converted into [`StyleFetchAuthority`].
///
/// ```compile_fail
/// use wild_buzzard_engine::{NonProductStyleFetchAuthority, StyleFetchAuthority};
///
/// fn cannot_promote(authority: NonProductStyleFetchAuthority) {
///     let product: StyleFetchAuthority = authority.into();
///     drop(product);
/// }
/// ```
pub struct NonProductStyleFetchAuthority(StyleFetchAuthorityCore);

impl NonProductStyleFetchAuthority {
    pub(crate) fn from_committed_document(
        source_client: &GeneralWebClient,
        navigation_commit: &NavigationCommitMetadata,
        document_version: DocumentVersion,
        transport_policy: StyleFetchTransportPolicy,
    ) -> Result<Self, StyleFetchOwnerError> {
        prepare_style_fetch_authority(
            source_client,
            navigation_commit,
            document_version,
            transport_policy,
            StyleFetchScope::NonProduct,
        )
        .map(Self)
    }
}

impl fmt::Debug for NonProductStyleFetchAuthority {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NonProductStyleFetchAuthority")
            .field("document_version", &self.0.document_version)
            .finish_non_exhaustive()
    }
}

fn prepare_style_fetch_authority(
    source_client: &GeneralWebClient,
    navigation_commit: &NavigationCommitMetadata,
    document_version: DocumentVersion,
    transport_policy: StyleFetchTransportPolicy,
    scope: StyleFetchScope,
) -> Result<StyleFetchAuthorityCore, StyleFetchOwnerError> {
    match scope {
        StyleFetchScope::Product(navigation) => navigation_commit
            .validate_general_web_for_navigation(navigation, document_version)
            .map_err(|_| StyleFetchOwnerError::ProductNavigationRequired)?,
        StyleFetchScope::NonProduct => {
            navigation_commit
                .validate_general_web_for_subresources(document_version)
                .map_err(|_| StyleFetchOwnerError::AuthorityUnavailable)?;
            if navigation_commit.navigation().is_some() {
                return Err(StyleFetchOwnerError::NonProductDocumentRequired);
            }
        }
    }
    let response_authority = navigation_commit
        .committed_response_authority(document_version)
        .map_err(|_| StyleFetchOwnerError::AuthorityUnavailable)?;
    let config = sealed_transport_config(transport_policy)?;
    let client = source_client
        .delegate_for_response(response_authority, config)
        .map_err(|_| StyleFetchOwnerError::AuthorityUnavailable)?;
    let network_access = client
        .network_access_for_committed_response(
            response_authority,
            LocalNetworkAccessPermissions::deny_all(),
        )
        .map_err(|_| StyleFetchOwnerError::AuthorityUnavailable)?;
    let lifecycle = match scope {
        StyleFetchScope::Product(navigation) => {
            navigation_commit.issue_product_style_fetch(navigation, document_version)
        }
        StyleFetchScope::NonProduct => {
            navigation_commit.issue_non_product_style_fetch(document_version)
        }
    }
    .map_err(map_style_document_issuance_error)?;
    Ok(StyleFetchAuthorityCore {
        client,
        network_access,
        document_version,
        navigation_commit: navigation_commit.clone(),
        lifecycle,
    })
}

fn map_style_document_issuance_error(error: StyleDocumentAccessError) -> StyleFetchOwnerError {
    match error {
        StyleDocumentAccessError::Retired => StyleFetchOwnerError::AuthorityUnavailable,
        StyleDocumentAccessError::AlreadyIssued
        | StyleDocumentAccessError::TransactionActive
        | StyleDocumentAccessError::TransactionConsumed => {
            StyleFetchOwnerError::AuthorityAlreadyIssued
        }
        StyleDocumentAccessError::ProductNavigationRequired => {
            StyleFetchOwnerError::ProductNavigationRequired
        }
        StyleDocumentAccessError::NonProductNavigationBound => {
            StyleFetchOwnerError::NonProductDocumentRequired
        }
    }
}

fn map_style_document_transaction_error(error: StyleDocumentAccessError) -> StyleFetchRejection {
    match error {
        StyleDocumentAccessError::Retired => StyleFetchRejection::DocumentNotCurrent,
        StyleDocumentAccessError::TransactionConsumed => StyleFetchRejection::TransactionConsumed,
        StyleDocumentAccessError::TransactionActive => StyleFetchRejection::TransactionInProgress,
        StyleDocumentAccessError::AlreadyIssued
        | StyleDocumentAccessError::ProductNavigationRequired
        | StyleDocumentAccessError::NonProductNavigationBound => StyleFetchRejection::PlanOwnership,
    }
}

struct StyleFetchOwnerCore {
    client: GeneralWebClient,
    network_access: GeneralWebNetworkAccess,
    document_version: DocumentVersion,
    navigation_commit: NavigationCommitMetadata,
    lifecycle: StyleDocumentFetchCapability,
    limits: StyleFetchLimits,
}

/// One-shot product stylesheet owner for an exact current navigation.
///
/// The mutable transaction boundary prevents safe concurrent use in addition
/// to the shared per-document ledger which rejects duplicate issuance.
///
/// ```compile_fail
/// use wild_buzzard_engine::StyleFetchOwner;
///
/// fn cannot_borrow_twice(owner: &mut StyleFetchOwner) {
///     let first = &mut *owner;
///     let second = &mut *owner;
///     drop((first, second));
/// }
/// ```
pub struct StyleFetchOwner(StyleFetchOwnerCore);

impl StyleFetchOwner {
    /// Consumes one exact product document/navigation delegation.
    ///
    /// # Errors
    ///
    /// Returns a redacted typed error if the delegated response/document
    /// binding is no longer coherent. Transport limits were sealed while the
    /// delegation was issued.
    pub fn new(
        authority: StyleFetchAuthority,
        limits: StyleFetchLimits,
    ) -> Result<Self, StyleFetchOwnerError> {
        StyleFetchOwnerCore::new(authority.0, limits).map(Self)
    }

    /// Returns this owner's immutable lower-or-equal limits.
    #[must_use]
    pub const fn limits(&self) -> StyleFetchLimits {
        self.0.limits()
    }

    /// Consumes the exact document's sole stylesheet transaction.
    ///
    /// # Errors
    ///
    /// Returns a redacted all-or-nothing failure when the document is no longer
    /// current, the transaction was consumed, or any plan, policy, transport,
    /// response-admission, cancellation, deadline, or resource gate rejects.
    pub fn fetch_plan(
        &mut self,
        plan: &StyleResourcePlan,
        cancellation: &CancellationToken,
        deadline: Instant,
    ) -> Result<StyleFetchSet, StyleFetchFailure> {
        self.0.fetch_plan(plan, cancellation, deadline)
    }
}

/// One-shot owner for deterministic direct non-product fixtures.
///
/// It shares the same one-issuance, one-transaction ledger semantics as the
/// product owner but its authority type cannot cross the product boundary.
pub struct NonProductStyleFetchOwner(StyleFetchOwnerCore);

impl NonProductStyleFetchOwner {
    /// Consumes one exact non-product document delegation.
    ///
    /// # Errors
    ///
    /// Returns a redacted typed error if the delegated document ledger is no
    /// longer current or the supplied limits cannot be represented safely.
    pub fn new(
        authority: NonProductStyleFetchAuthority,
        limits: StyleFetchLimits,
    ) -> Result<Self, StyleFetchOwnerError> {
        StyleFetchOwnerCore::new(authority.0, limits).map(Self)
    }

    /// Returns this owner's immutable lower-or-equal limits.
    #[must_use]
    pub const fn limits(&self) -> StyleFetchLimits {
        self.0.limits()
    }

    /// Consumes the exact direct document's sole stylesheet transaction.
    ///
    /// # Errors
    ///
    /// Returns a redacted all-or-nothing failure under the same admission and
    /// one-shot ledger rules as [`StyleFetchOwner::fetch_plan`].
    pub fn fetch_plan(
        &mut self,
        plan: &StyleResourcePlan,
        cancellation: &CancellationToken,
        deadline: Instant,
    ) -> Result<StyleFetchSet, StyleFetchFailure> {
        self.0.fetch_plan(plan, cancellation, deadline)
    }
}

impl StyleFetchOwnerCore {
    fn new(
        authority: StyleFetchAuthorityCore,
        limits: StyleFetchLimits,
    ) -> Result<Self, StyleFetchOwnerError> {
        authority
            .lifecycle
            .ensure_current()
            .map_err(map_style_document_issuance_error)?;
        Ok(Self {
            client: authority.client,
            network_access: authority.network_access,
            document_version: authority.document_version,
            navigation_commit: authority.navigation_commit,
            lifecycle: authority.lifecycle,
            limits,
        })
    }

    /// Returns this owner's immutable lower-or-equal limits.
    #[must_use]
    pub const fn limits(&self) -> StyleFetchLimits {
        self.limits
    }

    /// Fetches every immutable plan request as one all-or-nothing transaction.
    ///
    /// Redirects are manual. Each target is parsed, credential/scheme/mixed-
    /// content checked, loop/deadline/cancellation bounded, and required to
    /// retain the initial request origin when enforcing CSP exists. The same-
    /// origin restriction is the only redirect-policy proof available from
    /// W9-A5N's deliberately nonce-free immutable identity; with enforcing CSP,
    /// cross-origin redirects fail closed rather than accepting transplantable
    /// policy input. With no enforcing policy, generic HTTP(S) redirects remain
    /// eligible after all other gates.
    ///
    /// The mutable receiver and shared document ledger are the serialization
    /// boundary. Exactly one call, including a cancelled, stale, invalid, or
    /// failed call, consumes the document's sole transaction. The supplied
    /// deadline must be no more than [`MAX_STYLE_FETCH_DURATION`] from entry.
    ///
    /// # Errors
    ///
    /// Returns a failure containing only redacted diagnostics, including for an
    /// expired or over-horizon deadline. No response, body, header, or URL state
    /// is returned unless every request succeeds.
    pub fn fetch_plan(
        &mut self,
        plan: &StyleResourcePlan,
        cancellation: &CancellationToken,
        deadline: Instant,
    ) -> Result<StyleFetchSet, StyleFetchFailure> {
        let requests = plan.requests();
        let fallback = requests.first().map_or_else(
            || ErrorPoint::plan(plan.document_version()),
            |request| ErrorPoint::request(0, request, 0),
        );
        let _transaction = self.lifecycle.begin_transaction().map_err(|error| {
            failure(
                fallback.error(map_style_document_transaction_error(error)),
                Vec::new(),
            )
        })?;
        if let Err(reason) = validate_operation_start(cancellation, deadline) {
            return Err(failure(fallback.error(reason), Vec::new()));
        }
        let mut diagnostics = try_vec_with_capacity(self.limits.diagnostics)
            .map_err(|reason| failure(fallback.error(reason), Vec::new()))?;
        let result = self.fetch_plan_inner(plan, cancellation, deadline, &mut diagnostics);
        match result {
            Ok(set) => Ok(set),
            Err(error) => {
                if let Some(owner) = error.owner {
                    let rejected = StyleFetchDiagnostic {
                        document_version: error.document_version,
                        owner,
                        request_index: error.request_index,
                        redirect_index: error.redirect_index,
                        kind: StyleFetchDiagnosticKind::Rejected(error.rejection),
                    };
                    push_authoritative_diagnostic_lossy(
                        &mut diagnostics,
                        rejected,
                        self.limits.diagnostics,
                    );
                }
                Err(StyleFetchFailure { error, diagnostics })
            }
        }
    }

    fn fetch_plan_inner(
        &self,
        plan: &StyleResourcePlan,
        cancellation: &CancellationToken,
        deadline: Instant,
        diagnostics: &mut Vec<StyleFetchDiagnostic>,
    ) -> Result<StyleFetchSet, StyleFetchError> {
        let requests = plan.requests();
        let point = requests.first().map_or_else(
            || ErrorPoint::plan(plan.document_version()),
            |request| ErrorPoint::request(0, request, 0),
        );
        checkpoint(cancellation, deadline, point)?;
        validate_plan_topology(plan).map_err(|reason| point.error(reason))?;
        enforce_count(
            requests.len(),
            self.limits.responses,
            StyleFetchLimit::Responses,
        )
        .map_err(|reason| point.error(reason))?;

        if plan.document_version() != self.document_version
            || plan.navigation_commit() != &self.navigation_commit
        {
            return Err(point.error(StyleFetchRejection::PlanOwnership));
        }

        let (_, document_target) =
            GeneralWebTarget::parse_navigation(plan.navigation_commit().final_url())
                .map_err(|_| point.error(StyleFetchRejection::PlanOwnership))?;
        let document_origin = document_target.origin().clone();
        let mut responses =
            try_vec_with_capacity(self.limits.responses).map_err(|reason| point.error(reason))?;
        let mut accounting = FetchAccounting::default();

        for (request_index, request) in requests.iter().enumerate() {
            let point = ErrorPoint::request(request_index, request, 0);
            checkpoint(cancellation, deadline, point)?;
            if request.document_version() != plan.document_version()
                || request.owner().document_id() != plan.document_version().document_id()
                || !request.policy_decision().is_allowed()
            {
                return Err(point.error(StyleFetchRejection::PlanOwnership));
            }
            if request.policy_decision().report_only_would_block() {
                push_non_enforcing_diagnostic_lossy(
                    diagnostics,
                    StyleFetchDiagnostic {
                        document_version: request.document_version(),
                        owner: request.owner(),
                        request_index,
                        redirect_index: 0,
                        kind: StyleFetchDiagnosticKind::ReportOnlyWouldBlock {
                            policies: request
                                .policy_decision()
                                .report_only_would_block_policy_count(),
                        },
                    },
                    self.limits.diagnostics,
                );
            }

            let response = self.fetch_one(
                request_index,
                request,
                &document_origin,
                &self.network_access,
                plan.enforcing_policy_count() != 0,
                cancellation,
                deadline,
                diagnostics,
                &mut accounting,
            )?;
            responses.push(response);
        }

        Ok(StyleFetchSet {
            document_version: plan.document_version(),
            navigation_commit: plan.navigation_commit().clone(),
            responses,
            diagnostics: std::mem::take(diagnostics),
            aggregate_body_bytes: accounting.body_bytes,
            aggregate_header_bytes: accounting.header_bytes,
            http_exchanges: accounting.http_exchanges,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn fetch_one(
        &self,
        request_index: usize,
        request_identity: &StyleResourceRequestIdentity,
        document_origin: &WebOrigin,
        network_access: &GeneralWebNetworkAccess,
        has_enforcing_policy: bool,
        cancellation: &CancellationToken,
        deadline: Instant,
        diagnostics: &mut Vec<StyleFetchDiagnostic>,
        accounting: &mut FetchAccounting,
    ) -> Result<StyleFetchResponse, StyleFetchError> {
        let mut point = ErrorPoint::request(request_index, request_identity, 0);
        let mut target = initial_request_target(request_identity, document_origin.scheme())
            .map_err(|reason| point.error(reason))?;
        let initial_origin = target.origin().clone();
        let mut visited = try_vec_with_capacity(self.limits.redirects.saturating_add(1))
            .map_err(|reason| point.error(reason))?;
        push_visited(&mut visited, target.url().as_str()).map_err(|reason| point.error(reason))?;
        let mut redirect_count = 0usize;
        let mut all_redirects_same_origin = true;

        loop {
            point.redirect_index = redirect_count;
            checkpoint(cancellation, deadline, point)?;
            accounting.http_exchanges = checked_increment(
                accounting.http_exchanges,
                self.limits.http_exchanges,
                StyleFetchLimit::HttpExchanges,
            )
            .map_err(|reason| point.error(reason))?;
            let response = execute_style_request(
                &self.client,
                &target,
                network_access,
                cancellation,
                deadline,
            )
            .map_err(|reason| point.error(reason))?;
            checkpoint(cancellation, deadline, point)?;
            let response_security = response.security();
            validate_connection_security(target.origin().scheme(), response_security)
                .map_err(|reason| point.error(reason))?;
            let status = response.head().status();
            let status_code = status.as_u16();

            if status.is_redirect() {
                let location = single_location(response.head().headers())
                    .map_err(|reason| point.error(reason))?;
                let resolved = target
                    .url()
                    .join(location)
                    .map_err(|_| point.error(StyleFetchRejection::RedirectLocationInvalid))?;
                let (_, next) = GeneralWebTarget::from_navigation_url(resolved)
                    .map_err(|error| point.error(map_redirect_target_error(&error)))?;
                if next.url().as_str().len() > MAX_STYLE_RESOURCE_URL_BYTES {
                    return Err(point.error(StyleFetchRejection::RedirectUrlBytes));
                }
                validate_mixed_content(document_origin.scheme(), &next)
                    .map_err(|reason| point.error(reason))?;
                if has_enforcing_policy && next.origin() != &initial_origin {
                    return Err(point.error(StyleFetchRejection::CrossOriginPolicyUnproven));
                }
                if visited.iter().any(|url| url == next.url().as_str()) {
                    return Err(point.error(StyleFetchRejection::RedirectLoop));
                }
                redirect_count = checked_increment(
                    redirect_count,
                    self.limits.redirects,
                    StyleFetchLimit::Redirects,
                )
                .map_err(|reason| point.error(reason))?;
                point.redirect_index = redirect_count;
                push_visited(&mut visited, next.url().as_str())
                    .map_err(|reason| point.error(reason))?;
                let diagnostic = point
                    .diagnostic(StyleFetchDiagnosticKind::RedirectFollowed {
                        status: status_code,
                    })
                    .map_err(|reason| point.error(reason))?;
                push_authoritative_diagnostic_lossy(
                    diagnostics,
                    diagnostic,
                    self.limits.diagnostics,
                );
                all_redirects_same_origin &= target.origin() == next.origin();
                target = next;
                continue;
            }

            if (300..=399).contains(&status_code) {
                return Err(point.error(StyleFetchRejection::UnsupportedRedirectStatus));
            }
            if !(200..=299).contains(&status_code) {
                return Err(point.error(StyleFetchRejection::HttpStatus));
            }
            return self.retain_final_response(
                &FinalResponseContext {
                    point,
                    request_identity,
                    request_index,
                    document_origin,
                    target: &target,
                    redirect_count,
                    all_redirects_same_origin,
                    status_code,
                    response_security,
                    cancellation,
                    deadline,
                },
                response,
                diagnostics,
                accounting,
            );
        }
    }

    fn retain_final_response(
        &self,
        context: &FinalResponseContext<'_>,
        response: GeneralWebResponse,
        diagnostics: &mut Vec<StyleFetchDiagnostic>,
        accounting: &mut FetchAccounting,
    ) -> Result<StyleFetchResponse, StyleFetchError> {
        let point = context.point;
        let (headers, unknown_mime) = admit_response_headers(response.head().headers())
            .map_err(|reason| point.error(reason))?;
        let retained_header_bytes = headers
            .checked_retained_bytes()
            .map_err(|reason| point.error(reason))?;
        let next_header_bytes = accounting
            .header_bytes
            .checked_add(retained_header_bytes)
            .ok_or_else(|| point.error(StyleFetchRejection::CounterOverflow))?;
        if next_header_bytes > MAX_STYLE_FETCH_AGGREGATE_HEADER_BYTES {
            return Err(point.error(StyleFetchRejection::Limit(
                StyleFetchLimit::AggregateHeaderBytes,
            )));
        }
        if unknown_mime {
            let diagnostic = point
                .diagnostic(StyleFetchDiagnosticKind::UnknownMimeAccepted)
                .map_err(|reason| point.error(reason))?;
            push_authoritative_diagnostic_lossy(diagnostics, diagnostic, self.limits.diagnostics);
        }

        let framing = response.head().body_framing();
        let (http_response, _) = response.into_parts();
        let (_, mut body) = http_response.into_parts();
        let retained_body = read_bounded_body(
            &mut body,
            framing,
            self.limits.response_body_bytes,
            accounting.body_bytes,
            self.limits.aggregate_body_bytes,
            context.cancellation,
            context.deadline,
        )
        .map_err(|reason| point.error(reason))?;
        let next_body_bytes = accounting
            .body_bytes
            .checked_add(retained_body.len())
            .ok_or_else(|| point.error(StyleFetchRejection::CounterOverflow))?;
        let final_url =
            try_copy_string(context.target.url().as_str()).map_err(|reason| point.error(reason))?;
        let origin_cleanliness = if context.all_redirects_same_origin
            && document_principal_subsumes(context.document_origin, context.target.origin())
        {
            StyleFetchOriginCleanliness::Clean
        } else {
            StyleFetchOriginCleanliness::Tainted
        };

        accounting.body_bytes = next_body_bytes;
        accounting.header_bytes = next_header_bytes;
        Ok(StyleFetchResponse {
            document_version: context.request_identity.document_version(),
            owner: context.request_identity.owner(),
            request_index: context.request_index,
            final_url,
            redirect_count: context.redirect_count,
            origin_cleanliness,
            status: context.status_code,
            security: context.response_security,
            headers,
            body: retained_body,
        })
    }
}

fn document_principal_subsumes(document: &WebOrigin, response: &WebOrigin) -> bool {
    // This slice admits only ordinary tuple-origin web principals. Expanded,
    // system, opaque, and CORS-authorized principals are outside its contract.
    document == response
}

impl fmt::Debug for StyleFetchOwner {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StyleFetchOwner")
            .field("limits", &self.0.limits)
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for NonProductStyleFetchOwner {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NonProductStyleFetchOwner")
            .field("limits", &self.0.limits)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy)]
struct ErrorPoint {
    document_version: DocumentVersion,
    owner: Option<NodeId>,
    request_index: usize,
    redirect_index: usize,
}

impl ErrorPoint {
    fn plan(document_version: DocumentVersion) -> Self {
        Self {
            document_version,
            owner: None,
            request_index: 0,
            redirect_index: 0,
        }
    }

    fn request(
        request_index: usize,
        request: &StyleResourceRequestIdentity,
        redirect_index: usize,
    ) -> Self {
        Self {
            document_version: request.document_version(),
            owner: Some(request.owner()),
            request_index,
            redirect_index,
        }
    }

    const fn error(self, rejection: StyleFetchRejection) -> StyleFetchError {
        StyleFetchError {
            document_version: self.document_version,
            owner: self.owner,
            request_index: self.request_index,
            redirect_index: self.redirect_index,
            rejection,
        }
    }

    const fn diagnostic(
        self,
        kind: StyleFetchDiagnosticKind,
    ) -> Result<StyleFetchDiagnostic, StyleFetchRejection> {
        let Some(owner) = self.owner else {
            return Err(StyleFetchRejection::PlanOwnership);
        };
        Ok(StyleFetchDiagnostic {
            document_version: self.document_version,
            owner,
            request_index: self.request_index,
            redirect_index: self.redirect_index,
            kind,
        })
    }
}

#[derive(Default)]
struct FetchAccounting {
    body_bytes: usize,
    header_bytes: usize,
    http_exchanges: usize,
}

struct FinalResponseContext<'a> {
    point: ErrorPoint,
    request_identity: &'a StyleResourceRequestIdentity,
    request_index: usize,
    document_origin: &'a WebOrigin,
    target: &'a GeneralWebTarget,
    redirect_count: usize,
    all_redirects_same_origin: bool,
    status_code: u16,
    response_security: ConnectionSecurity,
    cancellation: &'a CancellationToken,
    deadline: Instant,
}

fn sealed_transport_config(
    policy: StyleFetchTransportPolicy,
) -> Result<GeneralWebConfig, StyleFetchOwnerError> {
    let http = ClientConfig::try_new_explicit_v1(
        MAX_STYLE_FETCH_WIRE_HEADER_BYTES,
        MAX_STYLE_FETCH_WIRE_HEADER_FIELDS,
        MAX_STYLE_FETCH_WIRE_BODY_BYTES,
        MAX_STYLE_FETCH_REQUEST_HEAD_BYTES,
        MAX_STYLE_FETCH_REQUEST_HEADER_FIELDS,
        MAX_STYLE_FETCH_REQUEST_BODY_BYTES,
        policy.max_chunk_line_bytes(),
        MAX_STYLE_FETCH_INFORMATIONAL_RESPONSES,
        STYLE_FETCH_CONNECT_TIMEOUT,
        STYLE_FETCH_READ_TIMEOUT,
        STYLE_FETCH_WRITE_TIMEOUT,
    )
    .ok_or(StyleFetchOwnerError::TransportPolicyInvalid)?;
    GeneralWebConfig::try_new_explicit_v1(
        http,
        STYLE_FETCH_DNS_TIMEOUT,
        STYLE_FETCH_TLS_HANDSHAKE_TIMEOUT,
        MAX_STYLE_FETCH_DNS_CANDIDATES,
        MAX_STYLE_FETCH_CONNECTION_ATTEMPTS,
        MAX_STYLE_FETCH_TLS_HANDSHAKE_BYTES,
    )
    .ok_or(StyleFetchOwnerError::TransportPolicyInvalid)
}

fn validate_plan_topology(plan: &StyleResourcePlan) -> Result<(), StyleFetchRejection> {
    let mut expected_request = 0usize;
    for candidate in plan.candidates() {
        if candidate.document_version() != plan.document_version()
            || candidate.owner().document_id() != plan.document_version().document_id()
        {
            return Err(StyleFetchRejection::PlanOwnership);
        }
        if let StyleResourceCandidateStatus::Admitted { request_index } = candidate.status() {
            if request_index != expected_request {
                return Err(StyleFetchRejection::PlanOwnership);
            }
            let request = plan
                .requests()
                .get(request_index)
                .ok_or(StyleFetchRejection::PlanOwnership)?;
            if request.document_version() != candidate.document_version()
                || request.owner() != candidate.owner()
                || candidate.policy_decision() != Some(request.policy_decision())
            {
                return Err(StyleFetchRejection::PlanOwnership);
            }
            expected_request = expected_request
                .checked_add(1)
                .ok_or(StyleFetchRejection::CounterOverflow)?;
        }
    }
    if expected_request != plan.requests().len() {
        return Err(StyleFetchRejection::PlanOwnership);
    }
    Ok(())
}

fn validate_operation_start(
    cancellation: &CancellationToken,
    deadline: Instant,
) -> Result<(), StyleFetchRejection> {
    if cancellation.is_cancelled() {
        return Err(StyleFetchRejection::Cancelled);
    }
    let now = Instant::now();
    let Some(remaining) = deadline.checked_duration_since(now) else {
        return Err(StyleFetchRejection::DeadlineExceeded);
    };
    if remaining.is_zero() {
        return Err(StyleFetchRejection::DeadlineExceeded);
    }
    if remaining > MAX_STYLE_FETCH_DURATION {
        return Err(StyleFetchRejection::DeadlineTooFar);
    }
    Ok(())
}

fn checkpoint(
    cancellation: &CancellationToken,
    deadline: Instant,
    point: ErrorPoint,
) -> Result<(), StyleFetchError> {
    if cancellation.is_cancelled() {
        return Err(point.error(StyleFetchRejection::Cancelled));
    }
    if Instant::now() >= deadline {
        return Err(point.error(StyleFetchRejection::DeadlineExceeded));
    }
    Ok(())
}

fn initial_request_target(
    request: &StyleResourceRequestIdentity,
    document_scheme: WebScheme,
) -> Result<GeneralWebTarget, StyleFetchRejection> {
    let (identity, target) = GeneralWebTarget::parse_navigation(request.canonical_url())
        .map_err(|_| StyleFetchRejection::PlanOwnership)?;
    if identity.as_str() != request.canonical_url()
        || target.url().as_str() != request.canonical_url()
    {
        return Err(StyleFetchRejection::PlanOwnership);
    }
    validate_mixed_content(document_scheme, &target)?;
    Ok(target)
}

fn validate_mixed_content(
    document_scheme: WebScheme,
    target: &GeneralWebTarget,
) -> Result<(), StyleFetchRejection> {
    if document_scheme == WebScheme::Https
        && target.origin().scheme() == WebScheme::Http
        && !is_potentially_trustworthy_loopback_host(target.origin().host())
    {
        Err(StyleFetchRejection::MixedContent)
    } else {
        Ok(())
    }
}

fn is_potentially_trustworthy_loopback_host(host: &WebHost) -> bool {
    match host {
        WebHost::Domain(domain) => {
            let without_trailing_dot = domain.strip_suffix('.').unwrap_or(domain);
            without_trailing_dot.eq_ignore_ascii_case("localhost")
                || ends_with_ignore_ascii_case(without_trailing_dot, ".localhost")
        }
        WebHost::Ip(IpAddr::V4(address)) => address.octets()[0] == 127,
        WebHost::Ip(IpAddr::V6(address)) => *address == Ipv6Addr::LOCALHOST,
    }
}

fn ends_with_ignore_ascii_case(value: &str, suffix: &str) -> bool {
    value
        .get(value.len().saturating_sub(suffix.len())..)
        .is_some_and(|tail| tail.eq_ignore_ascii_case(suffix))
}

fn validate_connection_security(
    scheme: WebScheme,
    security: ConnectionSecurity,
) -> Result<(), StyleFetchRejection> {
    match (scheme, security) {
        (WebScheme::Http, ConnectionSecurity::Cleartext)
        | (WebScheme::Https, ConnectionSecurity::Tls { .. }) => Ok(()),
        (WebScheme::Http, ConnectionSecurity::Tls { .. })
        | (WebScheme::Https, ConnectionSecurity::Cleartext) => {
            Err(StyleFetchRejection::TransportSecurityMismatch)
        }
    }
}

fn single_location(headers: &Headers) -> Result<&str, StyleFetchRejection> {
    let mut locations = headers.values("location");
    let first = locations
        .next()
        .ok_or(StyleFetchRejection::RedirectLocationMissing)?;
    let first = trim_ows(first.as_bytes());
    for repeated in locations {
        if trim_ows(repeated.as_bytes()) != first {
            return Err(StyleFetchRejection::RedirectLocationConflict);
        }
    }
    std::str::from_utf8(first).map_err(|_| StyleFetchRejection::RedirectLocationNonUtf8)
}

fn map_redirect_target_error(error: &NetworkError) -> StyleFetchRejection {
    match error {
        NetworkError::CredentialsNotAllowed => StyleFetchRejection::RedirectCredentials,
        NetworkError::UnsupportedScheme(_) => StyleFetchRejection::RedirectScheme,
        NetworkError::LimitExceeded {
            kind: LimitKind::UrlBytes,
            ..
        } => StyleFetchRejection::RedirectUrlBytes,
        _ => StyleFetchRejection::RedirectLocationInvalid,
    }
}

fn execute_style_request(
    client: &GeneralWebClient,
    target: &GeneralWebTarget,
    network_access: &GeneralWebNetworkAccess,
    cancellation: &CancellationToken,
    deadline: Instant,
) -> Result<GeneralWebResponse, StyleFetchRejection> {
    let request = GeneralWebRequest::get_with_network_access(
        target.clone(),
        RedirectPolicy::Manual,
        network_access.clone(),
    )
    .with_cancellation(cancellation.clone())
    .with_deadline(deadline);
    client
        .execute_checked(&request)
        .map_err(|error| map_checked_network_error(error, cancellation, deadline))
}

fn map_network_error(
    error: &NetworkError,
    cancellation: &CancellationToken,
    deadline: Instant,
) -> StyleFetchRejection {
    if cancellation.is_cancelled() || matches!(error, NetworkError::Cancelled) {
        return StyleFetchRejection::Cancelled;
    }
    if Instant::now() >= deadline {
        return StyleFetchRejection::DeadlineExceeded;
    }
    let family = match error {
        NetworkError::Dns(_) => StyleFetchNetworkFailure::Dns,
        NetworkError::ConnectAttemptsExhausted { .. }
        | NetworkError::Io { .. }
        | NetworkError::Timeout(_) => StyleFetchNetworkFailure::Connection,
        NetworkError::Tls(_) | NetworkError::TrustStore(_) => StyleFetchNetworkFailure::Tls,
        NetworkError::LimitExceeded { .. } => StyleFetchNetworkFailure::ResourceLimit,
        NetworkError::InvalidLineEnding
        | NetworkError::MalformedStatusLine
        | NetworkError::MalformedHeader
        | NetworkError::ObsoleteLineFolding
        | NetworkError::ConflictingContentLength
        | NetworkError::AmbiguousBodyFraming
        | NetworkError::InvalidContentLength
        | NetworkError::UnsupportedTransferCoding(_)
        | NetworkError::UnsupportedContentCoding(_)
        | NetworkError::MalformedChunkSize
        | NetworkError::ProhibitedTrailer(_)
        | NetworkError::PrematureEof
        | NetworkError::ProtocolSwitchUnsupported
        | NetworkError::RedirectRejected(_) => StyleFetchNetworkFailure::HttpProtocol,
        _ => StyleFetchNetworkFailure::Other,
    };
    StyleFetchRejection::Network(family)
}

fn map_checked_network_error(
    error: GeneralWebExecutionError,
    cancellation: &CancellationToken,
    deadline: Instant,
) -> StyleFetchRejection {
    if cancellation.is_cancelled()
        || matches!(
            error,
            GeneralWebExecutionError::Transport(GeneralWebTransportFailure::Cancelled)
        )
    {
        return StyleFetchRejection::Cancelled;
    }
    if Instant::now() >= deadline {
        return StyleFetchRejection::DeadlineExceeded;
    }
    let family = match error {
        GeneralWebExecutionError::Policy(GeneralWebPolicyError::RestrictedPort) => {
            StyleFetchNetworkFailure::RestrictedPort
        }
        GeneralWebExecutionError::Policy(GeneralWebPolicyError::InvalidInitiatorEvidence) => {
            StyleFetchNetworkFailure::InitiatorEvidence
        }
        GeneralWebExecutionError::Policy(GeneralWebPolicyError::LocalNetworkAccessDenied {
            ..
        }) => StyleFetchNetworkFailure::LocalNetworkAccess,
        GeneralWebExecutionError::Transport(GeneralWebTransportFailure::Dns(_)) => {
            StyleFetchNetworkFailure::Dns
        }
        GeneralWebExecutionError::Transport(
            GeneralWebTransportFailure::Connection | GeneralWebTransportFailure::Timeout(_),
        ) => StyleFetchNetworkFailure::Connection,
        GeneralWebExecutionError::Transport(GeneralWebTransportFailure::Tls(_)) => {
            StyleFetchNetworkFailure::Tls
        }
        GeneralWebExecutionError::Transport(GeneralWebTransportFailure::Limit { .. }) => {
            StyleFetchNetworkFailure::ResourceLimit
        }
        GeneralWebExecutionError::Transport(
            GeneralWebTransportFailure::HttpProtocol
            | GeneralWebTransportFailure::RedirectRejected(_),
        ) => StyleFetchNetworkFailure::HttpProtocol,
        GeneralWebExecutionError::Transport(
            GeneralWebTransportFailure::Request | GeneralWebTransportFailure::Cancelled,
        ) => StyleFetchNetworkFailure::Other,
    };
    StyleFetchRejection::Network(family)
}

fn admit_response_headers(
    headers: &Headers,
) -> Result<(StyleFetchResponseHeaders, bool), StyleFetchRejection> {
    let content_type = merge_content_type_values(headers)?;
    let xcto_bytes = merged_field_value_len(headers, "x-content-type-options", 2)?;
    let relevant_bytes = content_type
        .as_ref()
        .map_or(0, |content_type| content_type.retained.len())
        .checked_add(xcto_bytes)
        .ok_or(StyleFetchRejection::CounterOverflow)?;
    if relevant_bytes > MAX_STYLE_FETCH_WIRE_HEADER_BYTES {
        return Err(StyleFetchRejection::Limit(StyleFetchLimit::WireHeaderBytes));
    }

    let nosniff = first_xcto_value_is_nosniff(headers);
    let extracted = content_type
        .as_ref()
        .map(|content_type| {
            extract_response_mime(&content_type.retained, content_type.latest_original)
        })
        .transpose()?
        .flatten();
    let (mime, charset) = match extracted {
        Some(extracted) if extracted.is_css => (MimeClassification::Css, extracted.charset),
        Some(extracted) => (MimeClassification::Other, extracted.charset),
        None => (MimeClassification::Unknown, None),
    };
    match (mime, nosniff) {
        (MimeClassification::Css, _) | (MimeClassification::Unknown, false) => {}
        (MimeClassification::Unknown | MimeClassification::Other, true) => {
            return Err(StyleFetchRejection::NoSniff);
        }
        (MimeClassification::Other, false) => return Err(StyleFetchRejection::MimeNotCss),
    }

    let unknown = mime == MimeClassification::Unknown;
    Ok((
        StyleFetchResponseHeaders {
            content_type: content_type.map(|content_type| content_type.retained),
            charset,
            mime: if unknown {
                StyleFetchMime::Unknown
            } else {
                StyleFetchMime::Css
            },
            nosniff,
        },
        unknown,
    ))
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum MimeClassification {
    Css,
    Unknown,
    Other,
}

struct MergedContentType<'a> {
    retained: Vec<u8>,
    latest_original: &'a [u8],
}

fn merge_content_type_values(
    headers: &Headers,
) -> Result<Option<MergedContentType<'_>>, StyleFetchRejection> {
    let mut length = 0usize;
    let mut latest_original = None;
    for value in headers.values("content-type") {
        if length != 0 {
            length = length
                .checked_add(1)
                .ok_or(StyleFetchRejection::CounterOverflow)?;
        }
        let bytes = value.as_bytes();
        length = length
            .checked_add(bytes.len())
            .ok_or(StyleFetchRejection::CounterOverflow)?;
        latest_original = Some(bytes);
    }
    let Some(latest_original) = latest_original else {
        return Ok(None);
    };
    if length > MAX_STYLE_FETCH_CONTENT_TYPE_BYTES {
        return Err(StyleFetchRejection::Limit(
            StyleFetchLimit::ContentTypeBytes,
        ));
    }
    let mut merged = Vec::new();
    merged
        .try_reserve_exact(length)
        .map_err(|_| StyleFetchRejection::AllocationFailed)?;
    for value in headers.values("content-type") {
        if !merged.is_empty() {
            merged.push(b',');
        }
        merged.extend_from_slice(value.as_bytes());
    }
    Ok(Some(MergedContentType {
        retained: merged,
        latest_original,
    }))
}

fn merged_field_value_len(
    headers: &Headers,
    name: &str,
    separator_bytes: usize,
) -> Result<usize, StyleFetchRejection> {
    let mut count = 0usize;
    let mut length = 0usize;
    for value in headers.values(name) {
        if count != 0 {
            length = length
                .checked_add(separator_bytes)
                .ok_or(StyleFetchRejection::CounterOverflow)?;
        }
        length = length
            .checked_add(value.as_bytes().len())
            .ok_or(StyleFetchRejection::CounterOverflow)?;
        count = count
            .checked_add(1)
            .ok_or(StyleFetchRejection::CounterOverflow)?;
    }
    Ok(length)
}

fn first_xcto_value_is_nosniff(headers: &Headers) -> bool {
    let Some(first_field) = headers.values("x-content-type-options").next() else {
        return false;
    };
    let first_value = first_field
        .as_bytes()
        .split(|byte| *byte == b',')
        .next()
        .map_or(&[][..], trim_http_whitespace);
    first_value.eq_ignore_ascii_case(b"nosniff")
}

struct ExtractedResponseMime {
    is_css: bool,
    charset: Option<Vec<u8>>,
}

fn extract_response_mime(
    merged: &[u8],
    latest_original: &[u8],
) -> Result<Option<ExtractedResponseMime>, StyleFetchRejection> {
    if let Some(extracted) = extract_mime_type(merged)? {
        return Ok(Some(ExtractedResponseMime {
            is_css: extracted.essence.is_css(),
            charset: extracted.charset,
        }));
    }
    extract_legacy_mime_type(latest_original)
}

struct LegacyMimeState<'a> {
    media_type: Option<&'a [u8]>,
    charset: Option<Vec<u8>>,
    had_charset: bool,
}

impl<'a> LegacyMimeState<'a> {
    const fn new() -> Self {
        Self {
            media_type: None,
            charset: None,
            had_charset: false,
        }
    }

    fn process(&mut self, value: &'a [u8]) -> Result<(), StyleFetchRejection> {
        let mut type_start = 0usize;
        while type_start < value.len() && is_legacy_http_lws(value[type_start]) {
            type_start += 1;
        }
        let mut type_end = type_start;
        while type_end < value.len()
            && !is_legacy_http_lws(value[type_end])
            && value[type_end] != b';'
        {
            type_end += 1;
        }
        let media_type = &value[type_start..type_end];
        if media_type.is_empty() || !media_type.contains(&b'/') || media_type == b"*/*" {
            return Ok(());
        }

        let legacy_charset = legacy_charset_parameter(value, type_end)?;
        let same_as_previous = self
            .media_type
            .is_some_and(|previous| previous.eq_ignore_ascii_case(media_type));
        if !same_as_previous {
            self.media_type = Some(media_type);
        }

        if (!same_as_previous && self.had_charset) || legacy_charset.is_some() {
            self.had_charset = true;
            self.charset = Some(legacy_charset.unwrap_or_default());
        }
        Ok(())
    }
}

fn extract_legacy_mime_type(
    value: &[u8],
) -> Result<Option<ExtractedResponseMime>, StyleFetchRejection> {
    let mut state = LegacyMimeState::new();
    let mut start = 0usize;
    loop {
        let end = legacy_find_media_delimiter(value, start, b',');
        state.process(&value[start..end])?;
        if end == value.len() {
            break;
        }
        start = end
            .checked_add(1)
            .ok_or(StyleFetchRejection::CounterOverflow)?;
        if start >= value.len() {
            break;
        }
    }
    Ok(state.media_type.map(|media_type| ExtractedResponseMime {
        is_css: media_type.eq_ignore_ascii_case(b"text/css"),
        charset: state.charset,
    }))
}

fn legacy_charset_parameter(
    value: &[u8],
    type_end: usize,
) -> Result<Option<Vec<u8>>, StyleFetchRejection> {
    let Some(parameter_start) = value[type_end..]
        .iter()
        .position(|byte| *byte == b';')
        .and_then(|offset| type_end.checked_add(offset))
    else {
        return Ok(None);
    };

    let mut current = parameter_start
        .checked_add(1)
        .ok_or(StyleFetchRejection::CounterOverflow)?;
    let mut charset_range = None;
    while current <= value.len() {
        let end = legacy_find_media_delimiter(value, current, b';');
        let mut name_start = current;
        while name_start < end && is_legacy_http_lws(value[name_start]) {
            name_start += 1;
        }
        let charset_prefix = b"charset=";
        if value
            .get(name_start..name_start.saturating_add(charset_prefix.len()))
            .is_some_and(|name| name.eq_ignore_ascii_case(charset_prefix))
        {
            let charset_start = name_start
                .checked_add(charset_prefix.len())
                .ok_or(StyleFetchRejection::CounterOverflow)?;
            charset_range = Some((charset_start, end));
        }
        if end == value.len() {
            break;
        }
        current = end
            .checked_add(1)
            .ok_or(StyleFetchRejection::CounterOverflow)?;
    }

    let Some((mut charset_start, charset_limit)) = charset_range else {
        return Ok(None);
    };
    while charset_start < charset_limit && is_legacy_http_lws(value[charset_start]) {
        charset_start += 1;
    }
    if value.get(charset_start) == Some(&b'"') {
        let content_start = charset_start
            .checked_add(1)
            .ok_or(StyleFetchRejection::CounterOverflow)?;
        let content_end = legacy_find_string_end(value, charset_start).min(charset_limit);
        let mut charset = try_vec_with_capacity(content_end.saturating_sub(content_start))?;
        let mut position = content_start;
        while position < content_end {
            if value[position] == b'\\' && position + 1 < content_end {
                position += 1;
            }
            charset.push(value[position]);
            position += 1;
        }
        return Ok(Some(charset));
    }

    let mut charset_end = charset_start;
    while charset_end < charset_limit
        && !is_legacy_http_lws(value[charset_end])
        && value[charset_end] != b';'
    {
        charset_end += 1;
    }
    Ok(Some(try_copy_bytes(&value[charset_start..charset_end])?))
}

fn legacy_find_media_delimiter(value: &[u8], start: usize, delimiter: u8) -> usize {
    let mut position = start;
    while position < value.len() {
        if value[position] == delimiter {
            return position;
        }
        if value[position] == b'"' {
            let quote_end = legacy_find_string_end(value, position);
            if quote_end == value.len() {
                return quote_end;
            }
            position = quote_end + 1;
        } else {
            position += 1;
        }
    }
    value.len()
}

fn legacy_find_string_end(value: &[u8], quote_start: usize) -> usize {
    let mut position = quote_start.saturating_add(1);
    while position < value.len() {
        match value[position] {
            b'"' => return position,
            b'\\' => position = position.saturating_add(2),
            _ => position += 1,
        }
    }
    value.len()
}

const fn is_legacy_http_lws(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t')
}

struct ExtractedMime<'a> {
    essence: MimeEssence<'a>,
    charset: Option<Vec<u8>>,
}

#[derive(Clone, Copy)]
struct MimeEssence<'a> {
    type_name: &'a [u8],
    subtype: &'a [u8],
}

impl MimeEssence<'_> {
    fn is_wildcard(self) -> bool {
        self.type_name == b"*" && self.subtype == b"*"
    }

    fn is_css(self) -> bool {
        self.type_name.eq_ignore_ascii_case(b"text") && self.subtype.eq_ignore_ascii_case(b"css")
    }

    fn equals(self, other: Self) -> bool {
        self.type_name.eq_ignore_ascii_case(other.type_name)
            && self.subtype.eq_ignore_ascii_case(other.subtype)
    }
}

struct ParsedMime<'a> {
    essence: MimeEssence<'a>,
    charset: Option<Vec<u8>>,
}

struct MimeExtractionState<'a> {
    selected: Option<MimeEssence<'a>>,
    selected_charset: Option<Vec<u8>>,
    previous: Option<MimeEssence<'a>>,
    previous_charset: Option<Vec<u8>>,
}

impl<'a> MimeExtractionState<'a> {
    const fn new() -> Self {
        Self {
            selected: None,
            selected_charset: None,
            previous: None,
            previous_charset: None,
        }
    }

    fn process(&mut self, candidate: &'a [u8]) -> Result<(), StyleFetchRejection> {
        if candidate == b"error" {
            return Ok(());
        }
        let Some(parsed) = parse_mime_candidate(candidate)? else {
            return Ok(());
        };
        if parsed.essence.is_wildcard() {
            self.selected = self.previous;
            return Ok(());
        }

        let same_as_previous = self
            .previous
            .is_some_and(|previous| parsed.essence.equals(previous));
        if !same_as_previous {
            self.previous = Some(parsed.essence);
        }
        let type_has_charset = parsed.charset.is_some();
        let selected_charset = if let Some(charset) = parsed.charset {
            Some(charset)
        } else if same_as_previous {
            try_copy_optional_bytes(self.previous_charset.as_deref())?
        } else {
            None
        };
        if (!same_as_previous && self.previous_charset.is_some()) || type_has_charset {
            self.previous_charset = try_copy_optional_bytes(selected_charset.as_deref())?;
        }
        self.selected = Some(parsed.essence);
        self.selected_charset = selected_charset;
        Ok(())
    }
}

fn extract_mime_type(value: &[u8]) -> Result<Option<ExtractedMime<'_>>, StyleFetchRejection> {
    let mut state = MimeExtractionState::new();
    let mut in_quotes = false;
    let mut start = 0usize;
    for (index, byte) in value.iter().copied().enumerate() {
        if byte == b'"' && (index == 0 || value[index - 1] != b'\\') {
            in_quotes = !in_quotes;
        } else if byte == b',' && !in_quotes {
            state.process(&value[start..index])?;
            start = index
                .checked_add(1)
                .ok_or(StyleFetchRejection::CounterOverflow)?;
        }
    }
    if start < value.len() {
        state.process(&value[start..])?;
    }
    Ok(state.selected.map(|essence| ExtractedMime {
        essence,
        charset: state.selected_charset,
    }))
}

fn parse_mime_candidate(value: &[u8]) -> Result<Option<ParsedMime<'_>>, StyleFetchRejection> {
    let value = trim_http_whitespace(value);
    if value.is_empty() {
        return Ok(None);
    }
    let end = value.len();
    let mut position = 0usize;
    let type_start = position;
    while position < end && value[position] != b'/' {
        if !is_mime_token_byte(value[position]) {
            return Ok(None);
        }
        position += 1;
    }
    if type_start == position || position == end {
        return Ok(None);
    }
    let type_name = &value[type_start..position];
    position += 1;

    let subtype_start = position;
    let mut subtype_end = None;
    while position < end && value[position] != b';' {
        if !is_mime_token_byte(value[position]) {
            if !is_http_whitespace(value[position]) {
                return Ok(None);
            }
            subtype_end = Some(position);
            position += 1;
            while position < end && value[position] != b';' {
                if !is_http_whitespace(value[position]) {
                    return Ok(None);
                }
                position += 1;
            }
            break;
        }
        position += 1;
    }
    let subtype_end = subtype_end.unwrap_or(position);
    if subtype_start == subtype_end {
        return Ok(None);
    }

    let mut charset = None;
    while position < end {
        position += 1;
        while position < end && is_http_whitespace(value[position]) {
            position += 1;
        }
        let name_start = position;
        let mut invalid_name = false;
        while position < end && !matches!(value[position], b';' | b'=') {
            invalid_name |= !is_mime_token_byte(value[position]);
            position += 1;
        }
        let name = &value[name_start..position];
        if position == end {
            break;
        }
        if value[position] == b';' {
            continue;
        }
        position += 1;
        if position == end {
            break;
        }

        let capture = charset.is_none()
            && !name.is_empty()
            && !invalid_name
            && name.eq_ignore_ascii_case(b"charset");
        let (valid_value, captured) = if value[position] == b'"' {
            parse_quoted_parameter_value(value, &mut position, capture)?
        } else {
            parse_unquoted_parameter_value(value, &mut position, capture)?
        };
        if capture && valid_value {
            charset = captured;
        }
    }

    Ok(Some(ParsedMime {
        essence: MimeEssence {
            type_name,
            subtype: &value[subtype_start..subtype_end],
        },
        charset,
    }))
}

fn parse_quoted_parameter_value(
    value: &[u8],
    position: &mut usize,
    capture: bool,
) -> Result<(bool, Option<Vec<u8>>), StyleFetchRejection> {
    *position += 1;
    let mut valid = true;
    let mut captured = if capture {
        Some(try_vec_with_capacity(
            value.len().saturating_sub(*position),
        )?)
    } else {
        None
    };
    loop {
        while *position < value.len() && !matches!(value[*position], b'"' | b'\\') {
            let byte = value[*position];
            valid &= is_http_quoted_string_byte(byte);
            if let Some(bytes) = captured.as_mut() {
                bytes.push(byte);
            }
            *position += 1;
        }
        if *position < value.len() && value[*position] == b'\\' {
            *position += 1;
            if *position < value.len() {
                let byte = value[*position];
                valid &= is_http_quoted_string_byte(byte);
                if let Some(bytes) = captured.as_mut() {
                    bytes.push(byte);
                }
                *position += 1;
                continue;
            }
            if let Some(bytes) = captured.as_mut() {
                bytes.push(b'\\');
            }
        }
        break;
    }
    while *position < value.len() && value[*position] != b';' {
        *position += 1;
    }
    Ok((valid, captured))
}

fn parse_unquoted_parameter_value(
    value: &[u8],
    position: &mut usize,
    capture: bool,
) -> Result<(bool, Option<Vec<u8>>), StyleFetchRejection> {
    let start = *position;
    while *position < value.len() && value[*position] != b';' {
        *position += 1;
    }
    let unquoted = trim_trailing_http_whitespace(&value[start..*position]);
    if unquoted.is_empty() {
        return Ok((false, None));
    }
    let valid = unquoted.iter().copied().all(is_http_quoted_string_byte);
    let captured = if capture {
        Some(try_copy_bytes(unquoted)?)
    } else {
        None
    };
    Ok((valid, captured))
}

const fn is_mime_token_byte(byte: u8) -> bool {
    matches!(
        byte,
        b'!' | b'#'
            | b'$'
            | b'%'
            | b'&'
            | b'\''
            | b'*'
            | b'+'
            | b'-'
            | b'.'
            | b'^'
            | b'_'
            | b'`'
            | b'|'
            | b'~'
            | b'0'..=b'9'
            | b'A'..=b'Z'
            | b'a'..=b'z'
    )
}

const fn is_http_whitespace(byte: u8) -> bool {
    matches!(byte, b'\t' | b'\n' | b'\r' | b' ')
}

const fn is_http_quoted_string_byte(byte: u8) -> bool {
    matches!(byte, b'\t' | b' '..=b'~' | 0x80..=0xff)
}

fn trim_http_whitespace(mut value: &[u8]) -> &[u8] {
    while value.first().copied().is_some_and(is_http_whitespace) {
        value = &value[1..];
    }
    while value.last().copied().is_some_and(is_http_whitespace) {
        value = &value[..value.len() - 1];
    }
    value
}

fn trim_trailing_http_whitespace(mut value: &[u8]) -> &[u8] {
    while value.last().copied().is_some_and(is_http_whitespace) {
        value = &value[..value.len() - 1];
    }
    value
}

fn trim_ows(mut value: &[u8]) -> &[u8] {
    while matches!(value.first(), Some(b' ' | b'\t')) {
        value = &value[1..];
    }
    while matches!(value.last(), Some(b' ' | b'\t')) {
        value = &value[..value.len() - 1];
    }
    value
}

fn read_bounded_body(
    body: &mut Body,
    framing: BodyFraming,
    per_response_limit: usize,
    prior_aggregate: usize,
    aggregate_limit: usize,
    cancellation: &CancellationToken,
    deadline: Instant,
) -> Result<Vec<u8>, StyleFetchRejection> {
    if let BodyFraming::ContentLength(length) = framing {
        let length = usize::try_from(length)
            .map_err(|_| StyleFetchRejection::Limit(StyleFetchLimit::ResponseBodyBytes))?;
        if length > per_response_limit {
            return Err(StyleFetchRejection::Limit(
                StyleFetchLimit::ResponseBodyBytes,
            ));
        }
        let aggregate = prior_aggregate
            .checked_add(length)
            .ok_or(StyleFetchRejection::CounterOverflow)?;
        if aggregate > aggregate_limit {
            return Err(StyleFetchRejection::Limit(
                StyleFetchLimit::AggregateBodyBytes,
            ));
        }
    }

    let initial_capacity = match framing {
        BodyFraming::ContentLength(length) => usize::try_from(length)
            .unwrap_or(per_response_limit)
            .min(per_response_limit),
        BodyFraming::None | BodyFraming::Chunked | BodyFraming::ConnectionClose => 0,
    };
    let mut retained = Vec::new();
    retained
        .try_reserve_exact(initial_capacity)
        .map_err(|_| StyleFetchRejection::AllocationFailed)?;
    let mut chunk = [0_u8; BODY_READ_CHUNK_BYTES];
    loop {
        if cancellation.is_cancelled() {
            return Err(StyleFetchRejection::Cancelled);
        }
        if Instant::now() >= deadline {
            return Err(StyleFetchRejection::DeadlineExceeded);
        }
        let response_remaining = per_response_limit.saturating_sub(retained.len());
        let aggregate_used = prior_aggregate
            .checked_add(retained.len())
            .ok_or(StyleFetchRejection::CounterOverflow)?;
        let aggregate_remaining = aggregate_limit.saturating_sub(aggregate_used);
        let permitted = chunk
            .len()
            .min(response_remaining.saturating_add(1))
            .min(aggregate_remaining.saturating_add(1))
            .max(1);
        let count = body
            .read_chunk(&mut chunk[..permitted])
            .map_err(|error| map_network_error(&error, cancellation, deadline))?;
        if count == 0 {
            return Ok(retained);
        }
        let next_response = retained
            .len()
            .checked_add(count)
            .ok_or(StyleFetchRejection::CounterOverflow)?;
        if next_response > per_response_limit {
            return Err(StyleFetchRejection::Limit(
                StyleFetchLimit::ResponseBodyBytes,
            ));
        }
        let next_aggregate = prior_aggregate
            .checked_add(next_response)
            .ok_or(StyleFetchRejection::CounterOverflow)?;
        if next_aggregate > aggregate_limit {
            return Err(StyleFetchRejection::Limit(
                StyleFetchLimit::AggregateBodyBytes,
            ));
        }
        retained
            .try_reserve(count)
            .map_err(|_| StyleFetchRejection::AllocationFailed)?;
        retained.extend_from_slice(&chunk[..count]);
    }
}

fn enforce_count(
    actual: usize,
    maximum: usize,
    limit: StyleFetchLimit,
) -> Result<(), StyleFetchRejection> {
    if actual > maximum {
        Err(StyleFetchRejection::Limit(limit))
    } else {
        Ok(())
    }
}

fn checked_increment(
    current: usize,
    maximum: usize,
    limit: StyleFetchLimit,
) -> Result<usize, StyleFetchRejection> {
    let next = current
        .checked_add(1)
        .ok_or(StyleFetchRejection::CounterOverflow)?;
    if next > maximum {
        Err(StyleFetchRejection::Limit(limit))
    } else {
        Ok(next)
    }
}

fn try_vec_with_capacity<T>(capacity: usize) -> Result<Vec<T>, StyleFetchRejection> {
    let mut values = Vec::new();
    values
        .try_reserve_exact(capacity)
        .map_err(|_| StyleFetchRejection::AllocationFailed)?;
    Ok(values)
}

fn try_copy_string(value: &str) -> Result<String, StyleFetchRejection> {
    let mut copied = String::new();
    copied
        .try_reserve_exact(value.len())
        .map_err(|_| StyleFetchRejection::AllocationFailed)?;
    copied.push_str(value);
    Ok(copied)
}

fn try_copy_bytes(value: &[u8]) -> Result<Vec<u8>, StyleFetchRejection> {
    let mut copied = Vec::new();
    copied
        .try_reserve_exact(value.len())
        .map_err(|_| StyleFetchRejection::AllocationFailed)?;
    copied.extend_from_slice(value);
    Ok(copied)
}

fn try_copy_optional_bytes(value: Option<&[u8]>) -> Result<Option<Vec<u8>>, StyleFetchRejection> {
    value.map(try_copy_bytes).transpose()
}

fn push_visited(visited: &mut Vec<String>, value: &str) -> Result<(), StyleFetchRejection> {
    let copied = try_copy_string(value)?;
    visited.push(copied);
    Ok(())
}

fn push_authoritative_diagnostic_lossy(
    diagnostics: &mut Vec<StyleFetchDiagnostic>,
    diagnostic: StyleFetchDiagnostic,
    maximum: usize,
) {
    if maximum == 0 {
        return;
    }
    if diagnostics.len() < maximum {
        diagnostics.push(diagnostic);
        return;
    }
    if let Some(index) = diagnostics.iter().position(|retained| {
        matches!(
            retained.kind,
            StyleFetchDiagnosticKind::ReportOnlyWouldBlock { .. }
        )
    }) {
        diagnostics.remove(index);
        diagnostics.push(diagnostic);
    }
}

fn push_non_enforcing_diagnostic_lossy(
    diagnostics: &mut Vec<StyleFetchDiagnostic>,
    diagnostic: StyleFetchDiagnostic,
    maximum: usize,
) {
    if diagnostics.len() < maximum {
        diagnostics.push(diagnostic);
    }
}

fn failure(error: StyleFetchError, diagnostics: Vec<StyleFetchDiagnostic>) -> StyleFetchFailure {
    StyleFetchFailure { error, diagnostics }
}

#[cfg(test)]
mod tests {
    use super::*;

    type MimeCase<'a> = (&'a [u8], &'a [u8], Option<&'a [u8]>);

    #[test]
    fn sealed_transport_uses_every_explicit_v1_field_at_the_style_policy() {
        let config = sealed_transport_config(StyleFetchTransportPolicy::default())
            .expect("fixed style policy satisfies explicit v1 bounds");
        let http = config.http_config();

        assert_eq!(http.max_header_bytes(), MAX_STYLE_FETCH_WIRE_HEADER_BYTES);
        assert_eq!(http.max_header_count(), MAX_STYLE_FETCH_WIRE_HEADER_FIELDS);
        assert_eq!(http.max_body_bytes(), MAX_STYLE_FETCH_WIRE_BODY_BYTES);
        assert_eq!(
            http.max_request_head_bytes(),
            MAX_STYLE_FETCH_REQUEST_HEAD_BYTES
        );
        assert_eq!(
            http.max_request_header_count(),
            MAX_STYLE_FETCH_REQUEST_HEADER_FIELDS
        );
        assert_eq!(
            http.max_request_body_bytes(),
            MAX_STYLE_FETCH_REQUEST_BODY_BYTES
        );
        assert_eq!(
            http.max_chunk_line_bytes(),
            MAX_STYLE_FETCH_CHUNK_LINE_BYTES
        );
        assert_eq!(
            http.max_informational_responses(),
            MAX_STYLE_FETCH_INFORMATIONAL_RESPONSES
        );
        assert_eq!(http.connect_timeout(), STYLE_FETCH_CONNECT_TIMEOUT);
        assert_eq!(http.read_timeout(), STYLE_FETCH_READ_TIMEOUT);
        assert_eq!(http.write_timeout(), STYLE_FETCH_WRITE_TIMEOUT);
        assert_eq!(config.dns_timeout(), STYLE_FETCH_DNS_TIMEOUT);
        assert_eq!(
            config.tls_handshake_timeout(),
            STYLE_FETCH_TLS_HANDSHAKE_TIMEOUT
        );
        assert_eq!(config.max_dns_candidates(), MAX_STYLE_FETCH_DNS_CANDIDATES);
        assert_eq!(
            config.max_connection_attempts(),
            MAX_STYLE_FETCH_CONNECTION_ATTEMPTS
        );
        assert_eq!(
            config.max_tls_handshake_bytes(),
            MAX_STYLE_FETCH_TLS_HANDSHAKE_BYTES
        );
    }

    #[test]
    fn firefox_content_types_one_through_twenty_select_exact_essence_and_charset() {
        let cases: [MimeCase<'_>; 20] = [
            (b",text/plain", b"text/plain", None),
            (b"text/plain,", b"text/plain", None),
            (b"text/html,text/plain", b"text/plain", None),
            (b"text/plain;charset=gbk,text/html", b"text/html", None),
            (
                b"text/plain;charset=gbk,text/html;charset=windows-1254",
                b"text/html",
                Some(b"windows-1254"),
            ),
            (
                b"text/plain;charset=gbk,text/plain",
                b"text/plain",
                Some(b"gbk"),
            ),
            (
                b"text/plain;charset=gbk,text/plain;charset=windows-1252",
                b"text/plain",
                Some(b"windows-1252"),
            ),
            (
                b"text/html;charset=gbk,text/html;x=\",text/plain",
                b"text/html",
                Some(b"gbk"),
            ),
            (
                b"text/plain;charset=gbk;x=foo,text/plain",
                b"text/plain",
                Some(b"gbk"),
            ),
            (
                b"text/html;charset=gbk,text/plain,text/html",
                b"text/html",
                None,
            ),
            (b"text/plain,*/*", b"text/plain", None),
            (b"text/html,*/*", b"text/html", None),
            (b"*/*,text/html", b"text/html", None),
            (b"text/plain,*/*;charset=gbk", b"text/plain", None),
            (b"text/html,*/*;charset=gbk", b"text/html", None),
            (b"text/html;x=\",text/plain", b"text/html", None),
            (b"text/html;\",text/plain", b"text/html", None),
            (b"text/html;\",\\\",text/plain", b"text/html", None),
            (
                b"text/html;\",\\\",text/plain,\";charset=GBK",
                b"text/html",
                Some(b"GBK"),
            ),
            (b"text/html;\",\",text/plain", b"text/plain", None),
        ];

        for (index, (value, essence, charset)) in cases.into_iter().enumerate() {
            let extracted = extract_mime_type(value)
                .expect("bounded MIME extraction")
                .unwrap_or_else(|| panic!("contentTypes{} did not parse", index + 1));
            assert_eq!(
                extracted.essence.type_name.len() + 1 + extracted.essence.subtype.len(),
                essence.len()
            );
            let slash = essence
                .iter()
                .position(|byte| *byte == b'/')
                .expect("expected essence slash");
            assert!(
                extracted
                    .essence
                    .type_name
                    .eq_ignore_ascii_case(&essence[..slash])
            );
            assert!(
                extracted
                    .essence
                    .subtype
                    .eq_ignore_ascii_case(&essence[slash + 1..])
            );
            assert_eq!(
                extracted.charset.as_deref(),
                charset,
                "contentTypes{}",
                index + 1
            );
        }
    }

    #[test]
    fn shared_initial_and_redirect_mixed_content_gate_is_syntactic_only() {
        for admitted in [
            "http://localhost/style.css",
            "http://localhost./style.css",
            "http://sub.localhost/style.css",
            "http://sub.localhost./style.css",
            "http://127.0.0.1/style.css",
            "http://127.255.255.254/style.css",
            "http://[::1]/style.css",
        ] {
            let target = GeneralWebTarget::parse(admitted).expect("parse admitted loopback target");
            assert_eq!(
                validate_mixed_content(WebScheme::Https, &target),
                Ok(()),
                "{admitted}"
            );
        }

        for rejected in [
            "http://0.0.0.0/style.css",
            "http://126.255.255.255/style.css",
            "http://128.0.0.1/style.css",
            "http://[::ffff:127.0.0.1]/style.css",
            "http://example.test/style.css",
        ] {
            let target = GeneralWebTarget::parse(rejected).expect("parse rejected target");
            assert_eq!(
                validate_mixed_content(WebScheme::Https, &target),
                Err(StyleFetchRejection::MixedContent),
                "{rejected}"
            );
        }
    }
}
