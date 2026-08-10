// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::{
    io::{self, Read, Write},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose, date_time_ymd,
};
use rustls::{
    ServerConfig, ServerConnection, StreamOwned,
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
};

use super::*;
use crate::CertificateFailure;

const OK_RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";
const TEST_IO_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug)]
struct StaticResolver {
    addresses: Vec<IpAddr>,
}

impl StaticResolver {
    fn new(addresses: impl Into<Vec<IpAddr>>) -> Self {
        Self {
            addresses: addresses.into(),
        }
    }
}

impl HostResolver for StaticResolver {
    fn resolve(
        &self,
        _domain: &str,
        _timeout: Duration,
        _max_candidates: usize,
        cancellation: &CancellationToken,
        deadline: Option<Instant>,
    ) -> Result<Vec<IpAddr>> {
        check_control(cancellation, deadline, Operation::ResolveDns)?;
        Ok(self.addresses.clone())
    }
}

#[derive(Debug)]
struct BlockingResolver;

impl HostResolver for BlockingResolver {
    fn resolve(
        &self,
        _domain: &str,
        timeout: Duration,
        _max_candidates: usize,
        cancellation: &CancellationToken,
        deadline: Option<Instant>,
    ) -> Result<Vec<IpAddr>> {
        let started = Instant::now();
        loop {
            check_control(cancellation, deadline, Operation::ResolveDns)?;
            let wait = next_wait(started, timeout, deadline, Operation::ResolveDns)?;
            thread::sleep(wait);
        }
    }
}

#[derive(Debug)]
struct NoRecordsResolver;

impl HostResolver for NoRecordsResolver {
    fn resolve(
        &self,
        _domain: &str,
        _timeout: Duration,
        _max_candidates: usize,
        _cancellation: &CancellationToken,
        _deadline: Option<Instant>,
    ) -> Result<Vec<IpAddr>> {
        Err(Error::Dns(DnsFailure::NoRecords))
    }
}

#[derive(Debug)]
struct ScriptedConnectAttempt {
    polls: Arc<AtomicUsize>,
    ready_after: Option<usize>,
    cancel_on_poll: Option<(usize, CancellationSource)>,
    sleep_for_poll: bool,
}

impl PendingConnection for ScriptedConnectAttempt {
    fn poll_connected(&mut self, timeout: Duration) -> io::Result<bool> {
        let poll = self.polls.fetch_add(1, Ordering::SeqCst) + 1;
        if let Some((cancel_on_poll, source)) = &self.cancel_on_poll
            && poll == *cancel_on_poll
        {
            source.cancel();
        }
        if self.sleep_for_poll {
            thread::sleep(timeout);
        }
        Ok(self
            .ready_after
            .is_some_and(|ready_after| poll >= ready_after))
    }
}

#[derive(Debug)]
struct PlainObservation {
    request: Vec<u8>,
}

fn spawn_plain_server(
    host: IpAddr,
    response: &'static [u8],
) -> (SocketAddr, JoinHandle<PlainObservation>) {
    let listener = TcpListener::bind(SocketAddr::new(host, 0)).expect("bind local HTTP server");
    let address = listener.local_addr().expect("read local HTTP address");
    let handle = thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept local HTTP client");
        socket
            .set_read_timeout(Some(TEST_IO_TIMEOUT))
            .expect("set server read timeout");
        socket
            .set_write_timeout(Some(TEST_IO_TIMEOUT))
            .expect("set server write timeout");
        let request = read_request_head(&mut socket).expect("read HTTP request head");
        socket.write_all(response).expect("write HTTP response");
        PlainObservation { request }
    });
    (address, handle)
}

fn read_request_head(stream: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut request = Vec::new();
    let mut byte = [0_u8; 1];
    while !request.ends_with(b"\r\n\r\n") {
        if request.len() == 64 * 1024 {
            return Err(io::Error::other("test request head exceeded bound"));
        }
        stream.read_exact(&mut byte)?;
        request.push(byte[0]);
    }
    Ok(request)
}

fn http_url(host: &str, port: u16, suffix: &str) -> String {
    format!("http://{host}:{port}{suffix}")
}

fn https_url(host: &str, port: u16, suffix: &str) -> String {
    format!("https://{host}:{port}{suffix}")
}

fn request_for(url: &str) -> GeneralWebRequest {
    GeneralWebRequest::get(
        GeneralWebTarget::parse(url).expect("parse test URL"),
        RedirectPolicy::Manual,
    )
}

fn client_with_resolver(
    config: GeneralWebConfig,
    trust_store: TrustStore,
    resolver: impl HostResolver + 'static,
) -> GeneralWebClient {
    GeneralWebClient::with_resolver(config, trust_store, Arc::new(resolver))
        .expect("construct general-web client")
}

#[derive(Clone, Debug)]
struct TestCertificate {
    certificate: Vec<u8>,
    intermediates: Vec<Vec<u8>>,
    private_key: Vec<u8>,
}

fn test_certificate(names: &[&str]) -> TestCertificate {
    let signing_key = KeyPair::generate().expect("generate test signing key");
    let params = CertificateParams::new(names.iter().map(ToString::to_string).collect::<Vec<_>>())
        .expect("construct test certificate parameters");
    let certificate = params
        .self_signed(&signing_key)
        .expect("self-sign test certificate");
    TestCertificate {
        certificate: certificate.der().to_vec(),
        intermediates: Vec::new(),
        private_key: signing_key.serialize_der(),
    }
}

fn expired_test_certificate(name: &str) -> TestCertificate {
    let signing_key = KeyPair::generate().expect("generate expired test signing key");
    let mut params = CertificateParams::new(vec![name.to_owned()])
        .expect("construct expired certificate parameters");
    params.not_before = date_time_ymd(2000, 1, 1);
    params.not_after = date_time_ymd(2001, 1, 1);
    let certificate = params
        .self_signed(&signing_key)
        .expect("self-sign expired test certificate");
    TestCertificate {
        certificate: certificate.der().to_vec(),
        intermediates: Vec::new(),
        private_key: signing_key.serialize_der(),
    }
}

fn test_certificate_chain(name: &str) -> (Vec<u8>, TestCertificate) {
    let root_key = KeyPair::generate().expect("generate root key");
    let mut root_params = CertificateParams::new(Vec::new()).expect("construct root parameters");
    root_params
        .distinguished_name
        .push(DnType::CommonName, "Wild Buzzard test root");
    root_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    root_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let root = root_params
        .self_signed(&root_key)
        .expect("self-sign test root");
    let root_issuer = Issuer::new(root_params, root_key);

    let intermediate_key = KeyPair::generate().expect("generate intermediate key");
    let mut intermediate_params =
        CertificateParams::new(Vec::new()).expect("construct intermediate parameters");
    intermediate_params
        .distinguished_name
        .push(DnType::CommonName, "Wild Buzzard test intermediate");
    intermediate_params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    intermediate_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    intermediate_params.use_authority_key_identifier_extension = true;
    let intermediate = intermediate_params
        .signed_by(&intermediate_key, &root_issuer)
        .expect("sign test intermediate");
    let intermediate_issuer = Issuer::new(intermediate_params, intermediate_key);

    let leaf_key = KeyPair::generate().expect("generate leaf key");
    let mut leaf_params =
        CertificateParams::new(vec![name.to_owned()]).expect("construct leaf parameters");
    leaf_params
        .distinguished_name
        .push(DnType::CommonName, name);
    leaf_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    leaf_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    leaf_params.use_authority_key_identifier_extension = true;
    let leaf = leaf_params
        .signed_by(&leaf_key, &intermediate_issuer)
        .expect("sign test leaf");

    (
        root.der().to_vec(),
        TestCertificate {
            certificate: leaf.der().to_vec(),
            intermediates: vec![intermediate.der().to_vec()],
            private_key: leaf_key.serialize_der(),
        },
    )
}

#[derive(Clone, Copy, Debug)]
enum TestTlsVersions {
    Both,
    Tls12,
    Tls13,
}

#[derive(Debug)]
struct TlsObservation {
    request: Vec<u8>,
    server_name: Option<String>,
    alpn: Option<Vec<u8>>,
    transport_error: Option<io::ErrorKind>,
}

fn tls_server_config(
    certificate: &TestCertificate,
    versions: TestTlsVersions,
    alpn_protocols: &[&[u8]],
) -> ServerConfig {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let versions = match versions {
        TestTlsVersions::Both => vec![&rustls::version::TLS13, &rustls::version::TLS12],
        TestTlsVersions::Tls12 => vec![&rustls::version::TLS12],
        TestTlsVersions::Tls13 => vec![&rustls::version::TLS13],
    };
    let mut config = ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&versions)
        .expect("build test TLS versions")
        .with_no_client_auth()
        .with_single_cert(
            std::iter::once(certificate.certificate.clone())
                .chain(certificate.intermediates.iter().cloned())
                .map(CertificateDer::from)
                .collect(),
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(certificate.private_key.clone())),
        )
        .expect("build test TLS certificate");
    config.alpn_protocols = alpn_protocols.iter().map(|value| value.to_vec()).collect();
    config
}

fn spawn_tls_server(
    certificate: &TestCertificate,
    versions: TestTlsVersions,
    alpn_protocols: &[&[u8]],
    response: &'static [u8],
) -> (SocketAddr, JoinHandle<TlsObservation>) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind local TLS server");
    let address = listener.local_addr().expect("read local TLS address");
    let config = Arc::new(tls_server_config(certificate, versions, alpn_protocols));
    let handle = thread::spawn(move || {
        let (socket, _) = listener.accept().expect("accept local TLS client");
        socket
            .set_read_timeout(Some(TEST_IO_TIMEOUT))
            .expect("set TLS server read timeout");
        socket
            .set_write_timeout(Some(TEST_IO_TIMEOUT))
            .expect("set TLS server write timeout");
        let connection = ServerConnection::new(config).expect("create TLS server connection");
        let mut stream = StreamOwned::new(connection, socket);
        let request_result = read_request_head(&mut stream);
        let server_name = stream.conn.server_name().map(ToOwned::to_owned);
        let alpn = stream.conn.alpn_protocol().map(<[u8]>::to_vec);
        match request_result {
            Ok(request) => {
                let transport_error = stream.write_all(response).err().map(|error| error.kind());
                TlsObservation {
                    request,
                    server_name,
                    alpn,
                    transport_error,
                }
            }
            Err(error) => TlsObservation {
                request: Vec::new(),
                server_name,
                alpn,
                transport_error: Some(error.kind()),
            },
        }
    });
    (address, handle)
}

fn trusted_store(certificate: &TestCertificate) -> TrustStore {
    TrustStore::bundled_web_pki()
        .with_der_certificate(&certificate.certificate)
        .expect("add test trust anchor")
}

fn static_tls_client(certificate: &TestCertificate, config: GeneralWebConfig) -> GeneralWebClient {
    client_with_resolver(
        config,
        trusted_store(certificate),
        StaticResolver::new(vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]),
    )
}

fn spawn_stalling_server() -> (SocketAddr, JoinHandle<()>) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind stalling server");
    let address = listener.local_addr().expect("read stalling address");
    let handle = thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept stalling client");
        let mut byte = [0_u8; 1];
        let _ = socket.read(&mut byte);
        thread::sleep(Duration::from_millis(150));
    });
    (address, handle)
}

#[test]
fn connect_driver_polls_one_attempt_and_observes_timeout_and_cancellation() {
    let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 9);
    let token = CancellationSource::new().token();
    let polls = Arc::new(AtomicUsize::new(0));
    let mut creations = 0_usize;
    let attempt = drive_connect_attempt(address, Duration::from_secs(1), &token, None, |_| {
        creations += 1;
        Ok(ScriptedConnectAttempt {
            polls: Arc::clone(&polls),
            ready_after: Some(3),
            cancel_on_poll: None,
            sleep_for_poll: false,
        })
    })
    .expect("scripted connection becomes ready");
    assert_eq!(
        creations, 1,
        "one address attempt creates exactly one socket"
    );
    assert_eq!(attempt.polls.load(Ordering::SeqCst), 3);

    let timeout_polls = Arc::new(AtomicUsize::new(0));
    let timeout_token = CancellationSource::new().token();
    let timeout_result = drive_connect_attempt(
        address,
        Duration::from_millis(25),
        &timeout_token,
        None,
        |_| {
            Ok(ScriptedConnectAttempt {
                polls: Arc::clone(&timeout_polls),
                ready_after: None,
                cancel_on_poll: None,
                sleep_for_poll: true,
            })
        },
    );
    assert_eq!(
        timeout_result.unwrap_err(),
        Error::Timeout(Operation::Connect)
    );
    assert!(timeout_polls.load(Ordering::SeqCst) >= 2);

    let cancellation = CancellationSource::new();
    let cancellation_polls = Arc::new(AtomicUsize::new(0));
    let cancellation_result = drive_connect_attempt(
        address,
        Duration::from_secs(1),
        &cancellation.token(),
        None,
        |_| {
            Ok(ScriptedConnectAttempt {
                polls: Arc::clone(&cancellation_polls),
                ready_after: None,
                cancel_on_poll: Some((1, cancellation.clone())),
                sleep_for_poll: false,
            })
        },
    );
    assert_eq!(cancellation_result.unwrap_err(), Error::Cancelled);
    assert_eq!(cancellation_polls.load(Ordering::SeqCst), 1);
}

#[test]
fn general_target_uses_whatwg_normalization_and_rejects_sensitive_components() {
    let target = GeneralWebTarget::parse("https://BÜCHER.example:443/a b?value=✓")
        .expect("normalize general-web URL");
    assert_eq!(target.origin().scheme(), WebScheme::Https);
    assert_eq!(
        target.origin().host(),
        &WebHost::Domain("xn--bcher-kva.example".to_owned())
    );
    assert_eq!(target.origin().port(), 443);
    assert_eq!(target.origin().authority(), "xn--bcher-kva.example");
    assert_eq!(target.request_target().as_str(), "/a%20b?value=%E2%9C%93");

    assert!(matches!(
        GeneralWebTarget::parse("ftp://example.com/"),
        Err(Error::UnsupportedScheme(_))
    ));
    assert_eq!(
        GeneralWebTarget::parse("https://user:secret@example.com/").unwrap_err(),
        Error::CredentialsNotAllowed
    );
    assert_eq!(
        GeneralWebTarget::parse("https://example.com/#private").unwrap_err(),
        Error::FragmentNotAllowed
    );
    let (identity, transport) =
        GeneralWebTarget::parse_navigation("https://example.com/path?q=1#section")
            .expect("browser identity retains a fragment outside transport");
    assert_eq!(identity.as_str(), "https://example.com/path?q=1#section");
    assert_eq!(transport.url().as_str(), "https://example.com/path?q=1");
    assert_eq!(transport.request_target().as_str(), "/path?q=1");
    let oversized = format!(
        "http://example.test/{}",
        "a".repeat(crate::target::MAX_GENERAL_URL_BYTES)
    );
    assert_eq!(
        GeneralWebTarget::parse(&oversized).unwrap_err(),
        Error::LimitExceeded {
            kind: LimitKind::UrlBytes,
            limit: crate::target::MAX_GENERAL_URL_BYTES,
        }
    );
}

#[test]
fn cleartext_domain_request_is_bounded_and_serialized_without_credentials() {
    let (address, server) = spawn_plain_server(IpAddr::V4(Ipv4Addr::LOCALHOST), OK_RESPONSE);
    let client = client_with_resolver(
        GeneralWebConfig::default(),
        TrustStore::default(),
        StaticResolver::new(vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]),
    );
    let request = request_for(&http_url("example.test", address.port(), "/hello?x=1"));
    let response = client.execute(&request).expect("send cleartext request");
    assert_eq!(response.security(), ConnectionSecurity::Cleartext);
    assert_eq!(response.read_body_to_end().expect("read body"), b"ok");

    let observation = server.join().expect("join HTTP server");
    let wire = String::from_utf8(observation.request).expect("ASCII request");
    assert!(wire.starts_with("GET /hello?x=1 HTTP/1.1\r\n"));
    assert!(wire.contains(&format!("Host: example.test:{}\r\n", address.port())));
    assert!(!wire.contains("user"));
    assert!(!wire.contains("secret"));
}

#[test]
fn system_resolver_handles_localhost_without_public_network_access() {
    let (address, server) = spawn_plain_server(IpAddr::V4(Ipv4Addr::LOCALHOST), OK_RESPONSE);
    let client = GeneralWebClient::new(GeneralWebConfig::default(), TrustStore::default())
        .expect("construct system resolver client");
    let request = request_for(&http_url("localhost", address.port(), "/system-dns"));
    assert_eq!(
        client
            .execute(&request)
            .expect("resolve localhost")
            .read_body_to_end()
            .expect("read localhost body"),
        b"ok"
    );
    let observation = server.join().expect("join system DNS server");
    assert!(observation.request.starts_with(b"GET /system-dns "));
}

#[test]
fn system_resolver_runtime_is_owned_outside_callers_tokio_context() {
    let (address, server) = spawn_plain_server(IpAddr::V4(Ipv4Addr::LOCALHOST), OK_RESPONSE);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("construct caller Tokio runtime");
    let body = runtime.block_on(async {
        let client = GeneralWebClient::new(GeneralWebConfig::default(), TrustStore::default())
            .expect("construct client inside caller Tokio runtime");
        let request = request_for(&http_url("localhost", address.port(), "/nested-runtime"));
        let body = client
            .execute(&request)
            .expect("execute inside caller Tokio runtime")
            .read_body_to_end()
            .expect("read nested-runtime body");
        drop(client);
        body
    });
    assert_eq!(body, b"ok");
    server.join().expect("join nested-runtime server");
}

#[test]
fn numeric_ipv6_cleartext_target_connects_without_dns() {
    let (address, server) = spawn_plain_server(IpAddr::V6(Ipv6Addr::LOCALHOST), OK_RESPONSE);
    let client = client_with_resolver(
        GeneralWebConfig::default().with_max_dns_candidates(0),
        TrustStore::default(),
        NoRecordsResolver,
    );
    let request = request_for(&http_url("[::1]", address.port(), "/ipv6"));
    let response = client.execute(&request).expect("connect over IPv6");
    assert_eq!(response.read_body_to_end().expect("read IPv6 body"), b"ok");
    let observation = server.join().expect("join IPv6 server");
    assert!(observation.request.starts_with(b"GET /ipv6 "));
}

#[test]
fn proxy_credentials_are_reserved_before_any_origin_connection() {
    let mut request = request_for("https://example.test/");
    let error = request
        .append_header(
            HeaderName::new("Proxy-Authorization").expect("valid proxy field name"),
            HeaderValue::from_text("Basic secret").expect("valid proxy field value"),
        )
        .expect_err("reserve proxy credentials");
    assert_eq!(
        error,
        Error::ReservedRequestHeader("proxy-authorization".to_owned())
    );
}

#[test]
fn sequential_dual_stack_attempt_falls_back_from_ipv6_to_ipv4() {
    let (address, server) = spawn_plain_server(IpAddr::V4(Ipv4Addr::LOCALHOST), OK_RESPONSE);
    let client = client_with_resolver(
        GeneralWebConfig::default(),
        TrustStore::default(),
        StaticResolver::new(vec![
            IpAddr::V6(Ipv6Addr::LOCALHOST),
            IpAddr::V4(Ipv4Addr::LOCALHOST),
        ]),
    );
    let request = request_for(&http_url("dual-stack.test", address.port(), "/fallback"));
    assert_eq!(
        client
            .execute(&request)
            .expect("fall back to IPv4")
            .read_body_to_end()
            .expect("read fallback body"),
        b"ok"
    );
    server.join().expect("join fallback server");
}

#[test]
fn dns_candidates_are_deduplicated_and_bounded_before_connect() {
    let config = GeneralWebConfig::default().with_max_dns_candidates(1);
    let duplicate_client = client_with_resolver(
        config.clone(),
        TrustStore::default(),
        StaticResolver::new(vec![
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V4(Ipv4Addr::LOCALHOST),
        ]),
    );
    let closed = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("reserve closed port");
    let port = closed.local_addr().expect("read closed port").port();
    drop(closed);
    let request = request_for(&http_url("duplicate.test", port, "/"));
    assert!(matches!(
        duplicate_client.execute(&request),
        Err(Error::ConnectAttemptsExhausted { attempted: 1, .. })
    ));

    let bounded_client = client_with_resolver(
        config,
        TrustStore::default(),
        StaticResolver::new(vec![
            IpAddr::V6(Ipv6Addr::LOCALHOST),
            IpAddr::V4(Ipv4Addr::LOCALHOST),
        ]),
    );
    assert_eq!(
        bounded_client.execute(&request).unwrap_err(),
        Error::LimitExceeded {
            kind: LimitKind::DnsCandidates,
            limit: 1,
        }
    );
}

#[test]
fn connection_attempt_limit_stops_before_a_later_live_candidate() {
    let destination = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind live candidate");
    destination
        .set_nonblocking(true)
        .expect("make live candidate nonblocking");
    let address = destination
        .local_addr()
        .expect("read live candidate address");
    let client = client_with_resolver(
        GeneralWebConfig::default().with_max_connection_attempts(1),
        TrustStore::default(),
        StaticResolver::new(vec![
            IpAddr::V6(Ipv6Addr::LOCALHOST),
            IpAddr::V4(Ipv4Addr::LOCALHOST),
        ]),
    );
    let request = request_for(&http_url("attempt-limit.test", address.port(), "/"));
    assert_eq!(
        client.execute(&request).unwrap_err(),
        Error::LimitExceeded {
            kind: LimitKind::ConnectionAttempts,
            limit: 1,
        }
    );
    assert!(matches!(
        destination.accept(),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock
    ));
}

#[test]
fn dns_wait_observes_cancellation_deadline_and_timeout() {
    let source = CancellationSource::new();
    let cancelling_client = client_with_resolver(
        GeneralWebConfig::default().with_dns_timeout(Duration::from_secs(1)),
        TrustStore::default(),
        BlockingResolver,
    );
    let request = request_for("http://cancel-dns.test/").with_cancellation(source.token());
    let canceller = thread::spawn(move || {
        thread::sleep(Duration::from_millis(25));
        source.cancel();
    });
    assert_eq!(
        cancelling_client.execute(&request).unwrap_err(),
        Error::Cancelled
    );
    canceller.join().expect("join DNS canceller");

    let deadline_client = client_with_resolver(
        GeneralWebConfig::default().with_dns_timeout(Duration::from_secs(1)),
        TrustStore::default(),
        BlockingResolver,
    );
    let request = request_for("http://deadline-dns.test/")
        .with_deadline(Instant::now() + Duration::from_millis(25));
    assert_eq!(
        deadline_client.execute(&request).unwrap_err(),
        Error::Timeout(Operation::ResolveDns)
    );

    let timeout_client = client_with_resolver(
        GeneralWebConfig::default().with_dns_timeout(Duration::from_millis(25)),
        TrustStore::default(),
        BlockingResolver,
    );
    let request = request_for("http://timeout-dns.test/");
    assert_eq!(
        timeout_client.execute(&request).unwrap_err(),
        Error::Timeout(Operation::ResolveDns)
    );
}

#[test]
fn redirect_policy_returns_or_rejects_but_never_follows() {
    let destination = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind redirect sink");
    destination
        .set_nonblocking(true)
        .expect("make redirect sink nonblocking");
    let destination_address = destination
        .local_addr()
        .expect("read redirect sink address");
    let response = format!(
        "HTTP/1.1 302 Found\r\nLocation: http://{destination_address}/secret\r\nContent-Length: 0\r\n\r\n"
    );
    let response: &'static [u8] = Box::leak(response.into_bytes().into_boxed_slice());

    let (manual_address, manual_server) =
        spawn_plain_server(IpAddr::V4(Ipv4Addr::LOCALHOST), response);
    let client = client_with_resolver(
        GeneralWebConfig::default(),
        TrustStore::default(),
        StaticResolver::new(vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]),
    );
    let manual = request_for(&http_url("redirect.test", manual_address.port(), "/"));
    assert_eq!(
        client
            .execute(&manual)
            .expect("return manual redirect")
            .head()
            .status()
            .as_u16(),
        302
    );
    manual_server.join().expect("join manual redirect server");

    let (reject_address, reject_server) =
        spawn_plain_server(IpAddr::V4(Ipv4Addr::LOCALHOST), response);
    let reject = GeneralWebRequest::get(
        GeneralWebTarget::parse(&http_url("redirect.test", reject_address.port(), "/"))
            .expect("parse rejecting redirect URL"),
        RedirectPolicy::Reject,
    );
    assert_eq!(
        client.execute(&reject).unwrap_err(),
        Error::RedirectRejected(302)
    );
    reject_server.join().expect("join rejected redirect server");
    assert!(matches!(
        destination.accept(),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock
    ));
}

#[test]
fn trusted_tls_authenticates_name_sends_sni_and_negotiates_http11() {
    let certificate = test_certificate(&["localhost"]);
    let (address, server) = spawn_tls_server(
        &certificate,
        TestTlsVersions::Both,
        &[HTTP_11_ALPN],
        OK_RESPONSE,
    );
    let client = static_tls_client(&certificate, GeneralWebConfig::default());
    assert_eq!(client.inner.tls.alpn_protocols, vec![HTTP_11_ALPN.to_vec()]);
    assert!(client.inner.tls.check_selected_alpn);
    assert!(client.inner.tls.enable_sni);
    assert!(!client.inner.tls.enable_early_data);
    let request = request_for(&https_url("localhost", address.port(), "/secure"));
    let response = client.execute(&request).expect("send trusted TLS request");
    assert!(matches!(
        response.security(),
        ConnectionSecurity::Tls {
            version: TlsVersion::Tls13,
            alpn: AlpnOutcome::Http11,
        }
    ));
    assert_eq!(response.read_body_to_end().expect("read TLS body"), b"ok");

    let observation = server.join().expect("join TLS server");
    assert_eq!(observation.server_name.as_deref(), Some("localhost"));
    assert_eq!(observation.alpn.as_deref(), Some(HTTP_11_ALPN));
    assert!(observation.request.starts_with(b"GET /secure HTTP/1.1\r\n"));
    assert_eq!(observation.transport_error, None);
}

#[test]
fn tls_builds_a_leaf_intermediate_root_chain_and_requires_the_intermediate() {
    let (root, certificate) = test_certificate_chain("localhost");
    let (address, server) = spawn_tls_server(
        &certificate,
        TestTlsVersions::Both,
        &[HTTP_11_ALPN],
        OK_RESPONSE,
    );
    let trust = TrustStore::bundled_web_pki()
        .with_der_certificate(&root)
        .expect("trust only generated root");
    let client = client_with_resolver(
        GeneralWebConfig::default(),
        trust,
        StaticResolver::new(vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]),
    );
    let request = request_for(&https_url("localhost", address.port(), "/chain"));
    assert_eq!(
        client
            .execute(&request)
            .expect("build and authenticate certificate chain")
            .read_body_to_end()
            .expect("read chain response"),
        b"ok"
    );
    server.join().expect("join certificate-chain server");

    let mut missing_intermediate = certificate.clone();
    missing_intermediate.intermediates.clear();
    let (address, server) = spawn_tls_server(
        &missing_intermediate,
        TestTlsVersions::Both,
        &[HTTP_11_ALPN],
        OK_RESPONSE,
    );
    let trust = TrustStore::bundled_web_pki()
        .with_der_certificate(&root)
        .expect("trust generated root again");
    let client = client_with_resolver(
        GeneralWebConfig::default(),
        trust,
        StaticResolver::new(vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]),
    );
    let request = request_for(&https_url(
        "localhost",
        address.port(),
        "/missing-intermediate",
    ));
    assert_eq!(
        client.execute(&request).unwrap_err(),
        Error::Tls(TlsFailure::InvalidCertificate(
            CertificateFailure::UnknownIssuer
        ))
    );
    assert!(
        server
            .join()
            .expect("join missing-intermediate server")
            .transport_error
            .is_some()
    );
}

#[test]
fn numeric_tls_target_verifies_ip_san_and_sends_no_sni() {
    let certificate = test_certificate(&["127.0.0.1"]);
    let (address, server) = spawn_tls_server(
        &certificate,
        TestTlsVersions::Both,
        &[HTTP_11_ALPN],
        OK_RESPONSE,
    );
    let client = client_with_resolver(
        GeneralWebConfig::default(),
        trusted_store(&certificate),
        NoRecordsResolver,
    );
    let request = request_for(&https_url("127.0.0.1", address.port(), "/ip-san"));
    assert_eq!(
        client
            .execute(&request)
            .expect("authenticate IP subject alternative name")
            .read_body_to_end()
            .expect("read IP TLS body"),
        b"ok"
    );
    let observation = server.join().expect("join IP TLS server");
    assert_eq!(observation.server_name, None);
}

#[test]
fn tls_rejects_wrong_name_untrusted_and_expired_certificates() {
    let wrong_name = test_certificate(&["wrong-name.test"]);
    let (address, server) = spawn_tls_server(
        &wrong_name,
        TestTlsVersions::Both,
        &[HTTP_11_ALPN],
        OK_RESPONSE,
    );
    let client = static_tls_client(&wrong_name, GeneralWebConfig::default());
    let request = request_for(&https_url("localhost", address.port(), "/"));
    assert_eq!(
        client.execute(&request).unwrap_err(),
        Error::Tls(TlsFailure::InvalidCertificate(
            CertificateFailure::NotValidForName
        ))
    );
    assert!(
        server
            .join()
            .expect("join wrong-name server")
            .transport_error
            .is_some()
    );

    let untrusted = test_certificate(&["localhost"]);
    let (address, server) = spawn_tls_server(
        &untrusted,
        TestTlsVersions::Both,
        &[HTTP_11_ALPN],
        OK_RESPONSE,
    );
    let client = client_with_resolver(
        GeneralWebConfig::default(),
        TrustStore::default(),
        StaticResolver::new(vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]),
    );
    let request = request_for(&https_url("localhost", address.port(), "/"));
    assert_eq!(
        client.execute(&request).unwrap_err(),
        Error::Tls(TlsFailure::InvalidCertificate(
            CertificateFailure::UnknownIssuer
        ))
    );
    assert!(
        server
            .join()
            .expect("join untrusted server")
            .transport_error
            .is_some()
    );

    let expired = expired_test_certificate("localhost");
    let (address, server) = spawn_tls_server(
        &expired,
        TestTlsVersions::Both,
        &[HTTP_11_ALPN],
        OK_RESPONSE,
    );
    let client = static_tls_client(&expired, GeneralWebConfig::default());
    let request = request_for(&https_url("localhost", address.port(), "/"));
    assert_eq!(
        client.execute(&request).unwrap_err(),
        Error::Tls(TlsFailure::InvalidCertificate(CertificateFailure::Expired))
    );
    assert!(
        server
            .join()
            .expect("join expired server")
            .transport_error
            .is_some()
    );
}

#[test]
fn tls12_and_tls13_are_the_only_admitted_protocol_versions() {
    let certificate = test_certificate(&["localhost"]);
    for (versions, expected) in [
        (TestTlsVersions::Tls12, TlsVersion::Tls12),
        (TestTlsVersions::Tls13, TlsVersion::Tls13),
    ] {
        let (address, server) =
            spawn_tls_server(&certificate, versions, &[HTTP_11_ALPN], OK_RESPONSE);
        let client = static_tls_client(&certificate, GeneralWebConfig::default());
        let request = request_for(&https_url("localhost", address.port(), "/version"));
        let response = client.execute(&request).expect("negotiate TLS version");
        assert!(matches!(
            response.security(),
            ConnectionSecurity::Tls { version, .. } if version == expected
        ));
        assert_eq!(
            response.read_body_to_end().expect("read version body"),
            b"ok"
        );
        server.join().expect("join version server");
    }
}

#[test]
fn tls_without_alpn_uses_http11_but_never_offers_another_protocol() {
    let certificate = test_certificate(&["localhost"]);
    let (address, server) = spawn_tls_server(&certificate, TestTlsVersions::Both, &[], OK_RESPONSE);
    let client = static_tls_client(&certificate, GeneralWebConfig::default());
    let request = request_for(&https_url("localhost", address.port(), "/no-alpn"));
    let response = client.execute(&request).expect("use TLS without ALPN");
    assert!(matches!(
        response.security(),
        ConnectionSecurity::Tls {
            alpn: AlpnOutcome::NotNegotiated,
            ..
        }
    ));
    assert_eq!(
        response.read_body_to_end().expect("read no-ALPN body"),
        b"ok"
    );
    let observation = server.join().expect("join no-ALPN server");
    assert_eq!(observation.alpn, None);
}

#[test]
fn tls_rejects_a_server_requiring_a_non_http11_alpn() {
    let certificate = test_certificate(&["localhost"]);
    let (address, server) =
        spawn_tls_server(&certificate, TestTlsVersions::Both, &[b"h2"], OK_RESPONSE);
    let client = static_tls_client(&certificate, GeneralWebConfig::default());
    let request = request_for(&https_url("localhost", address.port(), "/h2-only"));
    assert_eq!(
        client.execute(&request).unwrap_err(),
        Error::Tls(TlsFailure::UnsupportedApplicationProtocol)
    );
    assert!(
        server
            .join()
            .expect("join h2-only server")
            .transport_error
            .is_some()
    );

    assert_eq!(
        classify_rustls_error(&rustls::Error::AlertReceived(
            rustls::AlertDescription::ProtocolVersion
        )),
        TlsFailure::UnsupportedVersion
    );
}

#[test]
fn tls_handshake_observes_cancellation_and_total_timeout() {
    let (address, server) = spawn_stalling_server();
    let source = CancellationSource::new();
    let client = client_with_resolver(
        GeneralWebConfig::default().with_tls_handshake_timeout(Duration::from_secs(1)),
        TrustStore::default(),
        StaticResolver::new(vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]),
    );
    let request =
        request_for(&https_url("localhost", address.port(), "/")).with_cancellation(source.token());
    let canceller = thread::spawn(move || {
        thread::sleep(Duration::from_millis(25));
        source.cancel();
    });
    assert_eq!(client.execute(&request).unwrap_err(), Error::Cancelled);
    canceller.join().expect("join TLS canceller");
    server.join().expect("join cancelled TLS server");

    let (address, server) = spawn_stalling_server();
    let client = client_with_resolver(
        GeneralWebConfig::default().with_tls_handshake_timeout(Duration::from_millis(25)),
        TrustStore::default(),
        StaticResolver::new(vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]),
    );
    let request = request_for(&https_url("localhost", address.port(), "/"));
    assert_eq!(
        client.execute(&request).unwrap_err(),
        Error::Timeout(Operation::TlsHandshake)
    );
    server.join().expect("join timed-out TLS server");
}

#[test]
fn tls_handshake_wire_bytes_are_bounded() {
    let certificate = test_certificate(&["localhost"]);
    let (address, server) = spawn_tls_server(
        &certificate,
        TestTlsVersions::Both,
        &[HTTP_11_ALPN],
        OK_RESPONSE,
    );
    let config = GeneralWebConfig::default().with_max_tls_handshake_bytes(1);
    let client = static_tls_client(&certificate, config);
    let request = request_for(&https_url("localhost", address.port(), "/"));
    assert_eq!(
        client.execute(&request).unwrap_err(),
        Error::LimitExceeded {
            kind: LimitKind::TlsHandshakeBytes,
            limit: 1,
        }
    );
    assert!(
        server
            .join()
            .expect("join limited TLS server")
            .transport_error
            .is_some()
    );
}

#[test]
fn strict_http_body_limit_remains_active_over_tls() {
    let certificate = test_certificate(&["localhost"]);
    let response = b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nabc";
    let (address, server) = spawn_tls_server(
        &certificate,
        TestTlsVersions::Both,
        &[HTTP_11_ALPN],
        response,
    );
    let config = GeneralWebConfig::default()
        .with_http_config(ClientConfig::default().with_max_body_bytes(2));
    let client = static_tls_client(&certificate, config);
    let request = request_for(&https_url("localhost", address.port(), "/limit"));
    assert_eq!(
        client.execute(&request).unwrap_err(),
        Error::LimitExceeded {
            kind: LimitKind::BodyBytes,
            limit: 2,
        }
    );
    server.join().expect("join TLS body-limit server");
}

#[test]
fn trust_store_extension_is_additive_and_rejects_invalid_der() {
    let bundled = TrustStore::bundled_web_pki();
    let initial_count = bundled.anchor_count();
    let certificate = test_certificate(&["localhost"]);
    let extended = bundled
        .with_der_certificate(&certificate.certificate)
        .expect("add valid certificate");
    assert_eq!(extended.anchor_count(), initial_count + 1);
    assert_eq!(
        extended
            .with_der_certificate(b"not a certificate")
            .unwrap_err(),
        Error::TrustStore(TrustStoreFailure::InvalidCertificate)
    );
}

#[test]
fn no_records_and_pre_cancelled_requests_fail_before_connection() {
    let client = client_with_resolver(
        GeneralWebConfig::default(),
        TrustStore::default(),
        NoRecordsResolver,
    );
    let request = request_for("http://missing.test/");
    assert_eq!(
        client.execute(&request).unwrap_err(),
        Error::Dns(DnsFailure::NoRecords)
    );

    let source = CancellationSource::new();
    source.cancel();
    let request = request_for("http://missing.test/").with_cancellation(source.token());
    assert_eq!(client.execute(&request).unwrap_err(), Error::Cancelled);
}

#[test]
#[ignore = "manual public-network smoke; set WILD_BUZZARD_PUBLIC_NETWORK=1"]
fn public_example_dot_com_is_explicitly_opt_in() {
    if std::env::var_os("WILD_BUZZARD_PUBLIC_NETWORK").as_deref() != Some(std::ffi::OsStr::new("1"))
    {
        return;
    }
    let client = GeneralWebClient::with_bundled_roots(GeneralWebConfig::default())
        .expect("construct public-network client");
    let target = GeneralWebTarget::parse("https://example.com/").expect("parse public URL");
    assert!(target.url().username().is_empty());
    assert!(target.url().password().is_none());
    let request = GeneralWebRequest::get(target, RedirectPolicy::Manual)
        .with_deadline(Instant::now() + Duration::from_secs(15));
    let response = client.execute(&request).expect("fetch example.com");
    let status = response.head().status().as_u16();
    let security = response.security();
    let body = response.read_body_to_end().expect("read public body");
    let body_limit = client.config().http_config().max_body_bytes();
    eprintln!(
        "target=https://example.com/ credentials=false status={status} security={security:?} body_bytes={} body_limit={body_limit}",
        body.len()
    );
    assert_eq!(status, 200);
    assert!(!body.is_empty());
    assert!(body.len() <= body_limit);
}
