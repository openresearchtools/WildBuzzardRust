use std::fs;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use wild_buzzard_engine::{
    CancellationSource, EngineEventKind, EngineLimits, EventReceiveError, ExecutionFailureKind,
    FontSourcePolicy, FrameLease, MAX_NAVIGATION_URL_BYTES, MAX_TOP_LEVEL_REDIRECTS,
    NavigationAlpn, NavigationCommitError, NavigationCommitMetadata,
    NavigationCommitValidationError, NavigationConnectionSecurity, NavigationEngine,
    NavigationGeneration, NavigationId, NavigationRequest, NavigationStage, NavigationTlsVersion,
    PipelineError, RedirectLocationFailure, StaticPageConfig, StaticPageEngine, TopLevelContextId,
    WorkerStopReason,
};
use wild_buzzard_headless::HeadlessLimits;
use wild_buzzard_net::{ClientConfig, GeneralWebConfig, TrustStore};

const DESKTOP_WIDTH: u32 = 1366;
const DESKTOP_HEIGHT: u32 = 768;
const FULL_HD_WIDTH: u32 = 1920;
const FULL_HD_HEIGHT: u32 = 1080;
const SERVER_TIMEOUT: Duration = Duration::from_secs(3);
const PAGE: &[u8] = br"<!doctype html><style>html,body{margin:0;background:#183c67}main{display:block;width:720px;height:240px;background:#e4edf7;color:#142d4a}</style><main>Redirected browser page</main>";

static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(1);

fn http_config() -> ClientConfig {
    ClientConfig::default()
        .with_max_body_bytes(512 * 1024)
        .with_connect_timeout(Duration::from_secs(1))
        .with_read_timeout(Duration::from_secs(2))
        .with_write_timeout(Duration::from_secs(2))
}

fn general_web_config(http: ClientConfig) -> GeneralWebConfig {
    GeneralWebConfig::default()
        .with_http_config(http)
        .with_dns_timeout(Duration::from_secs(2))
        .with_tls_handshake_timeout(Duration::from_secs(2))
        .with_max_dns_candidates(8)
        .with_max_connection_attempts(8)
}

fn page_config(width: u32, height: u32, timeout: Duration) -> StaticPageConfig {
    let pixels = usize::try_from(width)
        .unwrap()
        .checked_mul(usize::try_from(height).unwrap())
        .and_then(|pixels| pixels.checked_mul(4))
        .unwrap();
    StaticPageConfig {
        viewport_width: width,
        viewport_height: height,
        operation_timeout: timeout,
        network: http_config(),
        font_source: FontSourcePolicy::EmbeddedOnly,
        headless: HeadlessLimits::default()
            .with_max_width(width)
            .with_max_height(height)
            .with_max_pixel_bytes(pixels),
        ..StaticPageConfig::default()
    }
}

fn spawn_engine(
    width: u32,
    height: u32,
    timeout: Duration,
    trust: TrustStore,
) -> (NavigationEngine, wild_buzzard_engine::EngineEventReceiver) {
    let config = page_config(width, height, timeout);
    let general = general_web_config(config.network.clone());
    NavigationEngine::spawn_general_web(config, general, trust, EngineLimits::default()).unwrap()
}

fn response(status: &str, fields: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let mut bytes = format!("HTTP/1.1 {status}\r\n").into_bytes();
    for (name, value) in fields {
        bytes.extend_from_slice(name.as_bytes());
        bytes.extend_from_slice(b": ");
        bytes.extend_from_slice(value.as_bytes());
        bytes.extend_from_slice(b"\r\n");
    }
    bytes.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes());
    bytes.extend_from_slice(b"Connection: close\r\n\r\n");
    bytes.extend_from_slice(body);
    bytes
}

fn response_with_raw_locations(status: &str, locations: &[&[u8]]) -> Vec<u8> {
    let mut bytes = format!("HTTP/1.1 {status}\r\n").into_bytes();
    for location in locations {
        bytes.extend_from_slice(b"Location: ");
        bytes.extend_from_slice(location);
        bytes.extend_from_slice(b"\r\n");
    }
    bytes.extend_from_slice(b"Content-Length: 0\r\nConnection: close\r\n\r\n");
    bytes
}

fn read_request_head(stream: &mut TcpStream) -> io::Result<Vec<u8>> {
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() == 64 * 1024 {
            return Err(io::Error::other("fixture request head exceeded bound"));
        }
        stream.read_exact(&mut byte)?;
        head.push(byte[0]);
    }
    Ok(head)
}

fn spawn_http_script(
    build: impl FnOnce(u16) -> Vec<(String, Vec<u8>)>,
) -> (String, JoinHandle<Vec<Vec<u8>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let steps = build(port);
    let handle = thread::spawn(move || {
        let mut requests = Vec::with_capacity(steps.len());
        for (path, response) in steps {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(SERVER_TIMEOUT)).unwrap();
            stream.set_write_timeout(Some(SERVER_TIMEOUT)).unwrap();
            let request = read_request_head(&mut stream).unwrap();
            assert!(
                request.starts_with(format!("GET {path} HTTP/1.1\r\n").as_bytes()),
                "expected request target {path}, got {}",
                String::from_utf8_lossy(&request)
            );
            stream.write_all(&response).unwrap();
            requests.push(request);
        }
        requests
    });
    (format!("http://127.0.0.1:{port}"), handle)
}

fn assert_frame_size(frame: &FrameLease, width: u32, height: u32) {
    let rgba = frame.metadata().rgba8().unwrap();
    assert_eq!(rgba.size().width(), width);
    assert_eq!(rgba.size().height(), height);
    assert!(
        frame
            .rgba8_pixels()
            .unwrap()
            .chunks_exact(4)
            .any(|pixel| pixel != [255, 255, 255, 255]),
        "the final response must visibly render"
    );
}

fn receive_commit_and_frame(
    engine: &NavigationEngine,
    receiver: &mut wild_buzzard_engine::EngineEventReceiver,
    context: TopLevelContextId,
    url: &str,
) -> (NavigationId, NavigationCommitMetadata, FrameLease) {
    let navigation = engine
        .navigate(context, NavigationRequest::general_web(url).unwrap())
        .unwrap();
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
    let commitment = receiver.take_navigation_commit(navigation).unwrap();
    assert_eq!(commitment.navigation(), navigation);
    let ready = receiver.recv().unwrap();
    let EngineEventKind::FrameReady {
        navigation: ready_navigation,
        lease,
        metadata,
    } = ready.kind()
    else {
        panic!("successful redirect navigation did not publish a frame");
    };
    assert_eq!(ready_navigation, navigation);
    let frame = receiver.take_frame(lease).unwrap();
    assert_eq!(frame.metadata(), metadata);
    (navigation, commitment.into_metadata(), frame)
}

fn admitted_get_redirect_script(port: u16) -> Vec<(String, Vec<u8>)> {
    vec![
        (
            "/directory/start".into(),
            response(
                "301 Moved Permanently",
                &[("Location", "../permanent")],
                b"ignored",
            ),
        ),
        (
            "/permanent".into(),
            response("303 See Other", &[("Location", "/see-other")], b"ignored"),
        ),
        (
            "/see-other".into(),
            response("302 Found", &[("Location", "/middle")], b"ignored"),
        ),
        (
            "/middle".into(),
            response(
                "307 Temporary Redirect",
                &[(
                    "Location",
                    &format!("http://127.0.0.1:{port}/almost#section"),
                )],
                b"ignored",
            ),
        ),
        (
            "/almost".into(),
            response(
                "308 Permanent Redirect",
                &[("Location", "/final?q=rust")],
                b"ignored",
            ),
        ),
        (
            "/final?q=rust".into(),
            response(
                "200 OK",
                &[("Content-Type", "text/html; charset=utf-8")],
                PAGE,
            ),
        ),
    ]
}

#[test]
fn every_admitted_get_redirect_status_preserves_get_and_publishes_final_identity_once() {
    let (origin, server) = spawn_http_script(admitted_get_redirect_script);
    let start = format!("{origin}/directory/start#initial");
    let expected = format!("{origin}/final?q=rust#section");
    let (mut engine, mut receiver) = spawn_engine(
        DESKTOP_WIDTH,
        DESKTOP_HEIGHT,
        Duration::from_secs(10),
        TrustStore::bundled_web_pki(),
    );
    let context = TopLevelContextId::new(1).unwrap();
    let navigation = engine
        .navigate(context, NavigationRequest::general_web(&start).unwrap())
        .unwrap();
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

    let foreign = NavigationId::new(
        context,
        NavigationGeneration::INITIAL.checked_next().unwrap(),
    );
    assert_eq!(
        receiver.take_navigation_commit(foreign),
        Err(NavigationCommitError::Unknown),
        "a foreign generation must not consume the exact commitment"
    );
    let commitment = receiver.take_navigation_commit(navigation).unwrap();
    assert_eq!(commitment.metadata().final_url(), expected);
    assert_eq!(commitment.metadata().redirect_count(), 5);
    assert_eq!(
        commitment.metadata().security(),
        NavigationConnectionSecurity::Cleartext
    );
    assert!(!commitment.metadata().had_https_downgrade());
    assert_eq!(
        receiver.take_navigation_commit(navigation),
        Err(NavigationCommitError::Stale),
        "the commitment is one-shot"
    );

    let ready = receiver.recv().unwrap();
    let EngineEventKind::FrameReady { lease, .. } = ready.kind() else {
        panic!("final response did not publish a frame");
    };
    let frame = receiver.take_frame(lease).unwrap();
    assert_frame_size(&frame, DESKTOP_WIDTH, DESKTOP_HEIGHT);
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 6);
    assert!(
        requests.iter().all(|request| request.starts_with(b"GET ")),
        "all 301/302/303/307/308 hops must remain top-level GET requests"
    );
    assert_eq!(engine.shutdown().reason(), WorkerStopReason::Requested);
}

#[test]
fn product_commit_validation_rejects_hostile_identity_and_security_claims() {
    let tls = NavigationConnectionSecurity::AuthenticatedTls {
        version: NavigationTlsVersion::Tls13,
        alpn: NavigationAlpn::Http11,
    };
    let valid = NavigationCommitMetadata::new(
        "https://example.test/final#section",
        MAX_TOP_LEVEL_REDIRECTS,
        tls,
        true,
    )
    .unwrap();
    assert_eq!(valid.validate_general_web(), Ok(()));

    for invalid in [
        "not a URL",
        "javascript:alert(1)",
        "https://user:secret@example.test/",
    ] {
        let commitment = NavigationCommitMetadata::new(invalid, 1, tls, false).unwrap();
        assert_eq!(
            commitment.validate_general_web(),
            Err(NavigationCommitValidationError::InvalidFinalUrl)
        );
    }
    let noncanonical = NavigationCommitMetadata::new(
        "HTTP://PLAIN.TEST/noncanonical",
        1,
        NavigationConnectionSecurity::Cleartext,
        false,
    )
    .unwrap();
    assert_eq!(
        noncanonical.validate_general_web(),
        Err(NavigationCommitValidationError::NonCanonicalFinalUrl)
    );
    let mismatch = NavigationCommitMetadata::new("http://plain.test/", 1, tls, false).unwrap();
    assert_eq!(
        mismatch.validate_general_web(),
        Err(NavigationCommitValidationError::SchemeSecurityMismatch)
    );
    let https_cleartext = NavigationCommitMetadata::new(
        "https://secure.test/",
        1,
        NavigationConnectionSecurity::Cleartext,
        false,
    )
    .unwrap();
    assert_eq!(
        https_cleartext.validate_general_web(),
        Err(NavigationCommitValidationError::SchemeSecurityMismatch)
    );
    let unverified = NavigationCommitMetadata::new(
        "https://example.test/",
        0,
        NavigationConnectionSecurity::Unverified,
        false,
    )
    .unwrap();
    assert_eq!(
        unverified.validate_general_web(),
        Err(NavigationCommitValidationError::UnverifiedSecurity)
    );
    let excessive = NavigationCommitMetadata::new(
        "https://example.test/",
        MAX_TOP_LEVEL_REDIRECTS + 1,
        tls,
        false,
    )
    .unwrap();
    assert_eq!(
        excessive.validate_general_web(),
        Err(NavigationCommitValidationError::TooManyRedirects)
    );
}

#[derive(Clone, Copy, Debug)]
enum ExpectedRedirectFailure {
    Location(RedirectLocationFailure),
    UnsupportedStatus(u16),
}

fn assert_redirect_failure(error: &PipelineError, expected: ExpectedRedirectFailure) {
    match (error, expected) {
        (PipelineError::RedirectLocation(actual), ExpectedRedirectFailure::Location(expected)) => {
            assert_eq!(*actual, expected);
        }
        (
            PipelineError::UnsupportedRedirectStatus { status: actual },
            ExpectedRedirectFailure::UnsupportedStatus(expected),
        ) => assert_eq!(*actual, expected),
        _ => panic!("expected {expected:?}, received {error:?}"),
    }
}

fn redirect_failure_cases() -> Vec<(Vec<u8>, ExpectedRedirectFailure)> {
    let overlong_location = format!("/{}", "x".repeat(MAX_NAVIGATION_URL_BYTES));
    vec![
        (
            response("301 Moved Permanently", &[], b"ignored"),
            ExpectedRedirectFailure::Location(RedirectLocationFailure::Missing),
        ),
        (
            response(
                "302 Found",
                &[("Location", "/first"), ("Location", "/second")],
                b"ignored",
            ),
            ExpectedRedirectFailure::Location(RedirectLocationFailure::Multiple),
        ),
        (
            response_with_raw_locations("303 See Other", &[b"/invalid-\xff"]),
            ExpectedRedirectFailure::Location(RedirectLocationFailure::NonUtf8),
        ),
        (
            response(
                "307 Temporary Redirect",
                &[("Location", "http://user:secret@127.0.0.1/private")],
                b"ignored",
            ),
            ExpectedRedirectFailure::Location(RedirectLocationFailure::CredentialsNotAllowed),
        ),
        (
            response(
                "308 Permanent Redirect",
                &[("Location", "javascript:alert(1)")],
                b"ignored",
            ),
            ExpectedRedirectFailure::Location(RedirectLocationFailure::UnsupportedScheme),
        ),
        (
            response(
                "300 Multiple Choices",
                &[("Location", "/unused")],
                b"ignored",
            ),
            ExpectedRedirectFailure::UnsupportedStatus(300),
        ),
        (
            response(
                "302 Found",
                &[("Location", overlong_location.as_str())],
                b"ignored",
            ),
            ExpectedRedirectFailure::Location(RedirectLocationFailure::UrlTooLong),
        ),
    ]
}

#[test]
fn malformed_and_prohibited_redirects_fail_typed_without_document_publication() {
    let static_config = page_config(DESKTOP_WIDTH, DESKTOP_HEIGHT, Duration::from_secs(10));
    let static_general = general_web_config(static_config.network.clone());
    let mut static_engine = StaticPageEngine::new_general_web(
        static_config,
        static_general,
        TrustStore::bundled_web_pki(),
    )
    .unwrap();
    let (mut navigation_engine, mut receiver) = spawn_engine(
        DESKTOP_WIDTH,
        DESKTOP_HEIGHT,
        Duration::from_secs(10),
        TrustStore::bundled_web_pki(),
    );

    for (case, (response_bytes, expected)) in redirect_failure_cases().into_iter().enumerate() {
        let repeated = response_bytes.clone();
        let (origin, server) = spawn_http_script(move |_| {
            vec![
                ("/start".into(), response_bytes),
                ("/start".into(), repeated),
            ]
        });
        let url = format!("{origin}/start");
        let error = static_engine
            .load_general_web(&url, &CancellationSource::new().token())
            .unwrap_err();
        assert_redirect_failure(&error, expected);
        assert!(
            static_engine.live_document().is_none(),
            "redirect case {case} published a static document"
        );

        let context = TopLevelContextId::new(u64::try_from(case).unwrap() + 20).unwrap();
        let navigation = navigation_engine
            .navigate(context, NavigationRequest::general_web(&url).unwrap())
            .unwrap();
        assert_eq!(
            receiver.recv().unwrap().kind(),
            EngineEventKind::NavigationStarted { navigation }
        );
        let failed = receiver.recv().unwrap();
        let EngineEventKind::NavigationFailed {
            navigation: failed_navigation,
            failure,
        } = failed.kind()
        else {
            panic!("redirect case {case} published a document event: {failed:?}");
        };
        assert_eq!(failed_navigation, navigation);
        assert_eq!(failure.kind(), ExecutionFailureKind::Rejected);
        assert_eq!(failure.stage(), NavigationStage::Fetch);
        assert_eq!(receiver.try_recv(), Err(EventReceiveError::Empty));
        assert_eq!(server.join().unwrap().len(), 2);
    }

    static_engine.shutdown().unwrap();
    assert_eq!(
        navigation_engine.shutdown().reason(),
        WorkerStopReason::Requested
    );
}

#[test]
fn redirect_loop_and_ten_hop_limit_fail_closed_before_document_publication() {
    let config = page_config(DESKTOP_WIDTH, DESKTOP_HEIGHT, Duration::from_secs(10));
    let general = general_web_config(config.network.clone());
    let mut engine =
        StaticPageEngine::new_general_web(config, general, TrustStore::bundled_web_pki()).unwrap();

    let (loop_origin, loop_server) = spawn_http_script(|_| {
        vec![(
            "/loop".into(),
            response("302 Found", &[("Location", "/loop")], b"ignored"),
        )]
    });
    let loop_error = engine
        .load_general_web(
            &format!("{loop_origin}/loop"),
            &CancellationSource::new().token(),
        )
        .unwrap_err();
    assert!(matches!(loop_error, PipelineError::RedirectLoop));
    loop_server.join().unwrap();

    let (hop_origin, hop_server) = spawn_http_script(|_| {
        (0..=10)
            .map(|hop| {
                (
                    format!("/{hop}"),
                    response(
                        "302 Found",
                        &[("Location", &format!("/{}", hop + 1))],
                        b"ignored",
                    ),
                )
            })
            .collect()
    });
    let hop_error = engine
        .load_general_web(
            &format!("{hop_origin}/0"),
            &CancellationSource::new().token(),
        )
        .unwrap_err();
    assert!(matches!(
        hop_error,
        PipelineError::TooManyRedirects {
            maximum: MAX_TOP_LEVEL_REDIRECTS
        }
    ));
    assert_eq!(hop_server.join().unwrap().len(), 11);
    engine.shutdown().unwrap();
}

fn spawn_redirect_then_stall(stall: Duration) -> (String, Receiver<()>, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (accepted_sender, accepted_receiver) = mpsc::sync_channel(1);
    let handle = thread::spawn(move || {
        let (mut first, _) = listener.accept().unwrap();
        first.set_read_timeout(Some(SERVER_TIMEOUT)).unwrap();
        let head = read_request_head(&mut first).unwrap();
        assert!(head.starts_with(b"GET /start HTTP/1.1\r\n"));
        first
            .write_all(&response(
                "302 Found",
                &[("Location", "/stall")],
                b"ignored",
            ))
            .unwrap();
        drop(first);

        let (mut second, _) = listener.accept().unwrap();
        second.set_read_timeout(Some(SERVER_TIMEOUT)).unwrap();
        let head = read_request_head(&mut second).unwrap();
        assert!(head.starts_with(b"GET /stall HTTP/1.1\r\n"));
        accepted_sender.send(()).unwrap();
        thread::sleep(stall);
    });
    (
        format!("http://127.0.0.1:{port}/start"),
        accepted_receiver,
        handle,
    )
}

#[test]
fn cancellation_and_absolute_deadline_continue_across_redirect_hops() {
    let (cancel_url, cancel_accepted, cancel_server) =
        spawn_redirect_then_stall(Duration::from_millis(350));
    let (mut engine, mut receiver) = spawn_engine(
        DESKTOP_WIDTH,
        DESKTOP_HEIGHT,
        Duration::from_secs(10),
        TrustStore::bundled_web_pki(),
    );
    let navigation = engine
        .navigate(
            TopLevelContextId::new(2).unwrap(),
            NavigationRequest::general_web(&cancel_url).unwrap(),
        )
        .unwrap();
    assert_eq!(
        receiver.recv().unwrap().kind(),
        EngineEventKind::NavigationStarted { navigation }
    );
    cancel_accepted.recv_timeout(SERVER_TIMEOUT).unwrap();
    engine.cancel_navigation(navigation).unwrap();
    assert_eq!(
        receiver.recv().unwrap().kind(),
        EngineEventKind::NavigationCancelled { navigation }
    );
    cancel_server.join().unwrap();
    assert_eq!(engine.shutdown().reason(), WorkerStopReason::Requested);

    let (deadline_url, deadline_accepted, deadline_server) =
        spawn_redirect_then_stall(Duration::from_millis(350));
    let (mut engine, mut receiver) = spawn_engine(
        DESKTOP_WIDTH,
        DESKTOP_HEIGHT,
        Duration::from_millis(120),
        TrustStore::bundled_web_pki(),
    );
    let navigation = engine
        .navigate(
            TopLevelContextId::new(3).unwrap(),
            NavigationRequest::general_web(&deadline_url).unwrap(),
        )
        .unwrap();
    assert_eq!(
        receiver.recv().unwrap().kind(),
        EngineEventKind::NavigationStarted { navigation }
    );
    deadline_accepted.recv_timeout(SERVER_TIMEOUT).unwrap();
    let failed = receiver.recv().unwrap();
    let EngineEventKind::NavigationFailed {
        navigation: failed_navigation,
        failure,
    } = failed.kind()
    else {
        panic!("redirect deadline did not fail the navigation");
    };
    assert_eq!(failed_navigation, navigation);
    assert_eq!(failure.kind(), ExecutionFailureKind::DeadlineExceeded);
    assert_eq!(failure.stage(), NavigationStage::Fetch);
    deadline_server.join().unwrap();
    assert_eq!(engine.shutdown().reason(), WorkerStopReason::Requested);
}

struct OpenSslHttpFixture {
    directory: PathBuf,
    certificate_der: Vec<u8>,
    origin: String,
    child: Option<Child>,
}

impl OpenSslHttpFixture {
    fn start(files: Vec<(&str, Vec<u8>)>, accepts: usize) -> Self {
        let directory = unique_test_directory();
        fs::create_dir_all(&directory).unwrap();
        let certificate_pem = directory.join("certificate.pem");
        let certificate_der = directory.join("certificate.der");
        let private_key = directory.join("private-key.pem");
        for (name, contents) in files {
            fs::write(directory.join(name), contents).unwrap();
        }
        let output = Command::new("openssl")
            .args([
                "req",
                "-x509",
                "-newkey",
                "rsa:2048",
                "-sha256",
                "-nodes",
                "-days",
                "2",
                "-subj",
                "/CN=localhost",
                "-addext",
                "subjectAltName=DNS:localhost",
                "-addext",
                "basicConstraints=critical,CA:FALSE",
                "-addext",
                "keyUsage=critical,digitalSignature,keyEncipherment",
                "-addext",
                "extendedKeyUsage=serverAuth",
                "-keyout",
            ])
            .arg(&private_key)
            .arg("-out")
            .arg(&certificate_pem)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let output = Command::new("openssl")
            .args(["x509", "-in"])
            .arg(&certificate_pem)
            .args(["-outform", "DER", "-out"])
            .arg(&certificate_der)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );

        let address = reserve_address();
        let mut child = Command::new("openssl")
            .args([
                "s_server", "-quiet", "-HTTP", "-tls1_3", "-alpn", "http/1.1", "-accept",
            ])
            .arg(address.to_string())
            .arg("-cert")
            .arg(&certificate_pem)
            .arg("-key")
            .arg(&private_key)
            .arg("-naccept")
            .arg(accepts.to_string())
            .current_dir(&directory)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        wait_for_listener(&mut child, address);
        Self {
            certificate_der: fs::read(certificate_der).unwrap(),
            origin: format!("https://localhost:{}", address.port()),
            directory,
            child: Some(child),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.origin)
    }

    fn finish(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        let deadline = Instant::now() + SERVER_TIMEOUT;
        loop {
            match child.try_wait().unwrap() {
                Some(status) => {
                    assert!(status.success(), "openssl fixture failed: {status}");
                    return;
                }
                None if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
                None => {
                    child.kill().unwrap();
                    let _ = child.wait();
                    panic!("openssl fixture did not finish");
                }
            }
        }
    }
}

impl Drop for OpenSslHttpFixture {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = fs::remove_dir_all(&self.directory);
    }
}

fn unique_test_directory() -> PathBuf {
    let root = std::env::var_os("CARGO_TARGET_DIR").map_or_else(std::env::temp_dir, PathBuf::from);
    let sequence = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
    root.join(format!(
        "wild-buzzard-redirect-tls-{}-{sequence}",
        std::process::id()
    ))
}

fn reserve_address() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap()
}

fn wait_for_listener(child: &mut Child, address: SocketAddr) {
    let deadline = Instant::now() + SERVER_TIMEOUT;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("openssl fixture exited during startup: {status}");
        }
        match TcpListener::bind(address) {
            Err(error) if error.kind() == io::ErrorKind::AddrInUse => return,
            Ok(listener) => drop(listener),
            Err(error) => panic!("inspect openssl listener: {error}"),
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("openssl fixture did not bind");
}

#[test]
fn authenticated_tls_and_sticky_https_downgrade_are_published_exactly() {
    let secure_response = response(
        "200 OK",
        &[("Content-Type", "text/html; charset=utf-8")],
        PAGE,
    );
    let mut secure = OpenSslHttpFixture::start(vec![("secure", secure_response)], 1);

    let (http_origin, http_server) = spawn_http_script(|_| {
        vec![(
            "/final".into(),
            response(
                "200 OK",
                &[("Content-Type", "text/html; charset=utf-8")],
                PAGE,
            ),
        )]
    });
    let downgrade_response = response(
        "302 Found",
        &[("Location", &format!("{http_origin}/final#land"))],
        b"ignored",
    );
    let mut downgrade = OpenSslHttpFixture::start(vec![("downgrade", downgrade_response)], 1);

    let trust = TrustStore::bundled_web_pki()
        .with_der_certificate(&secure.certificate_der)
        .unwrap()
        .with_der_certificate(&downgrade.certificate_der)
        .unwrap();
    let (mut engine, mut receiver) = spawn_engine(
        FULL_HD_WIDTH,
        FULL_HD_HEIGHT,
        Duration::from_secs(10),
        trust,
    );

    let secure_url = secure.url("/secure");
    let (_, secure_commit, secure_frame) = receive_commit_and_frame(
        &engine,
        &mut receiver,
        TopLevelContextId::new(4).unwrap(),
        &secure_url,
    );
    assert_eq!(secure_commit.final_url(), secure_url);
    assert_eq!(secure_commit.redirect_count(), 0);
    assert_eq!(
        secure_commit.security(),
        NavigationConnectionSecurity::AuthenticatedTls {
            version: NavigationTlsVersion::Tls13,
            alpn: NavigationAlpn::Http11,
        }
    );
    assert!(!secure_commit.had_https_downgrade());
    assert_frame_size(&secure_frame, FULL_HD_WIDTH, FULL_HD_HEIGHT);

    let downgrade_url = downgrade.url("/downgrade");
    let (_, downgrade_commit, downgrade_frame) = receive_commit_and_frame(
        &engine,
        &mut receiver,
        TopLevelContextId::new(5).unwrap(),
        &downgrade_url,
    );
    assert_eq!(
        downgrade_commit.final_url(),
        format!("{http_origin}/final#land")
    );
    assert_eq!(downgrade_commit.redirect_count(), 1);
    assert_eq!(
        downgrade_commit.security(),
        NavigationConnectionSecurity::Cleartext
    );
    assert!(downgrade_commit.had_https_downgrade());
    assert_frame_size(&downgrade_frame, FULL_HD_WIDTH, FULL_HD_HEIGHT);

    secure.finish();
    downgrade.finish();
    http_server.join().unwrap();
    assert_eq!(engine.shutdown().reason(), WorkerStopReason::Requested);
}
