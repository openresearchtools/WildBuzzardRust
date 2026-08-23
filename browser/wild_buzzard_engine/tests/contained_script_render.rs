#![cfg(feature = "contained_inline_classic")]

use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

use wild_buzzard_engine::{
    FontSourcePolicy, PipelineError, PipelineStage, ScriptLoopCancellationSource, StaticPageConfig,
    StaticPageEngine,
};
use wild_buzzard_script::{PRODUCT_SCRIPT_ADMISSION_ENABLED, ScriptDisposition};

const WIDTH: u32 = 128;
const HEIGHT: u32 = 72;
const STATIC_PANEL: [u8; 4] = [140, 20, 20, 255];
const SCRIPTED_PANEL: [u8; 4] = [20, 120, 200, 255];

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
        network: wild_buzzard_net::ClientConfig::default()
            .with_max_body_bytes(64 * 1024)
            .with_connect_timeout(Duration::from_secs(1))
            .with_read_timeout(Duration::from_secs(2))
            .with_write_timeout(Duration::from_secs(2)),
        headless: wild_buzzard_headless::HeadlessLimits::default()
            .with_max_width(WIDTH)
            .with_max_height(HEIGHT)
            .with_max_pixel_bytes(WIDTH as usize * HEIGHT as usize * 4),
        ..StaticPageConfig::default()
    }
}

fn serve_once(body: &'static str) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("numeric loopback must bind");
    let address = listener.local_addr().unwrap();
    listener
        .set_nonblocking(true)
        .expect("the fixture accept must be bounded");
    let server = thread::spawn(move || {
        let mut stream = accept_before(&listener, Instant::now() + Duration::from_secs(5));
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        consume_request_head(&mut stream, "/contained.html");
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes()).unwrap();
        stream.write_all(body.as_bytes()).unwrap();
    });
    (format!("http://{address}/contained.html"), server)
}

fn accept_before(listener: &TcpListener, deadline: Instant) -> TcpStream {
    loop {
        match listener.accept() {
            Ok((stream, _)) => return stream,
            Err(error) if error.kind() == ErrorKind::WouldBlock && Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(1));
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                panic!("page load did not connect before the fixture deadline");
            }
            Err(error) => panic!("fixture accept failed: {error}"),
        }
    }
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

#[test]
fn contained_script_mutation_reaches_the_real_rgba_frame_and_pre_cancel_never_connects() {
    let product_script_admission = std::hint::black_box(PRODUCT_SCRIPT_ADMISSION_ENABLED);
    assert!(
        !product_script_admission,
        "this bounded loopback proof must not enable general-web product script admission"
    );

    let mut engine =
        StaticPageEngine::new(config()).expect("the Linux EGL pbuffer must initialize");

    let untouched_listener =
        TcpListener::bind("127.0.0.1:0").expect("pre-cancel fixture must bind");
    untouched_listener
        .set_nonblocking(true)
        .expect("the zero-request check must not block");
    let untouched_url = format!(
        "http://{}/must-not-connect.html",
        untouched_listener.local_addr().unwrap()
    );
    let cancelled = ScriptLoopCancellationSource::new();
    assert!(cancelled.cancel());
    assert!(matches!(
        engine.load_contained_inline_classic(&untouched_url, &cancelled.token()),
        Err(PipelineError::Cancelled {
            stage: PipelineStage::Fetch
        })
    ));
    assert!(matches!(
        untouched_listener.accept(),
        Err(error) if error.kind() == ErrorKind::WouldBlock
    ));
    drop(untouched_listener);

    let (url, server) = serve_once(DOCUMENT);
    let cancellation = ScriptLoopCancellationSource::new();
    let result = engine.load_contained_inline_classic_with_deadline(
        &url,
        &cancellation.token(),
        Instant::now() + Duration::from_secs(10),
    );
    server.join().unwrap();
    let rendered = result.expect("the contained parser/script/render slice must succeed");

    assert_eq!(rendered.evidence.http_status, 200);
    assert_eq!(rendered.evidence.source_bytes, DOCUMENT.len());
    assert_eq!(rendered.script.input_bytes(), DOCUMENT.len());
    assert_eq!(rendered.script.boundaries().len(), 1);
    let boundary = &rendered.script.boundaries()[0];
    assert!(matches!(
        boundary.disposition(),
        ScriptDisposition::Success(_)
    ));
    assert_ne!(
        boundary.parser_version(),
        boundary.completed_version(),
        "the generic setAttribute host call must commit a DOM revision"
    );
    assert_eq!(
        boundary.completed_version(),
        rendered.script.final_version()
    );
    assert_eq!(
        rendered.evidence.document_version,
        rendered.script.final_version()
    );
    assert_eq!(
        rendered.frame.document_version(),
        rendered.script.final_version()
    );

    assert_eq!(rendered.frame.size().width(), WIDTH);
    assert_eq!(rendered.frame.size().height(), HEIGHT);
    assert_eq!(rendered.frame.pending_text_runs(), 0);
    assert_eq!(rendered.frame.pixel(0, 0), Some(SCRIPTED_PANEL));
    assert_ne!(rendered.frame.pixel(0, 0), Some(STATIC_PANEL));
    assert!(
        rendered
            .frame
            .pixels()
            .chunks_exact(4)
            .all(|pixel| pixel != STATIC_PANEL),
        "the final WebRender frame must not contain the pre-script panel color"
    );

    engine.shutdown().expect("engine must shut down cleanly");
}
