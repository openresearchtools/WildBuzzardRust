use std::fs;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use wild_buzzard_engine::{
    CancellationSource, EngineEventKind, EngineLimits, ExecutionFailureKind, FontSourcePolicy,
    FrameLease, NavigationEngine, NavigationNetworkCapability, NavigationRequest, NavigationStage,
    PipelineError, StaticPageConfig, StaticPageEngine, TopLevelContextId, WorkerStopReason,
};
use wild_buzzard_headless::HeadlessLimits;
use wild_buzzard_net::{ClientConfig, GeneralWebConfig, TrustStore};

const DESKTOP_WIDTH: u32 = 1366;
const DESKTOP_HEIGHT: u32 = 768;
const FULL_HD_WIDTH: u32 = 1920;
const FULL_HD_HEIGHT: u32 = 1080;
const SERVER_TIMEOUT: Duration = Duration::from_secs(3);
const CLEAR: [u8; 4] = [255, 255, 255, 255];
const NAVY: [u8; 4] = [28, 46, 74, 255];
const SEARCH: [u8; 4] = [230, 238, 248, 255];

const DESKTOP_DOCUMENT: &str = r#"<!doctype html>
<html>
<head>
  <meta charset="utf-8">
  <title>Wild Buzzard general navigation fixture</title>
  <style>
    html, body { margin: 0; background-color: rgb(255 255 255); color: rgb(28 46 74); }
    #masthead { display: block; width: 100%; height: 68px; background-color: rgb(28 46 74); color: white; font-size: 20px; line-height: 60px; padding-left: 28px; }
    #search { display: block; width: 760px; height: 52px; margin-top: 96px; margin-left: 180px; padding: 12px; background-color: rgb(230 238 248); color: rgb(28 46 74); font-size: 18px; line-height: 28px; }
    #result { display: block; width: 880px; height: 96px; margin-top: 36px; margin-left: 180px; padding: 14px; background-color: rgb(245 248 252); color: rgb(20 70 140); font-size: 16px; line-height: 26px; }
  </style>
</head>
<body>
  <div id="masthead">Wild Buzzard</div>
  <div id="search">Search the open web</div>
  <div id="result">A real HTTP response reached HTML, DOM, Stylo, layout, text shaping, and WebRender.</div>
</body>
</html>"#;

static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(1);

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

fn page_config(width: u32, height: u32, operation_timeout: Duration) -> StaticPageConfig {
    let pixel_bytes = usize::try_from(width)
        .ok()
        .and_then(|width| {
            usize::try_from(height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .and_then(|pixels| pixels.checked_mul(4))
        .expect("desktop fixture dimensions fit RGBA8 bytes");
    StaticPageConfig {
        viewport_width: width,
        viewport_height: height,
        operation_timeout,
        network: http_config(),
        font_source: FontSourcePolicy::EmbeddedOnly,
        headless: HeadlessLimits::default()
            .with_max_width(width)
            .with_max_height(height)
            .with_max_pixel_bytes(pixel_bytes),
        ..StaticPageConfig::default()
    }
}

fn response(status: &str, fields: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let mut response = format!("HTTP/1.1 {status}\r\n").into_bytes();
    for (name, value) in fields {
        response.extend_from_slice(name.as_bytes());
        response.extend_from_slice(b": ");
        response.extend_from_slice(value.as_bytes());
        response.extend_from_slice(b"\r\n");
    }
    response.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes());
    response.extend_from_slice(b"Connection: close\r\n\r\n");
    response.extend_from_slice(body);
    response
}

fn spawn_http_server(
    host_in_url: &str,
    path: &str,
    response: Vec<u8>,
) -> (String, JoinHandle<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind deterministic HTTP fixture");
    let address = listener.local_addr().expect("read fixture address");
    let path = path.to_owned();
    let url = format!("http://{host_in_url}:{}{path}", address.port());
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept general-web request");
        stream
            .set_read_timeout(Some(SERVER_TIMEOUT))
            .expect("set fixture read timeout");
        stream
            .set_write_timeout(Some(SERVER_TIMEOUT))
            .expect("set fixture write timeout");
        let request = read_request_head(&mut stream).expect("read bounded request head");
        assert!(
            request.starts_with(format!("GET {path} HTTP/1.1\r\n").as_bytes()),
            "the requested origin-form path must reach the fixture"
        );
        stream
            .write_all(&response)
            .expect("write deterministic HTTP response");
        request
    });
    (url, handle)
}

fn spawn_stalling_http_server(
    path: &str,
    stall: Duration,
) -> (String, Receiver<()>, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind stalling HTTP fixture");
    let address = listener.local_addr().expect("read stalling address");
    let path = path.to_owned();
    let url = format!("http://localhost:{}{path}", address.port());
    let (accepted_sender, accepted_receiver) = mpsc::sync_channel(1);
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept stalling request");
        stream
            .set_read_timeout(Some(SERVER_TIMEOUT))
            .expect("set stalling read timeout");
        let request = read_request_head(&mut stream).expect("read stalling request head");
        assert!(request.starts_with(format!("GET {path} HTTP/1.1\r\n").as_bytes()));
        accepted_sender
            .send(())
            .expect("test still observes accepted request");
        thread::sleep(stall);
    });
    (url, accepted_receiver, handle)
}

fn read_request_head(stream: &mut TcpStream) -> io::Result<Vec<u8>> {
    let mut request = Vec::new();
    let mut byte = [0_u8; 1];
    while !request.ends_with(b"\r\n\r\n") {
        if request.len() == 64 * 1024 {
            return Err(io::Error::other("fixture request head exceeded bound"));
        }
        stream.read_exact(&mut byte)?;
        request.push(byte[0]);
    }
    Ok(request)
}

fn spawn_engine(
    width: u32,
    height: u32,
    operation_timeout: Duration,
    trust_store: TrustStore,
) -> (NavigationEngine, wild_buzzard_engine::EngineEventReceiver) {
    let config = page_config(width, height, operation_timeout);
    let general_web = general_web_config(config.network.clone());
    NavigationEngine::spawn_general_web(config, general_web, trust_store, EngineLimits::default())
        .expect("general-web pipeline must initialize on its worker")
}

fn navigate_to_visible_frame(
    engine: &NavigationEngine,
    receiver: &mut wild_buzzard_engine::EngineEventReceiver,
    context: TopLevelContextId,
    url: &str,
) -> FrameLease {
    let request = NavigationRequest::general_web(url).expect("construct bounded general request");
    assert_eq!(
        request.network_capability(),
        NavigationNetworkCapability::GeneralWeb
    );
    let navigation = engine
        .navigate(context, request)
        .expect("admit general-web navigation");
    assert_eq!(
        receiver.recv().expect("started event").kind(),
        EngineEventKind::NavigationStarted { navigation }
    );
    assert_eq!(
        receiver.recv().expect("committed event").kind(),
        EngineEventKind::NavigationCommitted {
            navigation,
            http_status: 200,
        }
    );
    let ready = receiver.recv().expect("frame-ready event");
    let EngineEventKind::FrameReady {
        navigation: ready_navigation,
        lease,
        metadata,
    } = ready.kind()
    else {
        panic!("successful general navigation must publish a frame");
    };
    assert_eq!(ready_navigation, navigation);
    let frame = receiver
        .take_frame(lease)
        .expect("transfer exact current frame lease");
    assert_eq!(frame.navigation(), navigation);
    assert_eq!(frame.metadata(), metadata);
    assert!(frame.document_version().is_some());
    frame
}

fn assert_visible_desktop_frame(frame: &FrameLease, width: u32, height: u32) {
    let rgba = frame
        .metadata()
        .rgba8()
        .expect("headless general navigation returns RGBA8 metadata");
    assert_eq!(rgba.size().width(), width);
    assert_eq!(rgba.size().height(), height);
    assert_eq!(rgba.stride(), usize::try_from(width).unwrap() * 4);
    let pixels = frame
        .rgba8_pixels()
        .expect("headless general navigation owns pixels");
    assert_eq!(pixels.len(), rgba.byte_len());
    assert!(
        pixels.chunks_exact(4).any(|pixel| pixel == NAVY),
        "the author masthead color must reach the composed desktop frame"
    );
    assert!(
        pixels.chunks_exact(4).any(|pixel| pixel == SEARCH),
        "the author search-panel color must reach the composed desktop frame"
    );
    assert!(
        pixels.chunks_exact(4).any(|pixel| pixel != CLEAR),
        "the visible frame must not be an untouched clear surface"
    );
}

#[test]
fn general_http_dns_navigation_renders_a_visible_1366_by_768_frame() {
    let body = DESKTOP_DOCUMENT.as_bytes();
    let response = response(
        "200 OK",
        &[("Content-Type", "text/html; charset=utf-8")],
        body,
    );
    let (url, server) = spawn_http_server("localhost", "/search/index.html?q=rust", response);
    let (mut engine, mut receiver) = spawn_engine(
        DESKTOP_WIDTH,
        DESKTOP_HEIGHT,
        Duration::from_secs(10),
        TrustStore::bundled_web_pki(),
    );
    let frame = navigate_to_visible_frame(
        &engine,
        &mut receiver,
        TopLevelContextId::new(1).unwrap(),
        &url,
    );
    assert_visible_desktop_frame(&frame, DESKTOP_WIDTH, DESKTOP_HEIGHT);

    let request = server.join().expect("HTTP fixture must finish");
    assert!(
        request
            .windows(b"Host: localhost:".len())
            .any(|window| window.eq_ignore_ascii_case(b"Host: localhost:")),
        "the validated DNS authority must be serialized as Host"
    );
    assert_eq!(engine.shutdown().reason(), WorkerStopReason::Requested);
}

#[test]
fn authenticated_local_https_navigation_renders_a_visible_1920_by_1080_frame() {
    let mut tls = OpenSslTlsFixture::start(DESKTOP_DOCUMENT);
    let trust_store = TrustStore::bundled_web_pki()
        .with_der_certificate(tls.certificate_der())
        .expect("admit the local fixture's exact trust anchor");
    let (mut engine, mut receiver) = spawn_engine(
        FULL_HD_WIDTH,
        FULL_HD_HEIGHT,
        Duration::from_secs(10),
        trust_store,
    );
    let frame = navigate_to_visible_frame(
        &engine,
        &mut receiver,
        TopLevelContextId::new(2).unwrap(),
        tls.url(),
    );
    assert_visible_desktop_frame(&frame, FULL_HD_WIDTH, FULL_HD_HEIGHT);
    tls.finish();
    assert_eq!(engine.shutdown().reason(), WorkerStopReason::Requested);
}

#[test]
fn superseding_a_general_fetch_cancels_the_stale_generation_before_publication() {
    let (stale_url, stale_accepted, stale_server) =
        spawn_stalling_http_server("/stale", Duration::from_millis(400));
    let fresh_response = response(
        "200 OK",
        &[("Content-Type", "text/html; charset=utf-8")],
        DESKTOP_DOCUMENT.as_bytes(),
    );
    let (fresh_url, fresh_server) = spawn_http_server("localhost", "/fresh", fresh_response);
    let (mut engine, mut receiver) = spawn_engine(
        DESKTOP_WIDTH,
        DESKTOP_HEIGHT,
        Duration::from_secs(10),
        TrustStore::bundled_web_pki(),
    );
    let context = TopLevelContextId::new(3).unwrap();
    let stale = engine
        .navigate(context, NavigationRequest::general_web(&stale_url).unwrap())
        .expect("admit first general navigation");
    assert_eq!(
        receiver.recv().unwrap().kind(),
        EngineEventKind::NavigationStarted { navigation: stale }
    );
    stale_accepted
        .recv_timeout(SERVER_TIMEOUT)
        .expect("the stale request must be in response-head I/O");

    let fresh = engine
        .navigate(context, NavigationRequest::general_web(&fresh_url).unwrap())
        .expect("admit replacement general navigation");
    assert_ne!(stale, fresh);
    assert_eq!(
        receiver.recv().unwrap().kind(),
        EngineEventKind::NavigationCancelled { navigation: stale }
    );
    assert_eq!(
        receiver.recv().unwrap().kind(),
        EngineEventKind::NavigationStarted { navigation: fresh }
    );
    assert_eq!(
        receiver.recv().unwrap().kind(),
        EngineEventKind::NavigationCommitted {
            navigation: fresh,
            http_status: 200,
        }
    );
    let ready = receiver.recv().unwrap();
    let EngineEventKind::FrameReady {
        navigation,
        lease,
        metadata,
    } = ready.kind()
    else {
        panic!("only the replacement generation may publish a frame");
    };
    assert_eq!(navigation, fresh);
    let frame = receiver.take_frame(lease).unwrap();
    assert_eq!(frame.metadata(), metadata);
    assert_visible_desktop_frame(&frame, DESKTOP_WIDTH, DESKTOP_HEIGHT);

    fresh_server.join().expect("fresh server must finish");
    stale_server.join().expect("stale server must finish");
    assert_eq!(engine.shutdown().reason(), WorkerStopReason::Requested);
}

#[test]
fn general_fetch_absolute_deadline_remains_a_fetch_deadline_failure() {
    let (url, accepted, server) =
        spawn_stalling_http_server("/deadline", Duration::from_millis(350));
    let (mut engine, mut receiver) = spawn_engine(
        DESKTOP_WIDTH,
        DESKTOP_HEIGHT,
        Duration::from_millis(120),
        TrustStore::bundled_web_pki(),
    );
    let navigation = engine
        .navigate(
            TopLevelContextId::new(4).unwrap(),
            NavigationRequest::general_web(&url).unwrap(),
        )
        .expect("admit deadline fixture");
    assert_eq!(
        receiver.recv().unwrap().kind(),
        EngineEventKind::NavigationStarted { navigation }
    );
    accepted
        .recv_timeout(SERVER_TIMEOUT)
        .expect("deadline fixture must receive request");
    let failed = receiver.recv().expect("deadline failure event");
    let EngineEventKind::NavigationFailed {
        navigation: failed_navigation,
        failure,
    } = failed.kind()
    else {
        panic!("an absolute fetch deadline must not publish or masquerade as network failure");
    };
    assert_eq!(failed_navigation, navigation);
    assert_eq!(failure.kind(), ExecutionFailureKind::DeadlineExceeded);
    assert_eq!(failure.stage(), NavigationStage::Fetch);
    server.join().expect("deadline server must finish");
    assert_eq!(engine.shutdown().reason(), WorkerStopReason::Requested);
}

#[test]
fn capability_mismatch_and_redirects_fail_closed_without_fake_navigation_success() {
    let config = page_config(DESKTOP_WIDTH, DESKTOP_HEIGHT, Duration::from_secs(10));
    let general_web = general_web_config(config.network.clone());
    let mut direct =
        StaticPageEngine::new_general_web(config, general_web, TrustStore::bundled_web_pki())
            .expect("construct direct general-web engine");
    let mismatch = direct
        .load(
            "http://127.0.0.1:9/not-authorized",
            &CancellationSource::new().token(),
        )
        .expect_err("loopback entry point cannot consume general authority");
    assert!(matches!(
        mismatch,
        PipelineError::InvalidConfiguration {
            field: "network_capability",
            ..
        }
    ));

    let redirect_response = response(
        "302 Found",
        &[("Location", "https://example.com/final")],
        b"redirect body must not become a document",
    );
    let (redirect_url, server) = spawn_http_server("localhost", "/redirect", redirect_response);
    let redirect = direct
        .load_general_web(&redirect_url, &CancellationSource::new().token())
        .expect_err("redirect cannot be mislabeled as a final page");
    assert!(matches!(
        redirect,
        PipelineError::RedirectBlocked { status: 302 }
    ));
    server.join().expect("redirect fixture must finish");
    direct.shutdown().expect("direct engine shuts down");

    let loopback = NavigationRequest::new("http://127.0.0.1:9/").unwrap();
    let general = NavigationRequest::general_web("https://example.com/").unwrap();
    assert_eq!(
        loopback.network_capability(),
        NavigationNetworkCapability::NumericLoopback
    );
    assert_eq!(
        general.network_capability(),
        NavigationNetworkCapability::GeneralWeb
    );
}

#[test]
#[ignore = "opt-in public-network smoke; auto-margin layout support is still required"]
fn public_example_https_reaches_a_visible_desktop_frame() {
    let (mut engine, mut receiver) = spawn_engine(
        DESKTOP_WIDTH,
        DESKTOP_HEIGHT,
        Duration::from_secs(20),
        TrustStore::bundled_web_pki(),
    );
    let frame = navigate_to_visible_frame(
        &engine,
        &mut receiver,
        TopLevelContextId::new(5).unwrap(),
        "https://example.com/",
    );
    let rgba = frame
        .metadata()
        .rgba8()
        .expect("example.com frame metadata");
    assert_eq!(rgba.size().width(), DESKTOP_WIDTH);
    assert_eq!(rgba.size().height(), DESKTOP_HEIGHT);
    assert!(
        frame
            .rgba8_pixels()
            .expect("example.com pixels")
            .chunks_exact(4)
            .any(|pixel| pixel != CLEAR),
        "public HTML must visibly affect the composed frame"
    );
    assert_eq!(engine.shutdown().reason(), WorkerStopReason::Requested);
}

struct OpenSslTlsFixture {
    directory: PathBuf,
    certificate_der: Vec<u8>,
    url: String,
    child: Option<Child>,
}

impl OpenSslTlsFixture {
    fn start(document: &str) -> Self {
        let directory = unique_test_directory();
        fs::create_dir_all(&directory).expect("create external TLS fixture directory");
        let certificate_pem = directory.join("certificate.pem");
        let certificate_der = directory.join("certificate.der");
        let private_key = directory.join("private-key.pem");
        let page = directory.join("page.html");
        fs::write(&page, document).expect("write TLS fixture document");

        let certificate_output = Command::new("openssl")
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
            .expect("run openssl certificate fixture generator");
        assert_command_success("openssl req", &certificate_output);

        let der_output = Command::new("openssl")
            .args(["x509", "-in"])
            .arg(&certificate_pem)
            .args(["-outform", "DER", "-out"])
            .arg(&certificate_der)
            .output()
            .expect("run openssl DER conversion");
        assert_command_success("openssl x509", &der_output);
        let certificate_der_bytes =
            fs::read(&certificate_der).expect("read generated DER trust anchor");

        let address = reserve_fixture_address();
        let mut child = Command::new("openssl")
            .args(["s_server", "-quiet", "-accept"])
            .arg(address.to_string())
            .arg("-cert")
            .arg(&certificate_pem)
            .arg("-key")
            .arg(&private_key)
            .args(["-WWW", "-naccept", "1"])
            .current_dir(&directory)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn deterministic local TLS server");
        wait_for_listener(&mut child, address);

        Self {
            directory,
            certificate_der: certificate_der_bytes,
            url: format!("https://localhost:{}/page.html", address.port()),
            child: Some(child),
        }
    }

    fn certificate_der(&self) -> &[u8] {
        &self.certificate_der
    }

    fn url(&self) -> &str {
        &self.url
    }

    fn finish(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        let deadline = Instant::now() + SERVER_TIMEOUT;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    assert!(
                        status.success(),
                        "local openssl TLS fixture failed: {status}"
                    );
                    break;
                }
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
                Ok(None) => {
                    child.kill().expect("terminate wedged TLS fixture");
                    let _ = child.wait();
                    panic!("local openssl TLS fixture did not terminate");
                }
                Err(error) => panic!("wait for local openssl TLS fixture: {error}"),
            }
        }
    }
}

impl Drop for OpenSslTlsFixture {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        remove_fixture_file(&self.directory, "certificate.pem");
        remove_fixture_file(&self.directory, "certificate.der");
        remove_fixture_file(&self.directory, "private-key.pem");
        remove_fixture_file(&self.directory, "page.html");
        let _ = fs::remove_dir(&self.directory);
    }
}

fn assert_command_success(name: &str, output: &std::process::Output) {
    assert!(
        output.status.success(),
        "{name} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn unique_test_directory() -> PathBuf {
    let base = std::env::var_os("CARGO_TARGET_DIR").map_or_else(std::env::temp_dir, PathBuf::from);
    let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    base.join(format!(
        "wild-buzzard-general-navigation-tls-{}-{sequence}",
        std::process::id()
    ))
}

fn reserve_fixture_address() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("reserve TLS fixture port");
    listener.local_addr().expect("read reserved TLS port")
}

fn wait_for_listener(child: &mut Child, address: SocketAddr) {
    let deadline = Instant::now() + SERVER_TIMEOUT;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().expect("inspect TLS fixture startup") {
            panic!("local openssl TLS fixture exited during startup: {status}");
        }
        match TcpListener::bind(address) {
            Err(error) if error.kind() == io::ErrorKind::AddrInUse => return,
            Ok(listener) => drop(listener),
            Err(error) => panic!("inspect TLS fixture listener: {error}"),
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("local openssl TLS fixture did not bind within its deadline");
}

fn remove_fixture_file(directory: &Path, name: &str) {
    let _ = fs::remove_file(directory.join(name));
}
