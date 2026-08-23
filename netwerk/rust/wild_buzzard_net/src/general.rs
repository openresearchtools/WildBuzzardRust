// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::{
    collections::HashSet,
    fmt,
    io::{self, Read, Write},
    net::{IpAddr, SocketAddr, TcpStream},
    os::fd::OwnedFd,
    sync::{
        Arc,
        mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel},
    },
    thread,
    time::{Duration, Instant},
};

use hickory_resolver::{TokioResolver, config::LookupIpStrategy, proto::rr::Name};
use mio::{Events, Interest, Poll, Token, net::TcpStream as MioTcpStream};
use rustls::{
    ClientConfig as TlsClientConfig, ClientConnection, ProtocolVersion, RootCertStore, StreamOwned,
    pki_types::{CertificateDer, ServerName},
};

use crate::{
    Body, CancellationSource, CancellationToken, ClientConfig, DnsFailure, Error, GeneralWebTarget,
    HeaderName, HeaderValue, Headers, LimitKind, Method, Operation, RedirectPolicy, Response,
    ResponseHead, Result, TlsFailure, TrustStoreFailure, WebHost, WebScheme,
    client::{
        WireRequest, WireStream, check_control, execute_prepared, next_wait, prepare_request,
    },
    error::classify_rustls_error,
    message::is_reserved_request_header,
};

const DEFAULT_DNS_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_MAX_DNS_CANDIDATES: usize = 32;
const DEFAULT_MAX_CONNECTION_ATTEMPTS: usize = 16;
const DEFAULT_MAX_TLS_HANDSHAKE_BYTES: usize = 1024 * 1024;
const DNS_CACHE_ENTRIES: u64 = 256;
const DNS_MAX_ACTIVE_REQUESTS: usize = 32;
const DNS_ATTEMPTS: usize = 2;
const DNS_WORK_QUEUE: usize = 32;
const TLS_BUFFER_BYTES: usize = 64 * 1024;
const HTTP_11_ALPN: &[u8] = b"http/1.1";
const CONNECT_TOKEN: Token = Token(0);

// Firefox ESR153 `nsIOService.cpp` `gBadPortList`, which in turn tracks the
// Fetch restricted-port table. This is deliberately fixed policy rather than
// caller configuration.
const RESTRICTED_WEB_PORTS: &[u16] = &[
    1, 7, 9, 11, 13, 15, 17, 19, 20, 21, 22, 23, 25, 37, 42, 43, 53, 69, 77, 79, 87, 95, 101, 102,
    103, 104, 109, 110, 111, 113, 115, 117, 119, 123, 135, 137, 139, 143, 161, 179, 389, 427, 465,
    512, 513, 514, 515, 526, 530, 531, 532, 540, 548, 554, 556, 563, 587, 601, 636, 989, 990, 993,
    995, 1719, 1720, 1723, 2049, 3659, 4045, 4190, 5060, 5061, 6000, 6566, 6665, 6666, 6667, 6668,
    6669, 6679, 6697, 10080,
];

/// Network address space assigned to one concrete IP address.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IpAddressSpace {
    /// No authenticated IP evidence is available.
    Unknown,
    /// An ordinary globally scoped IP address.
    Public,
    /// A private, shared, link-local, or unique-local IP address.
    Private,
    /// A loopback or unspecified IP address.
    Local,
}

/// Permission state supplied for a more-private network transition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocalNetworkPermission {
    /// No permission evidence was supplied.
    Unknown,
    /// A permission decision is not final.
    Pending,
    /// Permission was explicitly denied.
    Denied,
    /// Permission was explicitly granted.
    Granted,
}

/// Immutable Local Network Access permission evidence carried by a request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LocalNetworkAccessPermissions {
    local: LocalNetworkPermission,
    private: LocalNetworkPermission,
}

impl LocalNetworkAccessPermissions {
    /// Creates explicit permission evidence for Local and Private targets.
    #[must_use]
    pub const fn new(local: LocalNetworkPermission, private: LocalNetworkPermission) -> Self {
        Self { local, private }
    }

    /// Creates an explicit deny-all permission record.
    #[must_use]
    pub const fn deny_all() -> Self {
        Self::new(
            LocalNetworkPermission::Denied,
            LocalNetworkPermission::Denied,
        )
    }

    /// Returns the permission evidence for a Local target.
    #[must_use]
    pub const fn local(self) -> LocalNetworkPermission {
        self.local
    }

    /// Returns the permission evidence for a Private target.
    #[must_use]
    pub const fn private(self) -> LocalNetworkPermission {
        self.private
    }
}

#[derive(Debug)]
struct GeneralWebClientIdentity;

struct CommittedResponseAuthorityInner {
    client_identity: Arc<GeneralWebClientIdentity>,
    response_identity: Arc<()>,
    target: GeneralWebTarget,
    security: ConnectionSecurity,
    address_space: IpAddressSpace,
}

/// Opaque authority for one exact transport response.
///
/// The transport issues this only after one concrete candidate was connected
/// and a response head was parsed. It binds the issuing client capability, a
/// never-reused response identity, the exact fragment-free request URL and
/// origin, connection security, and classified peer address space. Cloning
/// preserves that exact authority; independently issued authorities never
/// compare equal, even for byte-identical URLs and responses.
///
/// This authority is deliberately not [`Copy`]. It has no public constructor,
/// and its debug representation never exposes the request URL.
///
/// ```compile_fail
/// use wild_buzzard_net::CommittedResponseAuthority;
///
/// fn cannot_copy(authority: CommittedResponseAuthority) {
///     let first = authority;
///     let second = authority;
///     drop((first, second));
/// }
/// ```
#[derive(Clone)]
pub struct CommittedResponseAuthority {
    inner: Arc<CommittedResponseAuthorityInner>,
}

impl CommittedResponseAuthority {
    fn issue(
        client_identity: Arc<GeneralWebClientIdentity>,
        target: GeneralWebTarget,
        security: ConnectionSecurity,
        address_space: IpAddressSpace,
    ) -> Self {
        Self {
            inner: Arc::new(CommittedResponseAuthorityInner {
                client_identity,
                response_identity: Arc::new(()),
                target,
                security,
                address_space,
            }),
        }
    }

    fn is_issued_by(&self, client_identity: &Arc<GeneralWebClientIdentity>) -> bool {
        Arc::ptr_eq(&self.inner.client_identity, client_identity)
    }

    /// Whether this authority names the exact fragment-free request target.
    #[must_use]
    pub fn matches_target(&self, target: &GeneralWebTarget) -> bool {
        self.inner.target.url().as_str() == target.url().as_str()
            && self.inner.target.origin() == target.origin()
            && self.inner.target.request_target() == target.request_target()
    }

    /// Returns the exact response connection's authenticated or cleartext state.
    #[must_use]
    pub fn security(&self) -> ConnectionSecurity {
        self.inner.security
    }

    /// Returns the exact connected peer's address-space classification.
    #[must_use]
    pub fn address_space(&self) -> IpAddressSpace {
        self.inner.address_space
    }
}

impl PartialEq for CommittedResponseAuthority {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(
            &self.inner.response_identity,
            &other.inner.response_identity,
        ) && Arc::ptr_eq(&self.inner.client_identity, &other.inner.client_identity)
    }
}

impl Eq for CommittedResponseAuthority {}

impl fmt::Debug for CommittedResponseAuthority {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CommittedResponseAuthority")
            .field("security", &self.inner.security)
            .field("address_space", &self.inner.address_space)
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
enum InitiatorAddressSpaceEvidence {
    BrowserNavigation {
        client_identity: Arc<GeneralWebClientIdentity>,
    },
    CommittedDocument(CommittedResponseAuthority),
    #[cfg(test)]
    InheritedUnitTest(IpAddressSpace),
}

/// Explicit immutable address-space and permission context for one request.
///
/// There is intentionally no `Default`. Committed-response access is issued
/// only by the exact [`GeneralWebClient`] capability (or an identity-preserving
/// delegated client) which later validates it.
#[derive(Clone)]
pub struct GeneralWebNetworkAccess {
    initiator: InitiatorAddressSpaceEvidence,
    permissions: LocalNetworkAccessPermissions,
}

impl GeneralWebNetworkAccess {
    #[cfg(test)]
    fn inherited_unit_test() -> Self {
        Self {
            initiator: InitiatorAddressSpaceEvidence::InheritedUnitTest(IpAddressSpace::Local),
            permissions: LocalNetworkAccessPermissions::deny_all(),
        }
    }

    #[cfg(test)]
    fn committed_unit_test(
        parent: IpAddressSpace,
        permissions: LocalNetworkAccessPermissions,
    ) -> Self {
        Self {
            initiator: InitiatorAddressSpaceEvidence::InheritedUnitTest(parent),
            permissions,
        }
    }

    fn parent_address_space(
        &self,
        client_identity: &Arc<GeneralWebClientIdentity>,
    ) -> std::result::Result<IpAddressSpace, GeneralWebPolicyError> {
        match &self.initiator {
            InitiatorAddressSpaceEvidence::BrowserNavigation {
                client_identity: expected,
            } if Arc::ptr_eq(expected, client_identity) => Ok(IpAddressSpace::Local),
            InitiatorAddressSpaceEvidence::BrowserNavigation { .. } => {
                Err(GeneralWebPolicyError::InvalidInitiatorEvidence)
            }
            InitiatorAddressSpaceEvidence::CommittedDocument(authority)
                if authority.is_issued_by(client_identity) =>
            {
                Ok(authority.address_space())
            }
            InitiatorAddressSpaceEvidence::CommittedDocument(_) => {
                Err(GeneralWebPolicyError::InvalidInitiatorEvidence)
            }
            #[cfg(test)]
            InitiatorAddressSpaceEvidence::InheritedUnitTest(parent) => Ok(*parent),
        }
    }

    const fn permissions(&self) -> LocalNetworkAccessPermissions {
        self.permissions
    }
}

impl fmt::Debug for GeneralWebNetworkAccess {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let initiator = match &self.initiator {
            InitiatorAddressSpaceEvidence::BrowserNavigation { .. } => "browser-navigation",
            InitiatorAddressSpaceEvidence::CommittedDocument(_) => "committed-document",
            #[cfg(test)]
            InitiatorAddressSpaceEvidence::InheritedUnitTest(_) => "inherited-unit-test",
        };
        formatter
            .debug_struct("GeneralWebNetworkAccess")
            .field("initiator", &initiator)
            .field("permissions", &self.permissions)
            .finish_non_exhaustive()
    }
}

/// More-private target whose explicit permission was required.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocalNetworkTarget {
    /// A loopback or unspecified target.
    Local,
    /// A private, shared, link-local, or unique-local target.
    Private,
}

/// Privacy-safe policy failure raised before a prohibited connection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GeneralWebPolicyError {
    /// The target uses a Fetch/Firefox restricted port.
    RestrictedPort,
    /// Browser-initiator evidence belonged to a different client capability.
    InvalidInitiatorEvidence,
    /// A more-private transition lacked an exact granted permission.
    LocalNetworkAccessDenied {
        /// Authenticated initiator address space.
        parent: IpAddressSpace,
        /// Classified candidate address space.
        target: IpAddressSpace,
        /// Permission family required by this transition.
        required: LocalNetworkTarget,
        /// Exact supplied permission state.
        permission: LocalNetworkPermission,
    },
}

impl fmt::Display for GeneralWebPolicyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RestrictedPort => formatter.write_str("general-web target port is restricted"),
            Self::InvalidInitiatorEvidence => {
                formatter.write_str("general-web initiator evidence is invalid")
            }
            Self::LocalNetworkAccessDenied { .. } => {
                formatter.write_str("general-web local network access denied")
            }
        }
    }
}

impl std::error::Error for GeneralWebPolicyError {}

/// Redacted transport failure returned by policy-aware execution.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GeneralWebTransportFailure {
    /// Request validation or serialization failed.
    Request,
    /// Cooperative cancellation was observed.
    Cancelled,
    /// A bounded operation timed out.
    Timeout(Operation),
    /// DNS resolution failed.
    Dns(DnsFailure),
    /// TCP connection establishment or socket I/O failed.
    Connection,
    /// TLS setup, authentication, or protocol handling failed.
    Tls(TlsFailure),
    /// A transport resource limit was reached.
    Limit {
        /// Bounded resource family.
        kind: LimitKind,
        /// Configured maximum.
        limit: usize,
    },
    /// HTTP response syntax or framing failed.
    HttpProtocol,
    /// Redirect exposure was prohibited by the request policy.
    RedirectRejected(u16),
}

impl GeneralWebTransportFailure {
    fn from_transport(error: &Error) -> Self {
        match error {
            Error::Cancelled => Self::Cancelled,
            Error::Timeout(operation) => Self::Timeout(*operation),
            Error::Dns(failure) => Self::Dns(*failure),
            Error::Io { .. } | Error::ConnectAttemptsExhausted { .. } => Self::Connection,
            Error::Tls(failure) => Self::Tls(*failure),
            Error::LimitExceeded { kind, limit } => Self::Limit {
                kind: *kind,
                limit: *limit,
            },
            Error::RedirectRejected(status) => Self::RedirectRejected(*status),
            Error::TrustStore(_)
            | Error::InvalidUrl(_)
            | Error::UnsupportedScheme(_)
            | Error::CredentialsNotAllowed
            | Error::FragmentNotAllowed
            | Error::NonLoopbackTarget
            | Error::MissingPort
            | Error::MissingHost
            | Error::InvalidRequestTarget
            | Error::InvalidMethod
            | Error::InvalidHeaderName
            | Error::InvalidHeaderValue
            | Error::ReservedRequestHeader(_) => Self::Request,
            Error::InvalidLineEnding
            | Error::MalformedStatusLine
            | Error::MalformedHeader
            | Error::ObsoleteLineFolding
            | Error::ConflictingContentLength
            | Error::AmbiguousBodyFraming
            | Error::InvalidContentLength
            | Error::UnsupportedTransferCoding(_)
            | Error::UnsupportedContentCoding(_)
            | Error::MalformedChunkSize
            | Error::ProhibitedTrailer(_)
            | Error::PrematureEof
            | Error::ProtocolSwitchUnsupported => Self::HttpProtocol,
        }
    }
}

/// Typed privacy-safe failure from policy-aware general-web execution.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GeneralWebExecutionError {
    /// A pre-DNS or pre-connect security policy denied the request.
    Policy(GeneralWebPolicyError),
    /// The admitted request failed in bounded transport processing.
    Transport(GeneralWebTransportFailure),
}

impl fmt::Display for GeneralWebExecutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Policy(error) => error.fmt(formatter),
            Self::Transport(error) => write!(formatter, "general-web transport failed: {error:?}"),
        }
    }
}

impl std::error::Error for GeneralWebExecutionError {}

enum InternalExecutionError {
    Policy(GeneralWebPolicyError),
    Transport(Error),
}

impl From<Error> for InternalExecutionError {
    fn from(error: Error) -> Self {
        Self::Transport(error)
    }
}

impl InternalExecutionError {
    fn redacted(&self) -> GeneralWebExecutionError {
        match self {
            Self::Policy(error) => GeneralWebExecutionError::Policy(*error),
            Self::Transport(error) => GeneralWebExecutionError::Transport(
                GeneralWebTransportFailure::from_transport(error),
            ),
        }
    }

    fn into_legacy(self) -> Error {
        match self {
            Self::Policy(_) => Error::ConnectAttemptsExhausted {
                attempted: 0,
                last_kind: Some(io::ErrorKind::PermissionDenied),
            },
            Self::Transport(error) => error,
        }
    }
}

/// Returns whether `port` is blocked by the pinned ESR153 restricted-port set.
#[must_use]
pub fn is_restricted_web_port(port: u16) -> bool {
    RESTRICTED_WEB_PORTS.binary_search(&port).is_ok()
}

/// Classifies one resolved candidate without DNS-derived trust promotion.
#[must_use]
pub fn classify_ip_address_space(address: IpAddr) -> IpAddressSpace {
    match address {
        IpAddr::V4(address) => classify_ipv4_address_space(address),
        IpAddr::V6(address) => address.to_ipv4_mapped().map_or_else(
            || classify_ipv6_address_space(address),
            classify_ipv4_address_space,
        ),
    }
}

const fn classify_ipv4_address_space(address: std::net::Ipv4Addr) -> IpAddressSpace {
    let [first, second, ..] = address.octets();
    if address.is_loopback() || address.is_unspecified() {
        IpAddressSpace::Local
    } else if first == 0
        || first == 10
        || (first == 172 && second >= 16 && second <= 31)
        || (first == 192 && second == 168)
        || (first == 100 && second >= 64 && second <= 127)
        || (first == 169 && second == 254)
    {
        IpAddressSpace::Private
    } else {
        IpAddressSpace::Public
    }
}

const fn classify_ipv6_address_space(address: std::net::Ipv6Addr) -> IpAddressSpace {
    let first = address.segments()[0];
    if address.is_loopback() || address.is_unspecified() {
        IpAddressSpace::Local
    } else if first & 0xfe00 == 0xfc00 || first & 0xffc0 == 0xfe80 {
        IpAddressSpace::Private
    } else {
        IpAddressSpace::Public
    }
}

fn authorize_address_space_transition(
    parent: IpAddressSpace,
    target: IpAddressSpace,
    permissions: LocalNetworkAccessPermissions,
) -> std::result::Result<(), GeneralWebPolicyError> {
    if parent == IpAddressSpace::Unknown || target == IpAddressSpace::Unknown {
        return Err(GeneralWebPolicyError::InvalidInitiatorEvidence);
    }
    let required = match (parent, target) {
        (IpAddressSpace::Public | IpAddressSpace::Private, IpAddressSpace::Local) => {
            Some((LocalNetworkTarget::Local, permissions.local()))
        }
        (IpAddressSpace::Public, IpAddressSpace::Private) => {
            Some((LocalNetworkTarget::Private, permissions.private()))
        }
        _ => None,
    };
    let Some((required, permission)) = required else {
        return Ok(());
    };
    if permission == LocalNetworkPermission::Granted {
        Ok(())
    } else {
        Err(GeneralWebPolicyError::LocalNetworkAccessDenied {
            parent,
            target,
            required,
            permission,
        })
    }
}

/// An authenticated trust-anchor set for [`GeneralWebClient`].
///
/// The only public constructor starts with the bundled Web PKI roots. Extra
/// DER certificates can be added for locally administered roots; the
/// authenticated verifier cannot be replaced or disabled through this API.
#[derive(Clone)]
pub struct TrustStore {
    roots: RootCertStore,
}

impl TrustStore {
    /// Creates a trust store from the crate's pinned `webpki-roots` snapshot.
    #[must_use]
    pub fn bundled_web_pki() -> Self {
        Self {
            roots: webpki_roots::TLS_SERVER_ROOTS.iter().cloned().collect(),
        }
    }

    /// Adds one DER-encoded X.509 trust anchor.
    ///
    /// This is additive: bundled roots remain present.
    ///
    /// # Errors
    ///
    /// Returns [`Error::TrustStore`] when the certificate cannot be parsed as
    /// a usable trust anchor.
    pub fn add_der_certificate(&mut self, certificate: &[u8]) -> Result<()> {
        self.roots
            .add(CertificateDer::from(certificate))
            .map_err(|_| Error::TrustStore(TrustStoreFailure::InvalidCertificate))
    }

    /// Builder form of [`Self::add_der_certificate`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::TrustStore`] when the certificate cannot be parsed as
    /// a usable trust anchor.
    pub fn with_der_certificate(mut self, certificate: &[u8]) -> Result<Self> {
        self.add_der_certificate(certificate)?;
        Ok(self)
    }

    /// Returns the number of configured trust anchors.
    #[must_use]
    pub fn anchor_count(&self) -> usize {
        self.roots.len()
    }
}

impl Default for TrustStore {
    fn default() -> Self {
        Self::bundled_web_pki()
    }
}

impl fmt::Debug for TrustStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TrustStore")
            .field("anchor_count", &self.anchor_count())
            .finish_non_exhaustive()
    }
}

/// DNS, TCP, TLS, and HTTP limits for [`GeneralWebClient`].
#[derive(Clone, Debug)]
pub struct GeneralWebConfig {
    http: ClientConfig,
    dns_timeout: Duration,
    tls_handshake_timeout: Duration,
    max_dns_candidates: usize,
    max_connection_attempts: usize,
    max_tls_handshake_bytes: usize,
}

impl Default for GeneralWebConfig {
    fn default() -> Self {
        Self {
            http: ClientConfig::default(),
            dns_timeout: DEFAULT_DNS_TIMEOUT,
            tls_handshake_timeout: DEFAULT_TLS_HANDSHAKE_TIMEOUT,
            max_dns_candidates: DEFAULT_MAX_DNS_CANDIDATES,
            max_connection_attempts: DEFAULT_MAX_CONNECTION_ATTEMPTS,
            max_tls_handshake_bytes: DEFAULT_MAX_TLS_HANDSHAKE_BYTES,
        }
    }
}

impl GeneralWebConfig {
    /// Constructs the version-1 explicitly bounded general-web policy.
    ///
    /// Every private field is an argument and is repeated in the exhaustive
    /// `Self` literal below. Adding a field to `GeneralWebConfig` therefore
    /// fails compilation until this constructor is audited, extended, and
    /// each caller supplies the new policy value. Numeric quotas may be zero
    /// to deny that resource. DNS and TLS timeouts must be nonzero. Every
    /// general-web value must be at or below the version-1 hard maximum.
    ///
    /// Callers needing the same drift protection for the nested HTTP policy
    /// must construct `http` with [`ClientConfig::try_new_explicit_v1`]. This
    /// constructor intentionally does not call [`Self::default`].
    #[must_use]
    pub fn try_new_explicit_v1(
        http: ClientConfig,
        dns_timeout: Duration,
        tls_handshake_timeout: Duration,
        max_dns_candidates: usize,
        max_connection_attempts: usize,
        max_tls_handshake_bytes: usize,
    ) -> Option<Self> {
        if !http.is_within_explicit_v1_bounds()
            || dns_timeout.is_zero()
            || dns_timeout > DEFAULT_DNS_TIMEOUT
            || tls_handshake_timeout.is_zero()
            || tls_handshake_timeout > DEFAULT_TLS_HANDSHAKE_TIMEOUT
            || max_dns_candidates > DEFAULT_MAX_DNS_CANDIDATES
            || max_connection_attempts > DEFAULT_MAX_CONNECTION_ATTEMPTS
            || max_tls_handshake_bytes > DEFAULT_MAX_TLS_HANDSHAKE_BYTES
        {
            return None;
        }
        Some(Self {
            http,
            dns_timeout,
            tls_handshake_timeout,
            max_dns_candidates,
            max_connection_attempts,
            max_tls_handshake_bytes,
        })
    }

    /// Replaces the shared strict HTTP/1.1 parser and I/O policy.
    #[must_use]
    pub fn with_http_config(mut self, config: ClientConfig) -> Self {
        self.http = config;
        self
    }

    /// Sets the total DNS lookup timeout, including resolver admission.
    #[must_use]
    pub const fn with_dns_timeout(mut self, timeout: Duration) -> Self {
        self.dns_timeout = timeout;
        self
    }

    /// Sets the total TLS handshake timeout for each address attempt.
    #[must_use]
    pub const fn with_tls_handshake_timeout(mut self, timeout: Duration) -> Self {
        self.tls_handshake_timeout = timeout;
        self
    }

    /// Sets the maximum unique A/AAAA candidates accepted from one lookup.
    #[must_use]
    pub const fn with_max_dns_candidates(mut self, limit: usize) -> Self {
        self.max_dns_candidates = limit;
        self
    }

    /// Sets the maximum number of address candidates attempted per request.
    #[must_use]
    pub const fn with_max_connection_attempts(mut self, limit: usize) -> Self {
        self.max_connection_attempts = limit;
        self
    }

    /// Sets the aggregate TLS handshake wire-byte limit per address attempt.
    #[must_use]
    pub const fn with_max_tls_handshake_bytes(mut self, limit: usize) -> Self {
        self.max_tls_handshake_bytes = limit;
        self
    }

    /// Returns the strict HTTP/1.1 parser and I/O policy.
    #[must_use]
    pub const fn http_config(&self) -> &ClientConfig {
        &self.http
    }

    /// Returns the total DNS timeout.
    #[must_use]
    pub const fn dns_timeout(&self) -> Duration {
        self.dns_timeout
    }

    /// Returns the per-address TLS handshake timeout.
    #[must_use]
    pub const fn tls_handshake_timeout(&self) -> Duration {
        self.tls_handshake_timeout
    }

    /// Returns the unique DNS candidate limit.
    #[must_use]
    pub const fn max_dns_candidates(&self) -> usize {
        self.max_dns_candidates
    }

    /// Returns the connection-attempt limit.
    #[must_use]
    pub const fn max_connection_attempts(&self) -> usize {
        self.max_connection_attempts
    }

    /// Returns the per-address TLS handshake wire-byte limit.
    #[must_use]
    pub const fn max_tls_handshake_bytes(&self) -> usize {
        self.max_tls_handshake_bytes
    }
}

/// An owned request for the separate general-web transport capability.
#[derive(Clone, Debug)]
pub struct GeneralWebRequest {
    method: Method,
    target: GeneralWebTarget,
    network_access: GeneralWebNetworkAccess,
    headers: Headers,
    body: Vec<u8>,
    redirect_policy: RedirectPolicy,
    cancellation: CancellationToken,
    deadline: Option<Instant>,
}

impl GeneralWebRequest {
    /// Creates a request with explicit redirect and network-access policy.
    #[must_use]
    #[cfg(not(test))]
    pub fn new(
        method: Method,
        target: GeneralWebTarget,
        redirect_policy: RedirectPolicy,
        network_access: GeneralWebNetworkAccess,
    ) -> Self {
        Self::new_with_network_access(method, target, redirect_policy, network_access)
    }

    #[cfg(test)]
    fn new(method: Method, target: GeneralWebTarget, redirect_policy: RedirectPolicy) -> Self {
        Self::new_with_network_access(
            method,
            target,
            redirect_policy,
            GeneralWebNetworkAccess::inherited_unit_test(),
        )
    }

    /// Creates a request through the explicitly named network-access seam.
    #[must_use]
    pub fn new_with_network_access(
        method: Method,
        target: GeneralWebTarget,
        redirect_policy: RedirectPolicy,
        network_access: GeneralWebNetworkAccess,
    ) -> Self {
        Self {
            method,
            target,
            network_access,
            headers: Headers::new(),
            body: Vec::new(),
            redirect_policy,
            cancellation: CancellationSource::new().token(),
            deadline: None,
        }
    }

    /// Creates a bodyless `GET` with explicit redirect and network-access policy.
    #[must_use]
    #[cfg(not(test))]
    pub fn get(
        target: GeneralWebTarget,
        redirect_policy: RedirectPolicy,
        network_access: GeneralWebNetworkAccess,
    ) -> Self {
        Self::new(Method::get(), target, redirect_policy, network_access)
    }

    /// Unit-test compatibility constructor for the inherited transport suite.
    #[must_use]
    #[cfg(test)]
    fn get(target: GeneralWebTarget, redirect_policy: RedirectPolicy) -> Self {
        Self::new(Method::get(), target, redirect_policy)
    }

    /// Creates a bodyless `GET` through the explicitly named access seam.
    #[must_use]
    pub fn get_with_network_access(
        target: GeneralWebTarget,
        redirect_policy: RedirectPolicy,
        network_access: GeneralWebNetworkAccess,
    ) -> Self {
        Self::new_with_network_access(Method::get(), target, redirect_policy, network_access)
    }

    /// Adds a validated caller-owned request header.
    ///
    /// `Host`, framing, connection, and content-coding fields remain owned by
    /// the transport.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ReservedRequestHeader`] for a transport-owned field.
    pub fn append_header(&mut self, name: HeaderName, value: HeaderValue) -> Result<()> {
        if is_reserved_request_header(&name) {
            return Err(Error::ReservedRequestHeader(name.as_str().to_owned()));
        }
        self.headers.append(name, value);
        Ok(())
    }

    /// Builder form of [`Self::append_header`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::ReservedRequestHeader`] for a transport-owned field.
    pub fn with_header(mut self, name: HeaderName, value: HeaderValue) -> Result<Self> {
        self.append_header(name, value)?;
        Ok(self)
    }

    /// Replaces the outgoing body.
    #[must_use]
    pub fn with_body(mut self, body: Vec<u8>) -> Self {
        self.body = body;
        self
    }

    /// Uses a cancellation token supplied by the caller.
    #[must_use]
    pub fn with_cancellation(mut self, cancellation: CancellationToken) -> Self {
        self.cancellation = cancellation;
        self
    }

    /// Sets an absolute deadline covering DNS, connection, TLS, and body I/O.
    #[must_use]
    pub fn with_deadline(mut self, deadline: Instant) -> Self {
        self.deadline = Some(deadline);
        self
    }

    /// Returns the validated method.
    #[must_use]
    pub const fn method(&self) -> &Method {
        &self.method
    }

    /// Returns the validated general-web target.
    #[must_use]
    pub const fn target(&self) -> &GeneralWebTarget {
        &self.target
    }

    /// Returns the immutable initiator and permission context.
    #[must_use]
    pub const fn network_access(&self) -> &GeneralWebNetworkAccess {
        &self.network_access
    }

    /// Returns caller-supplied request fields.
    #[must_use]
    pub const fn headers(&self) -> &Headers {
        &self.headers
    }

    /// Returns the outgoing body.
    #[must_use]
    pub fn body(&self) -> &[u8] {
        &self.body
    }

    /// Returns the caller's explicit redirect policy.
    #[must_use]
    pub const fn redirect_policy(&self) -> RedirectPolicy {
        self.redirect_policy
    }

    /// Returns the cooperative cancellation token.
    #[must_use]
    pub const fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }

    /// Returns the absolute request deadline, when configured.
    #[must_use]
    pub const fn deadline(&self) -> Option<Instant> {
        self.deadline
    }
}

impl WireRequest for GeneralWebRequest {
    fn method(&self) -> &Method {
        self.method()
    }

    fn headers(&self) -> &Headers {
        self.headers()
    }

    fn body(&self) -> &[u8] {
        self.body()
    }

    fn redirect_policy(&self) -> RedirectPolicy {
        self.redirect_policy()
    }

    fn cancellation(&self) -> &CancellationToken {
        self.cancellation()
    }

    fn deadline(&self) -> Option<Instant> {
        self.deadline()
    }

    fn authority(&self) -> &str {
        self.target.origin().authority()
    }

    fn request_target(&self) -> &str {
        self.target.request_target().as_str()
    }
}

/// TLS protocol version authenticated for a response connection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TlsVersion {
    /// TLS 1.2.
    Tls12,
    /// TLS 1.3.
    Tls13,
}

/// HTTP/1.1 ALPN outcome for an authenticated TLS connection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AlpnOutcome {
    /// The peer explicitly selected `http/1.1`.
    Http11,
    /// The peer selected no application protocol, permitting HTTP/1.1.
    NotNegotiated,
}

/// Security properties of the connection that produced a response.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectionSecurity {
    /// The caller explicitly requested a cleartext `http` URL.
    Cleartext,
    /// The connection authenticated the target under TLS 1.2 or TLS 1.3.
    Tls {
        /// Negotiated TLS version.
        version: TlsVersion,
        /// HTTP/1.1 ALPN outcome.
        alpn: AlpnOutcome,
    },
}

/// A general-web response with connection-security metadata.
#[derive(Debug)]
pub struct GeneralWebResponse {
    response: Response,
    authority: CommittedResponseAuthority,
}

impl GeneralWebResponse {
    /// Returns the final response metadata.
    #[must_use]
    pub const fn head(&self) -> &ResponseHead {
        self.response.head()
    }

    /// Returns the streaming response body.
    #[must_use]
    pub const fn body(&self) -> &Body {
        self.response.body()
    }

    /// Returns the mutable streaming response body.
    #[must_use]
    pub const fn body_mut(&mut self) -> &mut Body {
        self.response.body_mut()
    }

    /// Returns the connection's authenticated or explicit-cleartext state.
    #[must_use]
    pub fn security(&self) -> ConnectionSecurity {
        self.authority.security()
    }

    /// Returns the opaque authority for this exact response.
    ///
    /// The returned borrow can be cloned for an explicit delegation, but an
    /// unrelated client cannot redeem it.
    #[must_use]
    pub const fn response_authority(&self) -> &CommittedResponseAuthority {
        &self.authority
    }

    /// Returns the address space of the exact connected response peer.
    #[must_use]
    pub fn address_space(&self) -> IpAddressSpace {
        self.authority.address_space()
    }

    /// Splits the HTTP response from its connection-security metadata.
    #[must_use]
    pub fn into_parts(self) -> (Response, ConnectionSecurity) {
        let security = self.authority.security();
        (self.response, security)
    }

    /// Reads the bounded body to completion.
    ///
    /// # Errors
    ///
    /// Returns a cancellation, timeout, I/O, TLS, limit, or framing error.
    pub fn read_body_to_end(self) -> Result<Vec<u8>> {
        self.response.read_body_to_end()
    }
}

/// A synchronous, cancellation-aware HTTP/1.1 client with bounded DNS and TLS.
#[derive(Clone, Debug)]
pub struct GeneralWebClient {
    inner: Arc<GeneralWebClientInner>,
}

#[derive(Debug)]
struct GeneralWebClientInner {
    config: GeneralWebConfig,
    resolver: Arc<dyn HostResolver>,
    tls: Arc<TlsClientConfig>,
    client_identity: Arc<GeneralWebClientIdentity>,
}

impl GeneralWebClient {
    /// Creates a client using Linux system DNS configuration and explicit roots.
    ///
    /// # Errors
    ///
    /// Returns a typed DNS, trust-store, or TLS configuration failure. The
    /// function never installs an invalid-certificate bypass.
    pub fn new(config: GeneralWebConfig, trust_store: TrustStore) -> Result<Self> {
        let resolver = Arc::new(SystemResolver::new(&config)?);
        Self::with_resolver(config, trust_store, resolver)
    }

    /// Creates a client using the pinned bundled Web PKI trust anchors.
    ///
    /// # Errors
    ///
    /// Returns a typed DNS or TLS configuration failure.
    pub fn with_bundled_roots(config: GeneralWebConfig) -> Result<Self> {
        Self::new(config, TrustStore::bundled_web_pki())
    }

    fn with_resolver(
        config: GeneralWebConfig,
        trust_store: TrustStore,
        resolver: Arc<dyn HostResolver>,
    ) -> Result<Self> {
        let tls = Arc::new(build_tls_config(trust_store)?);
        Ok(Self {
            inner: Arc::new(GeneralWebClientInner {
                config,
                resolver,
                tls,
                client_identity: Arc::new(GeneralWebClientIdentity),
            }),
        })
    }

    /// Returns this client's immutable transport policy.
    #[must_use]
    pub fn config(&self) -> &GeneralWebConfig {
        &self.inner.config
    }

    /// Issues explicit browser-navigation initiator evidence for this client.
    ///
    /// The returned value is bound to this exact client capability and cannot
    /// be used with another independently constructed client. Subresources
    /// must instead use [`Self::network_access_for_committed_response`].
    #[must_use]
    pub fn browser_navigation_network_access(
        &self,
        permissions: LocalNetworkAccessPermissions,
    ) -> GeneralWebNetworkAccess {
        GeneralWebNetworkAccess {
            initiator: InitiatorAddressSpaceEvidence::BrowserNavigation {
                client_identity: Arc::clone(&self.inner.client_identity),
            },
            permissions,
        }
    }

    /// Creates a bounded child client which preserves this client's identity.
    ///
    /// The supplied response authority must have been issued by this client or
    /// another child carrying the same identity. The child reuses the exact
    /// resolver and TLS trust configuration while replacing all transport
    /// resource policy with `config`. This is the only supported way for a
    /// later owner to preserve response provenance while narrowing its limits.
    ///
    /// # Errors
    ///
    /// Returns a redacted policy error when `authority` belongs to an
    /// unrelated client capability.
    pub fn delegate_for_response(
        &self,
        authority: &CommittedResponseAuthority,
        config: GeneralWebConfig,
    ) -> std::result::Result<Self, GeneralWebPolicyError> {
        if !authority.is_issued_by(&self.inner.client_identity) {
            return Err(GeneralWebPolicyError::InvalidInitiatorEvidence);
        }
        Ok(Self {
            inner: Arc::new(GeneralWebClientInner {
                config,
                resolver: Arc::clone(&self.inner.resolver),
                tls: Arc::clone(&self.inner.tls),
                client_identity: Arc::clone(&self.inner.client_identity),
            }),
        })
    }

    /// Issues committed-document network access for one exact response.
    ///
    /// An independently constructed client cannot turn a detached response
    /// authority into initiator evidence. Identity-preserving delegated
    /// clients remain eligible.
    ///
    /// # Errors
    ///
    /// Returns a redacted policy error when `authority` belongs to an
    /// unrelated client capability.
    pub fn network_access_for_committed_response(
        &self,
        authority: &CommittedResponseAuthority,
        permissions: LocalNetworkAccessPermissions,
    ) -> std::result::Result<GeneralWebNetworkAccess, GeneralWebPolicyError> {
        if !authority.is_issued_by(&self.inner.client_identity) {
            return Err(GeneralWebPolicyError::InvalidInitiatorEvidence);
        }
        Ok(GeneralWebNetworkAccess {
            initiator: InitiatorAddressSpaceEvidence::CommittedDocument(authority.clone()),
            permissions,
        })
    }

    /// Resolves, connects, authenticates when requested, and sends one request.
    ///
    /// Redirects are never followed here. A returned body remains streaming and
    /// bounded by the shared strict HTTP/1.1 policy.
    ///
    /// # Errors
    ///
    /// Returns structured validation, DNS, connection, TLS, limit,
    /// cancellation, timeout, I/O, or HTTP framing failures.
    pub fn execute(&self, request: &GeneralWebRequest) -> Result<GeneralWebResponse> {
        self.execute_internal(request)
            .map_err(InternalExecutionError::into_legacy)
    }

    /// Executes with privacy-safe typed restricted-port and LNA failures.
    ///
    /// This method performs the same operation as [`Self::execute`]. The
    /// compatibility method maps policy failures to a zero-attempt permission
    /// error; security-sensitive owners should use this typed surface.
    ///
    /// # Errors
    ///
    /// Returns a typed redacted policy or transport failure. Restricted ports
    /// fail before DNS, and every resolved candidate is checked immediately
    /// before its socket attempt.
    pub fn execute_checked(
        &self,
        request: &GeneralWebRequest,
    ) -> std::result::Result<GeneralWebResponse, GeneralWebExecutionError> {
        self.execute_internal(request)
            .map_err(|error| error.redacted())
    }

    fn execute_internal(
        &self,
        request: &GeneralWebRequest,
    ) -> std::result::Result<GeneralWebResponse, InternalExecutionError> {
        if is_restricted_web_port(request.target().origin().port()) {
            return Err(InternalExecutionError::Policy(
                GeneralWebPolicyError::RestrictedPort,
            ));
        }
        let parent_address_space = request
            .network_access()
            .parent_address_space(&self.inner.client_identity)
            .map_err(InternalExecutionError::Policy)?;
        let prepared = prepare_request(request, self.config().http_config())
            .map_err(InternalExecutionError::Transport)?;
        let addresses = self
            .resolve_addresses(request)
            .map_err(InternalExecutionError::Transport)?;
        let (stream, security, address_space) =
            self.connect_transport(request, &addresses, parent_address_space)?;
        let response = execute_prepared(request, self.config().http_config(), &prepared, stream)
            .map_err(InternalExecutionError::Transport)?;
        Ok(GeneralWebResponse {
            response,
            authority: CommittedResponseAuthority::issue(
                Arc::clone(&self.inner.client_identity),
                request.target().clone(),
                security,
                address_space,
            ),
        })
    }

    fn resolve_addresses(&self, request: &GeneralWebRequest) -> Result<Vec<SocketAddr>> {
        let origin = request.target().origin();
        if let Some(address) = origin.host().ip_addr() {
            check_control(
                request.cancellation(),
                request.deadline(),
                Operation::Connect,
            )?;
            return Ok(vec![SocketAddr::new(address, origin.port())]);
        }
        check_control(
            request.cancellation(),
            request.deadline(),
            Operation::ResolveDns,
        )?;
        let domain = origin
            .host()
            .domain()
            .ok_or(Error::Dns(DnsFailure::InvalidName))?;
        let addresses = self.inner.resolver.resolve(
            domain,
            self.config().dns_timeout,
            self.config().max_dns_candidates,
            request.cancellation(),
            request.deadline(),
        )?;
        normalize_addresses(addresses, origin.port(), self.config().max_dns_candidates)
    }

    fn connect_transport(
        &self,
        request: &GeneralWebRequest,
        addresses: &[SocketAddr],
        parent_address_space: IpAddressSpace,
    ) -> std::result::Result<
        (TransportStream, ConnectionSecurity, IpAddressSpace),
        InternalExecutionError,
    > {
        let attempt_limit = self.config().max_connection_attempts;
        if attempt_limit == 0 {
            return Err(Error::LimitExceeded {
                kind: LimitKind::ConnectionAttempts,
                limit: attempt_limit,
            }
            .into());
        }

        let mut attempted = 0_usize;
        let mut last_io_kind = None;
        let mut last_tls_error = None;
        for address in addresses.iter().copied().take(attempt_limit) {
            let address_space = classify_ip_address_space(address.ip());
            authorize_address_space_transition(
                parent_address_space,
                address_space,
                request.network_access().permissions(),
            )
            .map_err(InternalExecutionError::Policy)?;
            attempted += 1;
            let socket = match connect_general_interruptible(
                address,
                self.config().http.connect_timeout(),
                request.cancellation(),
                request.deadline(),
            ) {
                Ok(socket) => socket,
                Err(Error::Cancelled) => return Err(Error::Cancelled.into()),
                Err(Error::Timeout(Operation::Connect)) => {
                    check_control(
                        request.cancellation(),
                        request.deadline(),
                        Operation::Connect,
                    )?;
                    last_io_kind = Some(io::ErrorKind::TimedOut);
                    continue;
                }
                Err(Error::Io {
                    operation: Operation::Connect,
                    kind,
                }) => {
                    last_io_kind = Some(kind);
                    continue;
                }
                Err(error) => return Err(error.into()),
            };

            if request.target().origin().scheme() == WebScheme::Http {
                return Ok((
                    TransportStream::Cleartext(socket),
                    ConnectionSecurity::Cleartext,
                    address_space,
                ));
            }

            match establish_tls(
                socket,
                request.target().origin().host(),
                self.inner.tls.clone(),
                self.config(),
                request.cancellation(),
                request.deadline(),
            ) {
                Ok((stream, security)) => {
                    return Ok((
                        TransportStream::Tls(Box::new(stream)),
                        security,
                        address_space,
                    ));
                }
                Err(Error::Cancelled) => return Err(Error::Cancelled.into()),
                Err(error @ Error::Timeout(Operation::TlsHandshake)) => {
                    check_control(
                        request.cancellation(),
                        request.deadline(),
                        Operation::TlsHandshake,
                    )?;
                    last_tls_error = Some(error);
                }
                Err(error @ (Error::Tls(_) | Error::Io { .. } | Error::LimitExceeded { .. })) => {
                    last_tls_error = Some(error);
                }
                Err(error) => return Err(error.into()),
            }
        }

        if addresses.len() > attempt_limit {
            return Err(Error::LimitExceeded {
                kind: LimitKind::ConnectionAttempts,
                limit: attempt_limit,
            }
            .into());
        }
        if let Some(error) = last_tls_error {
            return Err(error.into());
        }
        Err(Error::ConnectAttemptsExhausted {
            attempted,
            last_kind: last_io_kind,
        }
        .into())
    }
}

trait PendingConnection {
    fn poll_connected(&mut self, timeout: Duration) -> io::Result<bool>;
}

#[derive(Debug)]
struct MioConnectAttempt {
    address: SocketAddr,
    poll: Poll,
    events: Events,
    stream: MioTcpStream,
}

impl MioConnectAttempt {
    fn start(address: SocketAddr) -> io::Result<Self> {
        let mut stream = MioTcpStream::connect(address)?;
        let poll = Poll::new()?;
        poll.registry()
            .register(&mut stream, CONNECT_TOKEN, Interest::WRITABLE)?;
        Ok(Self {
            address,
            poll,
            events: Events::with_capacity(4),
            stream,
        })
    }

    fn into_std(self) -> io::Result<TcpStream> {
        let Self {
            poll, mut stream, ..
        } = self;
        poll.registry().deregister(&mut stream)?;
        let descriptor: OwnedFd = stream.into();
        let stream = TcpStream::from(descriptor);
        stream.set_nonblocking(false)?;
        Ok(stream)
    }
}

impl PendingConnection for MioConnectAttempt {
    fn poll_connected(&mut self, timeout: Duration) -> io::Result<bool> {
        match self.poll.poll(&mut self.events, Some(timeout)) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => return Ok(false),
            Err(error) => return Err(error),
        }
        if !self
            .events
            .iter()
            .any(|event| event.token() == CONNECT_TOKEN)
        {
            return Ok(false);
        }
        if let Some(error) = self.stream.take_error()? {
            return Err(error);
        }
        match self.stream.peer_addr() {
            Ok(peer) if peer == self.address => Ok(true),
            Ok(_) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "connected peer did not match requested address",
            )),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotConnected | io::ErrorKind::WouldBlock
                ) =>
            {
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }
}

fn drive_connect_attempt<A, F>(
    address: SocketAddr,
    timeout: Duration,
    cancellation: &CancellationToken,
    deadline: Option<Instant>,
    start: F,
) -> Result<A>
where
    A: PendingConnection,
    F: FnOnce(SocketAddr) -> io::Result<A>,
{
    check_control(cancellation, deadline, Operation::Connect)?;
    let started = Instant::now();
    let mut attempt = start(address).map_err(|error| Error::io(Operation::Connect, &error))?;
    loop {
        check_control(cancellation, deadline, Operation::Connect)?;
        let wait = next_wait(started, timeout, deadline, Operation::Connect)?;
        if attempt
            .poll_connected(wait)
            .map_err(|error| Error::io(Operation::Connect, &error))?
        {
            check_control(cancellation, deadline, Operation::Connect)?;
            return Ok(attempt);
        }
    }
}

fn connect_general_interruptible(
    address: SocketAddr,
    timeout: Duration,
    cancellation: &CancellationToken,
    deadline: Option<Instant>,
) -> Result<TcpStream> {
    drive_connect_attempt(
        address,
        timeout,
        cancellation,
        deadline,
        MioConnectAttempt::start,
    )?
    .into_std()
    .map_err(|error| Error::io(Operation::Connect, &error))
}

fn normalize_addresses(addresses: Vec<IpAddr>, port: u16, limit: usize) -> Result<Vec<SocketAddr>> {
    let addresses = collect_bounded_unique(addresses, limit)?;
    Ok(addresses
        .into_iter()
        .map(|address| SocketAddr::new(address, port))
        .collect())
}

fn collect_bounded_unique(
    addresses: impl IntoIterator<Item = IpAddr>,
    limit: usize,
) -> Result<Vec<IpAddr>> {
    let mut unique = HashSet::new();
    let mut normalized = Vec::new();
    for address in addresses {
        if unique.insert(address) {
            if normalized.len() == limit {
                return Err(Error::LimitExceeded {
                    kind: LimitKind::DnsCandidates,
                    limit,
                });
            }
            normalized.push(address);
        }
    }
    if normalized.is_empty() {
        return Err(Error::Dns(DnsFailure::NoRecords));
    }
    Ok(normalized)
}

trait HostResolver: fmt::Debug + Send + Sync {
    fn resolve(
        &self,
        domain: &str,
        timeout: Duration,
        max_candidates: usize,
        cancellation: &CancellationToken,
        deadline: Option<Instant>,
    ) -> Result<Vec<IpAddr>>;
}

struct SystemResolver {
    sender: Option<SyncSender<ResolveCommand>>,
    worker: Option<thread::JoinHandle<()>>,
}

struct ResolveCommand {
    name: Name,
    started: Instant,
    timeout: Duration,
    max_candidates: usize,
    cancellation: CancellationToken,
    deadline: Option<Instant>,
    response: SyncSender<Result<Vec<IpAddr>>>,
}

impl SystemResolver {
    fn new(config: &GeneralWebConfig) -> Result<Self> {
        let (sender, receiver) = sync_channel(DNS_WORK_QUEUE);
        let (startup_sender, startup_receiver) = sync_channel(1);
        let dns_timeout = config.dns_timeout;
        let worker = thread::Builder::new()
            .name("wild-buzzard-dns".to_owned())
            .spawn(move || resolver_worker(dns_timeout, &startup_sender, &receiver))
            .map_err(|_| Error::Dns(DnsFailure::Runtime))?;
        match startup_receiver.recv() {
            Ok(Ok(())) => Ok(Self {
                sender: Some(sender),
                worker: Some(worker),
            }),
            Ok(Err(error)) => {
                drop(sender);
                let _ = worker.join();
                Err(error)
            }
            Err(_) => {
                drop(sender);
                let _ = worker.join();
                Err(Error::Dns(DnsFailure::Runtime))
            }
        }
    }

    fn sender(&self) -> Result<&SyncSender<ResolveCommand>> {
        self.sender
            .as_ref()
            .ok_or(Error::Dns(DnsFailure::RuntimePoisoned))
    }
}

impl Drop for SystemResolver {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl fmt::Debug for SystemResolver {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SystemResolver { .. }")
    }
}

impl HostResolver for SystemResolver {
    fn resolve(
        &self,
        domain: &str,
        timeout: Duration,
        max_candidates: usize,
        cancellation: &CancellationToken,
        deadline: Option<Instant>,
    ) -> Result<Vec<IpAddr>> {
        let started = Instant::now();
        let name = Name::from_ascii(domain).map_err(|_| Error::Dns(DnsFailure::InvalidName))?;
        let (response, result) = sync_channel(1);
        let mut command = ResolveCommand {
            name,
            started,
            timeout,
            max_candidates,
            cancellation: cancellation.clone(),
            deadline,
            response,
        };

        loop {
            check_control(cancellation, deadline, Operation::ResolveDns)?;
            let wait = next_wait(started, timeout, deadline, Operation::ResolveDns)?;
            match self.sender()?.try_send(command) {
                Ok(()) => break,
                Err(TrySendError::Full(returned)) => {
                    command = returned;
                    thread::sleep(wait);
                }
                Err(TrySendError::Disconnected(_)) => {
                    return Err(Error::Dns(DnsFailure::RuntimePoisoned));
                }
            }
        }

        loop {
            check_control(cancellation, deadline, Operation::ResolveDns)?;
            let wait = next_wait(started, timeout, deadline, Operation::ResolveDns)?;
            match result.recv_timeout(wait) {
                Ok(result) => return result,
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(Error::Dns(DnsFailure::RuntimePoisoned));
                }
            }
        }
    }
}

fn resolver_worker(
    dns_timeout: Duration,
    startup: &SyncSender<Result<()>>,
    commands: &Receiver<ResolveCommand>,
) {
    let initialized = std::panic::catch_unwind(|| build_resolver_runtime(dns_timeout));
    let (runtime, resolver) = match initialized {
        Ok(Ok(initialized)) => initialized,
        Ok(Err(error)) => {
            let _ = startup.send(Err(error));
            return;
        }
        Err(_) => {
            let _ = startup.send(Err(Error::Dns(DnsFailure::Runtime)));
            return;
        }
    };
    if startup.send(Ok(())).is_err() {
        return;
    }

    while let Ok(command) = commands.recv() {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            resolve_on_worker(&runtime, &resolver, &command)
        }));
        let panicked = outcome.is_err();
        let result = outcome.unwrap_or(Err(Error::Dns(DnsFailure::Runtime)));
        let _ = command.response.send(result);
        if panicked {
            return;
        }
    }
}

fn build_resolver_runtime(
    dns_timeout: Duration,
) -> Result<(tokio::runtime::Runtime, TokioResolver)> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .map_err(|_| Error::Dns(DnsFailure::Runtime))?;
    let resolver = {
        let _entered = runtime.enter();
        let mut builder =
            TokioResolver::builder_tokio().map_err(|_| Error::Dns(DnsFailure::Configuration))?;
        let options = builder.options_mut();
        options.timeout = dns_timeout;
        options.attempts = DNS_ATTEMPTS;
        options.ip_strategy = LookupIpStrategy::Ipv6AndIpv4;
        options.cache_size = DNS_CACHE_ENTRIES;
        options.max_active_requests = DNS_MAX_ACTIVE_REQUESTS;
        options.num_concurrent_reqs = 2;
        options.preserve_intermediates = false;
        options.try_tcp_on_error = true;
        builder
            .build()
            .map_err(|_| Error::Dns(DnsFailure::Configuration))?
    };
    Ok((runtime, resolver))
}

fn resolve_on_worker(
    runtime: &tokio::runtime::Runtime,
    resolver: &TokioResolver,
    command: &ResolveCommand,
) -> Result<Vec<IpAddr>> {
    let lookup = resolver.lookup_ip(command.name.clone());
    tokio::pin!(lookup);
    loop {
        check_control(
            &command.cancellation,
            command.deadline,
            Operation::ResolveDns,
        )?;
        let wait = next_wait(
            command.started,
            command.timeout,
            command.deadline,
            Operation::ResolveDns,
        )?;
        match runtime.block_on(async { tokio::time::timeout(wait, lookup.as_mut()).await }) {
            Ok(Ok(addresses)) => {
                return collect_bounded_unique(addresses.iter(), command.max_candidates);
            }
            Ok(Err(error)) if error.is_no_records_found() || error.is_nx_domain() => {
                return Err(Error::Dns(DnsFailure::NoRecords));
            }
            Ok(Err(_)) => return Err(Error::Dns(DnsFailure::Lookup)),
            Err(_) => {}
        }
    }
}

fn build_tls_config(trust_store: TrustStore) -> Result<TlsClientConfig> {
    if trust_store.roots.is_empty() {
        return Err(Error::TrustStore(TrustStoreFailure::Empty));
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = TlsClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
        .map_err(|_| Error::Tls(TlsFailure::Configuration))?
        .with_root_certificates(trust_store.roots)
        .with_no_client_auth();
    config.alpn_protocols = vec![HTTP_11_ALPN.to_vec()];
    config.check_selected_alpn = true;
    config.enable_sni = true;
    config.enable_early_data = false;
    Ok(config)
}

fn establish_tls(
    mut socket: TcpStream,
    host: &WebHost,
    config: Arc<TlsClientConfig>,
    policy: &GeneralWebConfig,
    cancellation: &CancellationToken,
    deadline: Option<Instant>,
) -> Result<(TlsWireStream, ConnectionSecurity)> {
    let server_name = match host {
        WebHost::Domain(domain) => ServerName::try_from(domain.clone())
            .map_err(|_| Error::Tls(TlsFailure::InvalidServerName))?,
        WebHost::Ip(address) => ServerName::from(*address).to_owned(),
    };
    let mut connection = ClientConnection::new(config, server_name)
        .map_err(|error| Error::Tls(classify_rustls_error(&error)))?;
    connection.set_buffer_limit(Some(TLS_BUFFER_BYTES));
    let started = Instant::now();
    let mut transferred = 0_usize;

    while connection.is_handshaking() || connection.wants_write() {
        check_control(cancellation, deadline, Operation::TlsHandshake)?;
        flush_tls_handshake(
            &mut connection,
            &mut socket,
            &mut transferred,
            policy,
            cancellation,
            deadline,
            started,
        )?;
        if !connection.is_handshaking() {
            break;
        }
        read_tls_handshake(
            &mut connection,
            &mut socket,
            &mut transferred,
            policy,
            deadline,
            started,
        )?;
    }

    let version = match connection.protocol_version() {
        Some(ProtocolVersion::TLSv1_2) => TlsVersion::Tls12,
        Some(ProtocolVersion::TLSv1_3) => TlsVersion::Tls13,
        _ => return Err(Error::Tls(TlsFailure::UnsupportedVersion)),
    };
    let alpn = match connection.alpn_protocol() {
        Some(HTTP_11_ALPN) => AlpnOutcome::Http11,
        None => AlpnOutcome::NotNegotiated,
        Some(_) => return Err(Error::Tls(TlsFailure::UnsupportedApplicationProtocol)),
    };
    Ok((
        TlsWireStream(StreamOwned::new(connection, socket)),
        ConnectionSecurity::Tls { version, alpn },
    ))
}

#[allow(clippy::too_many_arguments)]
fn flush_tls_handshake(
    connection: &mut ClientConnection,
    socket: &mut TcpStream,
    transferred: &mut usize,
    policy: &GeneralWebConfig,
    cancellation: &CancellationToken,
    deadline: Option<Instant>,
    started: Instant,
) -> Result<()> {
    while connection.wants_write() {
        check_control(cancellation, deadline, Operation::TlsHandshake)?;
        let wait = next_wait(
            started,
            policy.tls_handshake_timeout,
            deadline,
            Operation::TlsHandshake,
        )?;
        socket
            .set_write_timeout(Some(wait))
            .map_err(|error| Error::io(Operation::TlsHandshake, &error))?;
        let mut bounded = HandshakeIo::new(socket, transferred, policy.max_tls_handshake_bytes);
        match connection.write_tls(&mut bounded) {
            Ok(0) => {
                return Err(Error::Io {
                    operation: Operation::TlsHandshake,
                    kind: io::ErrorKind::WriteZero,
                });
            }
            Ok(_) => {}
            Err(error) if is_handshake_limit_error(&error) => {
                return Err(handshake_limit_error(policy.max_tls_handshake_bytes));
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::WouldBlock
                ) => {}
            Err(error) => return Err(Error::io(Operation::TlsHandshake, &error)),
        }
    }
    Ok(())
}

fn read_tls_handshake(
    connection: &mut ClientConnection,
    socket: &mut TcpStream,
    transferred: &mut usize,
    policy: &GeneralWebConfig,
    deadline: Option<Instant>,
    started: Instant,
) -> Result<()> {
    let wait = next_wait(
        started,
        policy.tls_handshake_timeout,
        deadline,
        Operation::TlsHandshake,
    )?;
    socket
        .set_read_timeout(Some(wait))
        .map_err(|error| Error::io(Operation::TlsHandshake, &error))?;
    let mut bounded = HandshakeIo::new(socket, transferred, policy.max_tls_handshake_bytes);
    match connection.read_tls(&mut bounded) {
        Ok(0) => Err(Error::Io {
            operation: Operation::TlsHandshake,
            kind: io::ErrorKind::UnexpectedEof,
        }),
        Ok(_) => connection
            .process_new_packets()
            .map(|_| ())
            .map_err(|error| Error::Tls(classify_rustls_error(&error))),
        Err(error) if is_handshake_limit_error(&error) => {
            Err(handshake_limit_error(policy.max_tls_handshake_bytes))
        }
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::Interrupted | io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
            ) =>
        {
            Ok(())
        }
        Err(error) => Err(Error::io(Operation::TlsHandshake, &error)),
    }
}

const fn handshake_limit_error(limit: usize) -> Error {
    Error::LimitExceeded {
        kind: LimitKind::TlsHandshakeBytes,
        limit,
    }
}

#[derive(Debug)]
struct HandshakeByteLimit;

impl fmt::Display for HandshakeByteLimit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("TLS handshake byte limit reached")
    }
}

impl std::error::Error for HandshakeByteLimit {}

struct HandshakeIo<'socket> {
    socket: &'socket mut TcpStream,
    transferred: &'socket mut usize,
    limit: usize,
}

impl<'socket> HandshakeIo<'socket> {
    fn new(socket: &'socket mut TcpStream, transferred: &'socket mut usize, limit: usize) -> Self {
        Self {
            socket,
            transferred,
            limit,
        }
    }

    fn remaining(&self) -> io::Result<usize> {
        self.limit
            .checked_sub(*self.transferred)
            .filter(|remaining| *remaining > 0)
            .ok_or_else(handshake_limit_io_error)
    }
}

impl Read for HandshakeIo<'_> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let permitted = output.len().min(self.remaining()?);
        let count = self.socket.read(&mut output[..permitted])?;
        *self.transferred = self
            .transferred
            .checked_add(count)
            .ok_or_else(handshake_limit_io_error)?;
        Ok(count)
    }
}

impl Write for HandshakeIo<'_> {
    fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        let permitted = input.len().min(self.remaining()?);
        let count = self.socket.write(&input[..permitted])?;
        *self.transferred = self
            .transferred
            .checked_add(count)
            .ok_or_else(handshake_limit_io_error)?;
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.socket.flush()
    }
}

fn handshake_limit_io_error() -> io::Error {
    io::Error::other(HandshakeByteLimit)
}

fn is_handshake_limit_error(error: &io::Error) -> bool {
    match error.get_ref() {
        Some(source) => source.is::<HandshakeByteLimit>(),
        None => false,
    }
}

#[derive(Debug)]
struct TlsWireStream(StreamOwned<ClientConnection, TcpStream>);

impl Read for TlsWireStream {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        self.0.read(output)
    }
}

impl Write for TlsWireStream {
    fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        self.0.write(input)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl WireStream for TlsWireStream {
    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.0.sock.set_read_timeout(timeout)
    }

    fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.0.sock.set_write_timeout(timeout)
    }
}

#[derive(Debug)]
enum TransportStream {
    Cleartext(TcpStream),
    Tls(Box<TlsWireStream>),
}

impl Read for TransportStream {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Cleartext(stream) => stream.read(output),
            Self::Tls(stream) => stream.read(output),
        }
    }
}

impl Write for TransportStream {
    fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        match self {
            Self::Cleartext(stream) => stream.write(input),
            Self::Tls(stream) => stream.write(input),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Cleartext(stream) => stream.flush(),
            Self::Tls(stream) => stream.flush(),
        }
    }
}

impl WireStream for TransportStream {
    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        match self {
            Self::Cleartext(stream) => stream.set_read_timeout(timeout),
            Self::Tls(stream) => stream.set_read_timeout(timeout),
        }
    }

    fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        match self {
            Self::Cleartext(stream) => stream.set_write_timeout(timeout),
            Self::Tls(stream) => stream.set_write_timeout(timeout),
        }
    }
}

#[cfg(test)]
mod explicit_config_tests {
    use super::*;

    fn http_policy() -> ClientConfig {
        ClientConfig::try_new_explicit_v1(
            64 * 1024,
            256,
            8 * 1024 * 1024,
            64 * 1024,
            0,
            0,
            8 * 1024,
            8,
            Duration::from_secs(2),
            Duration::from_secs(5),
            Duration::from_secs(5),
        )
        .expect("bounded HTTP policy")
    }

    #[derive(Clone, Copy)]
    struct Inputs {
        dns_timeout: Duration,
        tls_handshake_timeout: Duration,
        max_dns_candidates: usize,
        max_connection_attempts: usize,
        max_tls_handshake_bytes: usize,
    }

    impl Inputs {
        const fn at_v1_maximum() -> Self {
            Self {
                dns_timeout: DEFAULT_DNS_TIMEOUT,
                tls_handshake_timeout: DEFAULT_TLS_HANDSHAKE_TIMEOUT,
                max_dns_candidates: DEFAULT_MAX_DNS_CANDIDATES,
                max_connection_attempts: DEFAULT_MAX_CONNECTION_ATTEMPTS,
                max_tls_handshake_bytes: DEFAULT_MAX_TLS_HANDSHAKE_BYTES,
            }
        }

        fn build(self) -> Option<GeneralWebConfig> {
            GeneralWebConfig::try_new_explicit_v1(
                http_policy(),
                self.dns_timeout,
                self.tls_handshake_timeout,
                self.max_dns_candidates,
                self.max_connection_attempts,
                self.max_tls_handshake_bytes,
            )
        }
    }

    #[test]
    fn explicit_v1_enumerates_fields_and_rejects_every_out_of_policy_value() {
        let input = Inputs::at_v1_maximum();
        let config = input.build().expect("exact v1 maxima are valid");
        assert_eq!(config.dns_timeout(), input.dns_timeout);
        assert_eq!(config.tls_handshake_timeout(), input.tls_handshake_timeout);
        assert_eq!(config.max_dns_candidates(), input.max_dns_candidates);
        assert_eq!(
            config.max_connection_attempts(),
            input.max_connection_attempts
        );
        assert_eq!(
            config.max_tls_handshake_bytes(),
            input.max_tls_handshake_bytes
        );
        assert_eq!(config.http_config().max_request_header_count(), 0);
        assert_eq!(config.http_config().max_request_body_bytes(), 0);

        assert!(
            GeneralWebConfig::try_new_explicit_v1(
                ClientConfig::default().with_max_chunk_line_bytes(8 * 1024 + 1),
                input.dns_timeout,
                input.tls_handshake_timeout,
                input.max_dns_candidates,
                input.max_connection_attempts,
                input.max_tls_handshake_bytes,
            )
            .is_none()
        );

        let mut invalid = input;
        invalid.dns_timeout += Duration::from_nanos(1);
        assert!(invalid.build().is_none());
        invalid = input;
        invalid.tls_handshake_timeout += Duration::from_nanos(1);
        assert!(invalid.build().is_none());
        invalid = input;
        invalid.max_dns_candidates += 1;
        assert!(invalid.build().is_none());
        invalid = input;
        invalid.max_connection_attempts += 1;
        assert!(invalid.build().is_none());
        invalid = input;
        invalid.max_tls_handshake_bytes += 1;
        assert!(invalid.build().is_none());

        invalid = input;
        invalid.dns_timeout = Duration::ZERO;
        assert!(invalid.build().is_none());
        invalid = input;
        invalid.tls_handshake_timeout = Duration::ZERO;
        assert!(invalid.build().is_none());
    }
}

#[cfg(test)]
mod security_policy_tests {
    use super::*;
    use std::{
        net::{Ipv4Addr, Ipv6Addr, TcpListener},
        sync::atomic::{AtomicUsize, Ordering},
    };

    #[derive(Debug)]
    struct CountingResolver {
        calls: Arc<AtomicUsize>,
        addresses: Vec<IpAddr>,
    }

    impl HostResolver for CountingResolver {
        fn resolve(
            &self,
            _domain: &str,
            _timeout: Duration,
            _max_candidates: usize,
            cancellation: &CancellationToken,
            deadline: Option<Instant>,
        ) -> Result<Vec<IpAddr>> {
            check_control(cancellation, deadline, Operation::ResolveDns)?;
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.addresses.clone())
        }
    }

    fn client_with_counting_resolver(
        addresses: Vec<IpAddr>,
        calls: Arc<AtomicUsize>,
    ) -> GeneralWebClient {
        let http = ClientConfig::default().with_connect_timeout(Duration::from_millis(20));
        let config = GeneralWebConfig::default()
            .with_http_config(http)
            .with_max_connection_attempts(4);
        GeneralWebClient::with_resolver(
            config,
            TrustStore::bundled_web_pki(),
            Arc::new(CountingResolver { calls, addresses }),
        )
        .expect("construct policy-test client")
    }

    fn committed_access(
        parent: IpAddressSpace,
        permissions: LocalNetworkAccessPermissions,
    ) -> GeneralWebNetworkAccess {
        GeneralWebNetworkAccess::committed_unit_test(parent, permissions)
    }

    fn checked_get(url: &str, access: GeneralWebNetworkAccess) -> GeneralWebRequest {
        GeneralWebRequest::get_with_network_access(
            GeneralWebTarget::parse(url).expect("parse policy-test target"),
            RedirectPolicy::Manual,
            access,
        )
    }

    fn assert_listener_received_no_connection(listener: &TcpListener) {
        listener
            .set_nonblocking(true)
            .expect("make no-connect listener nonblocking");
        let mut accepted = 0_usize;
        loop {
            match listener.accept() {
                Ok((socket, _)) => {
                    drop(socket);
                    accepted = accepted.checked_add(1).expect("connection count overflow");
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("inspect no-connect listener: {error}"),
            }
        }
        assert_eq!(accepted, 0, "policy denial reached the listener");
    }

    fn bind_allowed_loopback() -> TcpListener {
        loop {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
                .expect("bind allowed loopback listener");
            if !is_restricted_web_port(
                listener
                    .local_addr()
                    .expect("read allowed listener address")
                    .port(),
            ) {
                return listener;
            }
        }
    }

    fn bind_restricted_loopback() -> TcpListener {
        for port in RESTRICTED_WEB_PORTS
            .iter()
            .copied()
            .filter(|port| *port > 1024)
        {
            if let Ok(listener) = TcpListener::bind((Ipv4Addr::LOCALHOST, port)) {
                return listener;
            }
        }
        panic!("no high restricted test port was available");
    }

    #[test]
    fn restricted_port_table_is_exact_and_adjacent_ports_remain_allowed() {
        let actual = (1..=u16::MAX)
            .filter(|port| is_restricted_web_port(*port))
            .collect::<Vec<_>>();
        assert_eq!(actual, RESTRICTED_WEB_PORTS);

        for port in [2, 6, 8, 10, 12, 14, 16, 18, 24, 26, 68, 70, 10079, 10081] {
            assert!(!is_restricted_web_port(port), "adjacent port {port}");
        }
    }

    #[test]
    fn restricted_ports_fail_before_dns_and_before_numeric_socket_creation() {
        let calls = Arc::new(AtomicUsize::new(0));
        let client = client_with_counting_resolver(
            vec![IpAddr::V4(Ipv4Addr::LOCALHOST)],
            Arc::clone(&calls),
        );
        let access =
            client.browser_navigation_network_access(LocalNetworkAccessPermissions::deny_all());
        let request = checked_get("http://restricted.example:10080/", access);
        assert_eq!(
            client.execute_checked(&request).unwrap_err(),
            GeneralWebExecutionError::Policy(GeneralWebPolicyError::RestrictedPort)
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        let listener = bind_restricted_loopback();
        let address = listener
            .local_addr()
            .expect("read restricted listener address");
        let access =
            client.browser_navigation_network_access(LocalNetworkAccessPermissions::deny_all());
        let request = checked_get(&format!("http://{address}/"), access);
        assert_eq!(
            client.execute_checked(&request).unwrap_err(),
            GeneralWebExecutionError::Policy(GeneralWebPolicyError::RestrictedPort)
        );
        assert_listener_received_no_connection(&listener);
    }

    #[test]
    fn address_space_classification_matches_the_pinned_esr_ranges() {
        let cases = [
            ("0.0.0.0", IpAddressSpace::Local),
            ("0.0.0.1", IpAddressSpace::Private),
            ("0.255.255.255", IpAddressSpace::Private),
            ("1.0.0.0", IpAddressSpace::Public),
            ("127.0.0.1", IpAddressSpace::Local),
            ("127.255.255.255", IpAddressSpace::Local),
            ("10.0.0.1", IpAddressSpace::Private),
            ("100.64.0.0", IpAddressSpace::Private),
            ("100.127.255.255", IpAddressSpace::Private),
            ("100.128.0.0", IpAddressSpace::Public),
            ("169.254.255.254", IpAddressSpace::Private),
            ("169.255.0.0", IpAddressSpace::Public),
            ("172.16.0.0", IpAddressSpace::Private),
            ("172.31.255.255", IpAddressSpace::Private),
            ("172.32.0.0", IpAddressSpace::Public),
            ("192.168.0.1", IpAddressSpace::Private),
            ("192.169.0.1", IpAddressSpace::Public),
            ("198.18.0.1", IpAddressSpace::Public),
            ("8.8.8.8", IpAddressSpace::Public),
            ("::", IpAddressSpace::Local),
            ("::1", IpAddressSpace::Local),
            ("fc00::", IpAddressSpace::Private),
            ("fdff:ffff::1", IpAddressSpace::Private),
            ("fe80::1", IpAddressSpace::Private),
            ("febf::1", IpAddressSpace::Private),
            ("fec0::1", IpAddressSpace::Public),
            ("2001:4860:4860::8888", IpAddressSpace::Public),
            ("::ffff:0.0.0.0", IpAddressSpace::Local),
            ("::ffff:0.0.0.1", IpAddressSpace::Private),
            ("::ffff:0.255.255.255", IpAddressSpace::Private),
            ("::ffff:1.0.0.0", IpAddressSpace::Public),
            ("::ffff:127.9.8.7", IpAddressSpace::Local),
            ("::ffff:10.0.0.1", IpAddressSpace::Private),
            ("::ffff:100.64.0.1", IpAddressSpace::Private),
            ("::ffff:169.254.1.1", IpAddressSpace::Private),
            ("::ffff:1.1.1.1", IpAddressSpace::Public),
        ];
        for (input, expected) in cases {
            let address = input.parse::<IpAddr>().expect("parse classification case");
            assert_eq!(classify_ip_address_space(address), expected, "{input}");
        }

        assert_eq!(
            classify_ip_address_space(IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0xffff, 0xc0a8, 1,))),
            IpAddressSpace::Private
        );
    }

    #[test]
    fn zero_net_requires_private_permission_before_native_or_mapped_connect() {
        let listener = bind_allowed_loopback();
        let port = listener
            .local_addr()
            .expect("read zero-net probe listener address")
            .port();
        let calls = Arc::new(AtomicUsize::new(0));
        let client = client_with_counting_resolver(Vec::new(), Arc::clone(&calls));
        let access = committed_access(
            IpAddressSpace::Public,
            LocalNetworkAccessPermissions::deny_all(),
        );

        for host in ["0.0.0.1", "[::ffff:0.0.0.1]"] {
            let request = checked_get(&format!("http://{host}:{port}/"), access.clone());
            assert!(matches!(
                client.execute_checked(&request),
                Err(GeneralWebExecutionError::Policy(
                    GeneralWebPolicyError::LocalNetworkAccessDenied {
                        parent: IpAddressSpace::Public,
                        target: IpAddressSpace::Private,
                        required: LocalNetworkTarget::Private,
                        permission: LocalNetworkPermission::Denied,
                    }
                ))
            ));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_listener_received_no_connection(&listener);

        let private_granted = LocalNetworkAccessPermissions::new(
            LocalNetworkPermission::Denied,
            LocalNetworkPermission::Granted,
        );
        assert_eq!(
            authorize_address_space_transition(
                IpAddressSpace::Public,
                classify_ip_address_space("0.0.0.1".parse().expect("parse zero-net grant case")),
                private_granted,
            ),
            Ok(())
        );
        assert_eq!(
            authorize_address_space_transition(
                IpAddressSpace::Public,
                classify_ip_address_space(
                    "::ffff:0.0.0.1"
                        .parse()
                        .expect("parse mapped zero-net grant case"),
                ),
                private_granted,
            ),
            Ok(())
        );
    }

    #[test]
    fn only_exact_grants_admit_more_private_transitions() {
        for permission in [
            LocalNetworkPermission::Unknown,
            LocalNetworkPermission::Pending,
            LocalNetworkPermission::Denied,
        ] {
            assert!(matches!(
                authorize_address_space_transition(
                    IpAddressSpace::Public,
                    IpAddressSpace::Local,
                    LocalNetworkAccessPermissions::new(
                        permission,
                        LocalNetworkPermission::Granted,
                    ),
                ),
                Err(GeneralWebPolicyError::LocalNetworkAccessDenied {
                    required: LocalNetworkTarget::Local,
                    permission: actual,
                    ..
                }) if actual == permission
            ));
            assert!(matches!(
                authorize_address_space_transition(
                    IpAddressSpace::Public,
                    IpAddressSpace::Private,
                    LocalNetworkAccessPermissions::new(
                        LocalNetworkPermission::Granted,
                        permission,
                    ),
                ),
                Err(GeneralWebPolicyError::LocalNetworkAccessDenied {
                    required: LocalNetworkTarget::Private,
                    permission: actual,
                    ..
                }) if actual == permission
            ));
        }

        let granted = LocalNetworkAccessPermissions::new(
            LocalNetworkPermission::Granted,
            LocalNetworkPermission::Granted,
        );
        assert_eq!(
            authorize_address_space_transition(
                IpAddressSpace::Public,
                IpAddressSpace::Local,
                granted,
            ),
            Ok(())
        );
        assert_eq!(
            authorize_address_space_transition(
                IpAddressSpace::Public,
                IpAddressSpace::Private,
                granted,
            ),
            Ok(())
        );
        assert_eq!(
            authorize_address_space_transition(
                IpAddressSpace::Private,
                IpAddressSpace::Local,
                granted,
            ),
            Ok(())
        );

        let unknown = LocalNetworkAccessPermissions::new(
            LocalNetworkPermission::Unknown,
            LocalNetworkPermission::Unknown,
        );
        for (parent, target) in [
            (IpAddressSpace::Local, IpAddressSpace::Local),
            (IpAddressSpace::Local, IpAddressSpace::Private),
            (IpAddressSpace::Local, IpAddressSpace::Public),
            (IpAddressSpace::Private, IpAddressSpace::Private),
            (IpAddressSpace::Private, IpAddressSpace::Public),
            (IpAddressSpace::Public, IpAddressSpace::Public),
        ] {
            assert_eq!(
                authorize_address_space_transition(parent, target, unknown),
                Ok(())
            );
        }
        assert_eq!(
            authorize_address_space_transition(
                IpAddressSpace::Unknown,
                IpAddressSpace::Public,
                granted,
            ),
            Err(GeneralWebPolicyError::InvalidInitiatorEvidence)
        );
    }

    #[test]
    fn denied_initial_and_manual_redirect_hops_create_no_local_connection() {
        let first = bind_allowed_loopback();
        let second = bind_allowed_loopback();
        let calls = Arc::new(AtomicUsize::new(0));
        let client = client_with_counting_resolver(Vec::new(), calls);
        let access = committed_access(
            IpAddressSpace::Public,
            LocalNetworkAccessPermissions::deny_all(),
        );

        for listener in [&first, &second] {
            let address = listener.local_addr().expect("read LNA listener address");
            let request = checked_get(&format!("http://{address}/"), access.clone());
            assert!(matches!(
                client.execute_checked(&request),
                Err(GeneralWebExecutionError::Policy(
                    GeneralWebPolicyError::LocalNetworkAccessDenied {
                        parent: IpAddressSpace::Public,
                        target: IpAddressSpace::Local,
                        required: LocalNetworkTarget::Local,
                        permission: LocalNetworkPermission::Denied,
                    }
                ))
            ));
            assert_listener_received_no_connection(listener);
        }
    }

    #[test]
    fn mixed_dns_candidates_cannot_reach_a_later_prohibited_listener() {
        let listener = bind_allowed_loopback();
        let address = listener
            .local_addr()
            .expect("read mixed-DNS listener address");
        let calls = Arc::new(AtomicUsize::new(0));
        let client = client_with_counting_resolver(
            vec![
                IpAddr::V6("2001:db8::1".parse().expect("parse public test IP")),
                IpAddr::V4(Ipv4Addr::LOCALHOST),
            ],
            Arc::clone(&calls),
        );
        let access = committed_access(
            IpAddressSpace::Public,
            LocalNetworkAccessPermissions::deny_all(),
        );
        let request = checked_get(&format!("http://mixed.example:{}/", address.port()), access);
        assert!(matches!(
            client.execute_checked(&request),
            Err(GeneralWebExecutionError::Policy(
                GeneralWebPolicyError::LocalNetworkAccessDenied {
                    target: IpAddressSpace::Local,
                    ..
                }
            ))
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_listener_received_no_connection(&listener);
    }

    #[test]
    fn granted_local_permission_connects_and_response_authenticates_space() {
        let listener = bind_allowed_loopback();
        let address = listener
            .local_addr()
            .expect("read granted listener address");
        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("accept granted connection");
            let mut buffer = [0_u8; 4096];
            let _ = socket.read(&mut buffer).expect("read granted request");
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                .expect("write granted response");
        });
        let calls = Arc::new(AtomicUsize::new(0));
        let client = client_with_counting_resolver(Vec::new(), calls);
        let access = committed_access(
            IpAddressSpace::Public,
            LocalNetworkAccessPermissions::new(
                LocalNetworkPermission::Granted,
                LocalNetworkPermission::Denied,
            ),
        );
        let request = checked_get(&format!("http://{address}/"), access);
        let response = client
            .execute_checked(&request)
            .expect("granted local transition");
        assert_eq!(response.address_space(), IpAddressSpace::Local);
        assert_eq!(
            response.read_body_to_end().expect("read granted body"),
            b"ok"
        );
        server.join().expect("join granted server");
    }

    #[test]
    fn browser_navigation_evidence_is_bound_to_one_client_capability() {
        let first_calls = Arc::new(AtomicUsize::new(0));
        let first =
            client_with_counting_resolver(vec![IpAddr::V4(Ipv4Addr::LOCALHOST)], first_calls);
        let second_calls = Arc::new(AtomicUsize::new(0));
        let second = client_with_counting_resolver(
            vec![IpAddr::V4(Ipv4Addr::LOCALHOST)],
            Arc::clone(&second_calls),
        );
        let access =
            first.browser_navigation_network_access(LocalNetworkAccessPermissions::deny_all());
        let request = checked_get("http://bound.example:8080/", access);
        assert_eq!(
            second.execute_checked(&request).unwrap_err(),
            GeneralWebExecutionError::Policy(GeneralWebPolicyError::InvalidInitiatorEvidence)
        );
        assert_eq!(second_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn committed_response_authority_is_exact_and_only_its_client_family_can_redeem_it() {
        let listener = bind_allowed_loopback();
        let address = listener
            .local_addr()
            .expect("read authority listener address");
        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("accept authority connection");
            let mut buffer = [0_u8; 4096];
            let _ = socket.read(&mut buffer).expect("read authority request");
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                .expect("write authority response");
        });

        let first_calls = Arc::new(AtomicUsize::new(0));
        let first = client_with_counting_resolver(Vec::new(), first_calls);
        let original_target =
            GeneralWebTarget::parse(&format!("http://{address}/exact")).expect("parse exact URL");
        let request = checked_get(
            original_target.url().as_str(),
            first.browser_navigation_network_access(LocalNetworkAccessPermissions::deny_all()),
        );
        let response = first
            .execute_checked(&request)
            .expect("fetch authority response");
        let authority = response.response_authority().clone();
        assert!(authority.matches_target(&original_target));
        assert!(
            !authority.matches_target(
                &GeneralWebTarget::parse(&format!("http://{address}/other"))
                    .expect("parse distinct URL")
            )
        );
        assert_eq!(authority.security(), ConnectionSecurity::Cleartext);
        assert_eq!(authority.address_space(), IpAddressSpace::Local);
        let authority_debug = format!("{authority:?}");
        assert!(!authority_debug.contains("exact"));
        assert!(!authority_debug.contains(&address.port().to_string()));
        assert_eq!(
            response.read_body_to_end().expect("read authority body"),
            b"ok"
        );
        server.join().expect("join authority server");

        let second_calls = Arc::new(AtomicUsize::new(0));
        let second = client_with_counting_resolver(
            vec![IpAddr::V4(Ipv4Addr::LOCALHOST)],
            Arc::clone(&second_calls),
        );
        assert!(matches!(
            second.network_access_for_committed_response(
                &authority,
                LocalNetworkAccessPermissions::deny_all(),
            ),
            Err(GeneralWebPolicyError::InvalidInitiatorEvidence)
        ));
        assert!(matches!(
            second.delegate_for_response(&authority, GeneralWebConfig::default()),
            Err(GeneralWebPolicyError::InvalidInitiatorEvidence)
        ));
        assert_eq!(second_calls.load(Ordering::SeqCst), 0);

        let delegated = first
            .delegate_for_response(&authority, GeneralWebConfig::default())
            .expect("delegate exact client identity");
        let access = delegated
            .network_access_for_committed_response(
                &authority,
                LocalNetworkAccessPermissions::deny_all(),
            )
            .expect("redeem authority through identity-preserving child");
        assert!(matches!(
            access.initiator,
            InitiatorAddressSpaceEvidence::CommittedDocument(_)
        ));
    }
}

#[cfg(test)]
mod tests;
