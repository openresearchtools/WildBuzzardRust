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
    headers: Headers,
    body: Vec<u8>,
    redirect_policy: RedirectPolicy,
    cancellation: CancellationToken,
    deadline: Option<Instant>,
}

impl GeneralWebRequest {
    /// Creates a request with an explicit redirect policy.
    #[must_use]
    pub fn new(method: Method, target: GeneralWebTarget, redirect_policy: RedirectPolicy) -> Self {
        Self {
            method,
            target,
            headers: Headers::new(),
            body: Vec::new(),
            redirect_policy,
            cancellation: CancellationSource::new().token(),
            deadline: None,
        }
    }

    /// Creates a bodyless `GET` request with an explicit redirect policy.
    #[must_use]
    pub fn get(target: GeneralWebTarget, redirect_policy: RedirectPolicy) -> Self {
        Self::new(Method::get(), target, redirect_policy)
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
    security: ConnectionSecurity,
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
    pub const fn security(&self) -> ConnectionSecurity {
        self.security
    }

    /// Splits the HTTP response from its connection-security metadata.
    #[must_use]
    pub fn into_parts(self) -> (Response, ConnectionSecurity) {
        (self.response, self.security)
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
            }),
        })
    }

    /// Returns this client's immutable transport policy.
    #[must_use]
    pub fn config(&self) -> &GeneralWebConfig {
        &self.inner.config
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
        let prepared = prepare_request(request, self.config().http_config())?;
        let addresses = self.resolve_addresses(request)?;
        let (stream, security) = self.connect_transport(request, &addresses)?;
        let response = execute_prepared(request, self.config().http_config(), &prepared, stream)?;
        Ok(GeneralWebResponse { response, security })
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
    ) -> Result<(TransportStream, ConnectionSecurity)> {
        let attempt_limit = self.config().max_connection_attempts;
        if attempt_limit == 0 {
            return Err(Error::LimitExceeded {
                kind: LimitKind::ConnectionAttempts,
                limit: attempt_limit,
            });
        }

        let mut attempted = 0_usize;
        let mut last_io_kind = None;
        let mut last_tls_error = None;
        for address in addresses.iter().copied().take(attempt_limit) {
            attempted += 1;
            let socket = match connect_general_interruptible(
                address,
                self.config().http.connect_timeout(),
                request.cancellation(),
                request.deadline(),
            ) {
                Ok(socket) => socket,
                Err(Error::Cancelled) => return Err(Error::Cancelled),
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
                Err(error) => return Err(error),
            };

            if request.target().origin().scheme() == WebScheme::Http {
                return Ok((
                    TransportStream::Cleartext(socket),
                    ConnectionSecurity::Cleartext,
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
                    return Ok((TransportStream::Tls(Box::new(stream)), security));
                }
                Err(Error::Cancelled) => return Err(Error::Cancelled),
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
                Err(error) => return Err(error),
            }
        }

        if addresses.len() > attempt_limit {
            return Err(Error::LimitExceeded {
                kind: LimitKind::ConnectionAttempts,
                limit: attempt_limit,
            });
        }
        if let Some(error) = last_tls_error {
            return Err(error);
        }
        Err(Error::ConnectAttemptsExhausted {
            attempted,
            last_kind: last_io_kind,
        })
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
mod tests;
