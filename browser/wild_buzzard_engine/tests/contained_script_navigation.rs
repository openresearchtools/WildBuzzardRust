#![cfg(feature = "contained_inline_classic")]

use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use wild_buzzard_engine::{
    EngineEventKind, EngineEventReceiver, EngineLimits, ExecutionFailureKind,
    ExecutorShutdownStatus, FontSourcePolicy, FrameLease, NavigationEngine, NavigationRequest,
    NavigationStage, PRODUCT_SCRIPT_ADMISSION_ENABLED, StaticPageConfig, TopLevelContextId,
    WorkerStopReason,
};
use wild_buzzard_headless::HeadlessLimits;
use wild_buzzard_net::{ClientConfig, GeneralWebConfig, TrustStore};

const WIDTH: u32 = 128;
const HEIGHT: u32 = 72;
const STATIC_PANEL: [u8; 4] = [140, 20, 20, 255];
const SCRIPTED_PANEL: [u8; 4] = [20, 120, 200, 255];
const _: () = assert!(!PRODUCT_SCRIPT_ADMISSION_ENABLED);

const DOCUMENT: &str = r#"<body style="margin: 0"><div id="panel" style="display: block; width: 96px; height: 48px; background-color: rgb(140 20 20)"></div><script>
const dom = __wildBuzzardDom;
const panel = dom.lookup(4);
dom.setAttribute(panel, "style", "display: block; width: 96px; height: 48px; background-color: rgb(20 120 200)");
</script></body>"#;

fn config() -> StaticPageConfig {
    StaticPageConfig {
        viewport_width: WIDTH,
        viewport_height: HEIGHT,
        operation_timeout: Duration::from_secs(10),
        font_source: FontSourcePolicy::EmbeddedOnly,
        network: ClientConfig::default()
            .with_max_body_bytes(64 * 1024)
            .with_connect_timeout(Duration::from_secs(1))
            .with_read_timeout(Duration::from_secs(2))
            .with_write_timeout(Duration::from_secs(2)),
        headless: HeadlessLimits::default()
            .with_max_width(WIDTH)
            .with_max_height(HEIGHT)
            .with_max_pixel_bytes(WIDTH as usize * HEIGHT as usize * 4),
        ..StaticPageConfig::default()
    }
}

fn presentation_config() -> StaticPageConfig {
    StaticPageConfig {
        headless: HeadlessLimits::default()
            .with_max_width(1)
            .with_max_height(1)
            .with_max_pixel_bytes(4),
        ..config()
    }
}

fn serve_once(host_in_url: &str, body: &'static str) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("numeric loopback must bind");
    let address = listener.local_addr().unwrap();
    let url = format!("http://{host_in_url}:{}/contained.html", address.port());
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("fixture must receive one request");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        consume_request_head(&mut stream, "/contained.html");
        write_response(&mut stream, body);
    });
    (url, server)
}

type BlockedFixture = (
    String,
    mpsc::Receiver<()>,
    mpsc::SyncSender<()>,
    thread::JoinHandle<()>,
);

fn serve_once_after_release(body: &'static str) -> BlockedFixture {
    let listener = TcpListener::bind("127.0.0.1:0").expect("numeric loopback must bind");
    let address = listener.local_addr().unwrap();
    let (accepted_sender, accepted_receiver) = mpsc::sync_channel(1);
    let (release_sender, release_receiver) = mpsc::sync_channel(1);
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("fixture must receive one request");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        consume_request_head(&mut stream, "/blocked.html");
        accepted_sender.send(()).unwrap();
        release_receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("test must release the blocked response");
        write_response(&mut stream, body);
    });
    (
        format!("http://{address}/blocked.html"),
        accepted_receiver,
        release_sender,
        server,
    )
}

fn serve_stalled_once() -> BlockedFixture {
    let listener = TcpListener::bind("127.0.0.1:0").expect("numeric loopback must bind");
    let address = listener.local_addr().unwrap();
    let (accepted_sender, accepted_receiver) = mpsc::sync_channel(1);
    let (release_sender, release_receiver) = mpsc::sync_channel(1);
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("fixture must receive one request");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        consume_request_head(&mut stream, "/stalled.html");
        accepted_sender.send(()).unwrap();
        release_receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("test must release the stalled response");
    });
    (
        format!("http://{address}/stalled.html"),
        accepted_receiver,
        release_sender,
        server,
    )
}

fn consume_request_head(stream: &mut TcpStream, path: &str) {
    let mut received = Vec::new();
    let mut chunk = [0_u8; 256];
    while !received.ends_with(b"\r\n\r\n") {
        let count = stream.read(&mut chunk).expect("request head must read");
        assert!(count > 0, "request must contain a complete HTTP head");
        received.extend_from_slice(&chunk[..count]);
        assert!(received.len() <= 8 * 1024, "request head must be bounded");
    }
    assert!(received.starts_with(format!("GET {path} HTTP/1.1\r\n").as_bytes()));
}

fn write_response(stream: &mut TcpStream, body: &str) {
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).unwrap();
    stream.write_all(body.as_bytes()).unwrap();
}

fn next(receiver: &mut EngineEventReceiver) -> EngineEventKind {
    receiver
        .recv()
        .expect("worker must publish an event")
        .kind()
}

fn shutdown(engine: &mut NavigationEngine, receiver: &mut EngineEventReceiver) {
    let status = engine.shutdown();
    assert_eq!(status.reason(), WorkerStopReason::Requested);
    assert_eq!(status.executor(), ExecutorShutdownStatus::Clean);
    assert_eq!(next(receiver), EngineEventKind::ShutdownComplete { status });
}

fn receive_frame(
    engine: &NavigationEngine,
    receiver: &mut EngineEventReceiver,
    context: u64,
    request: NavigationRequest,
) -> FrameLease {
    let navigation = engine
        .navigate(TopLevelContextId::new(context).unwrap(), request)
        .unwrap();
    assert_eq!(
        next(receiver),
        EngineEventKind::NavigationStarted { navigation }
    );
    receive_ready_frame(receiver, navigation)
}

fn receive_ready_frame(
    receiver: &mut EngineEventReceiver,
    navigation: wild_buzzard_engine::NavigationId,
) -> FrameLease {
    assert_eq!(
        next(receiver),
        EngineEventKind::NavigationCommitted {
            navigation,
            http_status: 200,
        }
    );
    let lease = match next(receiver) {
        EngineEventKind::FrameReady {
            navigation: actual,
            lease,
            ..
        } => {
            assert_eq!(actual, navigation);
            lease
        }
        other => panic!("static navigation must publish one frame, got {other:?}"),
    };
    receiver.take_frame(lease).unwrap()
}

fn assert_static_red(frame: &FrameLease) {
    let pixels = frame.rgba8_pixels().expect("headless lease owns RGBA8");
    assert_eq!(&pixels[..4], STATIC_PANEL.as_slice());
    assert!(
        pixels.chunks_exact(4).all(|pixel| pixel != SCRIPTED_PANEL),
        "ordinary static parsing must not execute the inline classic script"
    );
}

fn assert_scripted_blue(frame: &FrameLease) {
    let pixels = frame.rgba8_pixels().expect("headless lease owns RGBA8");
    assert_eq!(&pixels[..4], SCRIPTED_PANEL.as_slice());
    assert!(
        pixels.chunks_exact(4).all(|pixel| pixel != STATIC_PANEL),
        "contained script publication must not expose the stale pre-script frame"
    );
}

#[test]
fn real_headless_route_retains_and_publishes_the_post_script_frame() {
    let (url, server) = serve_once("127.0.0.1", DOCUMENT);
    let (mut engine, mut receiver) =
        NavigationEngine::spawn_contained_inline_classic(config(), EngineLimits::default())
            .expect("contained headless worker must initialize");
    let frame = receive_frame(
        &engine,
        &mut receiver,
        1,
        NavigationRequest::new(&url).unwrap(),
    );
    assert_scripted_blue(&frame);
    assert!(matches!(
        receiver.try_recv(),
        Err(wild_buzzard_engine::EventReceiveError::Empty)
    ));
    server.join().unwrap();
    shutdown(&mut engine, &mut receiver);
}

#[test]
fn fetch_failure_before_owner_installation_is_reported_and_the_context_recovers() {
    let (stalled_url, accepted, release, stalled_server) = serve_stalled_once();
    let mut recovery_config = config();
    recovery_config.network = ClientConfig::default()
        .with_max_body_bytes(64 * 1024)
        .with_connect_timeout(Duration::from_millis(500))
        .with_read_timeout(Duration::from_millis(500))
        .with_write_timeout(Duration::from_millis(500));
    let (mut engine, mut receiver) =
        NavigationEngine::spawn_contained_inline_classic(recovery_config, EngineLimits::default())
            .expect("contained headless worker must initialize");
    let failed = engine
        .navigate(
            TopLevelContextId::new(1).unwrap(),
            NavigationRequest::new(&stalled_url).unwrap(),
        )
        .unwrap();
    assert_eq!(
        next(&mut receiver),
        EngineEventKind::NavigationStarted { navigation: failed }
    );
    accepted
        .recv_timeout(Duration::from_secs(5))
        .expect("stalled fixture must receive the request");
    match next(&mut receiver) {
        EngineEventKind::NavigationFailed {
            navigation,
            failure,
        } => {
            assert_eq!(navigation, failed);
            assert_eq!(failure.kind(), ExecutionFailureKind::Network);
            assert_eq!(failure.stage(), NavigationStage::Fetch);
        }
        other => panic!("stalled fetch must publish a network failure, got {other:?}"),
    }
    release.send(()).unwrap();
    stalled_server.join().unwrap();

    let (valid_url, valid_server) = serve_once("127.0.0.1", DOCUMENT);
    let frame = receive_frame(
        &engine,
        &mut receiver,
        1,
        NavigationRequest::new(&valid_url).unwrap(),
    );
    assert_scripted_blue(&frame);
    valid_server.join().unwrap();
    shutdown(&mut engine, &mut receiver);
}

#[test]
fn presentation_route_constructs_no_headless_renderer_and_general_web_is_rejected() {
    let (url, server) = serve_once("127.0.0.1", DOCUMENT);
    let (mut engine, mut receiver) =
        NavigationEngine::spawn_contained_inline_classic_for_presentation(
            presentation_config(),
            EngineLimits::default(),
        )
        .expect("presentation mode must ignore deliberately unusable headless limits");
    let frame = receive_frame(
        &engine,
        &mut receiver,
        1,
        NavigationRequest::new(&url).unwrap(),
    );
    assert!(frame.metadata().presentation().is_some());
    let scene = frame
        .into_presentation()
        .expect("contained presentation navigation must transfer its exact scene");
    assert_eq!(
        scene.metadata().document_version(),
        scene.compiled().document_version()
    );
    server.join().unwrap();

    let untouched = TcpListener::bind("127.0.0.1:0").unwrap();
    untouched.set_nonblocking(true).unwrap();
    let forbidden = format!(
        "http://localhost:{}/general.html",
        untouched.local_addr().unwrap().port()
    );
    let rejected = engine
        .navigate(
            TopLevelContextId::new(2).unwrap(),
            NavigationRequest::general_web(&forbidden).unwrap(),
        )
        .unwrap();
    assert_eq!(
        next(&mut receiver),
        EngineEventKind::NavigationStarted {
            navigation: rejected
        }
    );
    match next(&mut receiver) {
        EngineEventKind::NavigationFailed {
            navigation,
            failure,
        } => {
            assert_eq!(navigation, rejected);
            assert_eq!(failure.kind(), ExecutionFailureKind::Rejected);
            assert_eq!(failure.stage(), NavigationStage::Fetch);
        }
        other => panic!("general-web request must fail before fetch, got {other:?}"),
    }
    assert!(matches!(
        untouched.accept(),
        Err(error) if error.kind() == ErrorKind::WouldBlock
    ));
    shutdown(&mut engine, &mut receiver);
}

#[test]
fn cancellation_before_worker_execution_uses_the_paired_source_and_never_fetches() {
    let (blocking_url, accepted, release, server) = serve_once_after_release(DOCUMENT);
    let untouched = TcpListener::bind("127.0.0.1:0").unwrap();
    untouched.set_nonblocking(true).unwrap();
    let untouched_url = format!(
        "http://{}/must-not-connect.html",
        untouched.local_addr().unwrap()
    );

    let (mut engine, mut receiver) =
        NavigationEngine::spawn_contained_inline_classic(config(), EngineLimits::default())
            .expect("contained headless worker must initialize");
    let first = engine
        .navigate(
            TopLevelContextId::new(1).unwrap(),
            NavigationRequest::new(&blocking_url).unwrap(),
        )
        .unwrap();
    assert_eq!(
        next(&mut receiver),
        EngineEventKind::NavigationStarted { navigation: first }
    );
    accepted
        .recv_timeout(Duration::from_secs(5))
        .expect("first navigation must hold the worker");

    let cancelled = engine
        .navigate(
            TopLevelContextId::new(2).unwrap(),
            NavigationRequest::new(&untouched_url).unwrap(),
        )
        .unwrap();
    engine.cancel_navigation(cancelled).unwrap();
    release.send(()).unwrap();

    let first_frame = receive_ready_frame(&mut receiver, first);
    assert_scripted_blue(&first_frame);
    assert_eq!(
        next(&mut receiver),
        EngineEventKind::NavigationCancelled {
            navigation: cancelled
        }
    );
    assert!(matches!(
        untouched.accept(),
        Err(error) if error.kind() == ErrorKind::WouldBlock
    ));
    server.join().unwrap();
    shutdown(&mut engine, &mut receiver);
}

#[test]
fn ordinary_numeric_and_general_web_constructors_remain_static() {
    let (numeric_url, numeric_server) = serve_once("127.0.0.1", DOCUMENT);
    let (mut numeric, mut numeric_receiver) =
        NavigationEngine::spawn(config(), EngineLimits::default()).unwrap();
    let numeric_frame = receive_frame(
        &numeric,
        &mut numeric_receiver,
        1,
        NavigationRequest::new(&numeric_url).unwrap(),
    );
    assert_static_red(&numeric_frame);
    numeric_server.join().unwrap();
    shutdown(&mut numeric, &mut numeric_receiver);

    let (general_url, general_server) = serve_once("localhost", DOCUMENT);
    let general_config = config();
    let general_web = GeneralWebConfig::default()
        .with_http_config(general_config.network.clone())
        .with_dns_timeout(Duration::from_secs(2))
        .with_tls_handshake_timeout(Duration::from_secs(2));
    let (mut general, mut general_receiver) = NavigationEngine::spawn_general_web(
        general_config,
        general_web,
        TrustStore::bundled_web_pki(),
        EngineLimits::default(),
    )
    .unwrap();
    let general_frame = receive_frame(
        &general,
        &mut general_receiver,
        1,
        NavigationRequest::general_web(&general_url).unwrap(),
    );
    assert_static_red(&general_frame);
    general_server.join().unwrap();
    shutdown(&mut general, &mut general_receiver);
}
