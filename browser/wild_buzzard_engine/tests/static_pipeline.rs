use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

use wild_buzzard_engine::{
    CancellationSource, EngineEventKind, EngineLimits, EventReceiveError, ExecutorShutdownStatus,
    FontSourcePolicy, NavigationEngine, NavigationRequest, PipelineError, PipelineEvidence,
    PipelineStage, RenderedStaticPage, StaticPageConfig, StaticPageEngine, TopLevelContextId,
    WorkerStopReason,
};

const WIDTH: u32 = 192;
const HEIGHT: u32 = 96;
const CLEAR: [u8; 4] = [255, 255, 255, 255];
const PANEL: [u8; 4] = [18, 52, 86, 255];
const PANEL_TEXT: [u8; 4] = [240, 220, 30, 255];
const BADGE: [u8; 4] = [110, 20, 30, 255];

const DOCUMENT: &str = r#"<!doctype html>
<style>
  html, body { margin: 0; }
  #panel, #badge { display: block; }
  #panel {
    width: 120px;
    height: 32px;
    padding: 4px;
    background-color: rgb(18 52 86);
    color: rgb(240 220 30);
    font-size: 16px;
    line-height: 28px;
  }
  #badge {
    width: 72px;
    height: 24px;
    margin-top: 5px;
    margin-left: 18px;
    padding: 3px;
    background-color: rgb(110 20 30);
    color: rgb(20 230 180);
    font-size: 13px;
    line-height: 22px;
  }
</style>
<div id="panel">Wild Buzzard</div><div id="badge">Rust Engine</div>"#;

const NO_TEXT_DOCUMENT: &str = r#"<!doctype html>
<style>html, body { margin: 0; } #empty {
  display: block; width: 32px; height: 16px;
  background-color: rgb(120 40 10);
}</style><div id="empty"></div>"#;

const WHITESPACE_DOCUMENT: &str = r#"<!doctype html>
<style>html, body { margin: 0; } #space { white-space: pre; }</style>
<div id="space">   </div>"#;

const EMPTY_PANEL: [u8; 4] = [120, 40, 10, 255];

fn config() -> StaticPageConfig {
    StaticPageConfig {
        viewport_width: WIDTH,
        viewport_height: HEIGHT,
        operation_timeout: Duration::from_secs(15),
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

fn engine() -> StaticPageEngine {
    StaticPageEngine::new(config()).expect("host must provide a Linux EGL pbuffer")
}

fn serve_once(body: &'static str) -> (String, thread::JoinHandle<()>) {
    serve_response("200 OK", body.as_bytes())
}

fn serve_response(status: &'static str, body: &'static [u8]) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("numeric loopback must bind");
    let address = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("client must connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        consume_request_head(&mut stream);
        let head = format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes()).unwrap();
        stream.write_all(body).unwrap();
    });
    (format!("http://{address}/index.html"), handle)
}

fn consume_request_head(stream: &mut TcpStream) {
    let mut received = Vec::new();
    let mut chunk = [0_u8; 256];
    while !received.ends_with(b"\r\n\r\n") {
        let count = stream.read(&mut chunk).expect("request head must read");
        assert!(count > 0, "request must end with a complete head");
        received.extend_from_slice(&chunk[..count]);
        assert!(
            received.len() <= 8 * 1024,
            "request head must remain bounded"
        );
    }
    assert!(received.starts_with(b"GET /index.html HTTP/1.1\r\n"));
}

fn load_page(engine: &mut StaticPageEngine, document: &'static str) -> RenderedStaticPage {
    let (url, server) = serve_once(document);
    let result = engine
        .load(&url, &CancellationSource::new().token())
        .expect("the concrete static-page pipeline must succeed");
    server.join().unwrap();
    let live = engine
        .live_document()
        .expect("a successful load must retain its exact mutable document");
    assert_eq!(live.live_version(), result.evidence.document_version);
    assert_eq!(
        live.last_returned_frame_version(),
        result.evidence.document_version
    );
    result
}

fn pixel_at(pixels: &[u8], x: u32, y: u32) -> [u8; 4] {
    let offset = (y as usize * WIDTH as usize + x as usize) * 4;
    pixels[offset..offset + 4].try_into().unwrap()
}

fn assert_composed_text_page(engine: &mut StaticPageEngine) -> RenderedStaticPage {
    let result = load_page(engine, DOCUMENT);
    assert_eq!(result.evidence.http_status, 200);
    assert_eq!(
        result.evidence.document_version,
        result.frame.document_version()
    );
    assert!(result.evidence.document_version.revision() > 0);
    assert_eq!(result.evidence.source_bytes, DOCUMENT.len());
    assert!(result.evidence.dom_nodes >= 8);
    assert!(result.evidence.stylo_style_entries >= 3);
    assert!(result.evidence.layout_boxes >= 3);
    assert!(result.evidence.scene_items >= 4);
    assert!(result.evidence.pre_composition_display_list_bytes > 0);

    assert!(result.text.layout_measurement_requests > 0);
    assert_eq!(
        result.text.shaped_runs, 4,
        "the two positioned blocks must each finalize as two canonical word runs"
    );
    assert!(result.text.glyphs > 0);
    assert!(result.text.clusters > 0);

    assert_eq!(result.frame.size().width(), WIDTH);
    assert_eq!(result.frame.size().height(), HEIGHT);
    assert_eq!(result.frame.pending_text_runs(), 0);
    assert!(
        result
            .frame
            .pixels()
            .chunks_exact(4)
            .any(|pixel| pixel == PANEL),
        "Stylo's panel color must reach the composed frame"
    );
    assert_eq!(result.frame.pixel(0, 0), Some(PANEL));
    assert_eq!(result.frame.pixel(127, 39), Some(PANEL));
    assert_eq!(result.frame.pixel(128, 39), Some(CLEAR));
    assert_eq!(result.frame.pixel(18, 45), Some(BADGE));
    assert_eq!(result.frame.pixel(95, 74), Some(BADGE));
    assert_eq!(result.frame.pixel(96, 74), Some(CLEAR));
    assert!(
        result.frame.pixels().chunks_exact(4).any(|pixel| {
            pixel != CLEAR.as_slice() && pixel != PANEL.as_slice() && pixel != BADGE.as_slice()
        }),
        "the canonical shaped entries must be eligible to contribute glyph pixels"
    );
    result
}

fn assert_no_text_page(
    engine: &mut StaticPageEngine,
    previous: &PipelineEvidence,
    previous_epoch: u32,
) -> (PipelineEvidence, u32) {
    let no_text = load_page(engine, NO_TEXT_DOCUMENT);
    assert_eq!(no_text.text.layout_measurement_requests, 0);
    assert_eq!(no_text.text.shaped_runs, 0);
    assert_eq!(no_text.text.glyphs, 0);
    assert_eq!(no_text.text.clusters, 0);
    assert_eq!(no_text.frame.pending_text_runs(), 0);
    assert_eq!(
        no_text.evidence.document_version,
        no_text.frame.document_version()
    );
    assert_ne!(
        no_text.evidence.document_version.document_id(),
        previous.document_version.document_id(),
        "each navigation must retain its distinct DOM identity"
    );
    assert_eq!(
        no_text.frame.epoch(),
        previous_epoch + 1,
        "the pre-submission rejections and cancellations exercised above must not publish an epoch"
    );
    assert!(
        no_text.evidence.document_version.revision() < previous.document_version.revision(),
        "a lower local revision from a new document must render without synthetic rebasing"
    );
    assert_eq!(no_text.frame.pixel(0, 0), Some(EMPTY_PANEL));
    assert_eq!(no_text.frame.pixel(31, 15), Some(EMPTY_PANEL));
    assert_eq!(no_text.frame.pixel(32, 15), Some(CLEAR));
    assert_eq!(no_text.frame.pixel(31, 16), Some(CLEAR));
    (no_text.evidence, no_text.frame.epoch())
}

fn assert_whitespace_only_page(
    engine: &mut StaticPageEngine,
    previous: &PipelineEvidence,
    previous_epoch: u32,
) {
    let whitespace = load_page(engine, WHITESPACE_DOCUMENT);
    assert!(whitespace.text.shaped_runs > 0);
    assert_eq!(whitespace.frame.pending_text_runs(), 0);
    assert_eq!(
        whitespace.evidence.document_version,
        whitespace.frame.document_version()
    );
    assert_ne!(
        whitespace.evidence.document_version.document_id(),
        previous.document_version.document_id(),
        "sequential navigations must not collapse distinct documents"
    );
    assert_eq!(whitespace.frame.epoch(), previous_epoch + 1);
    assert!(
        whitespace
            .frame
            .pixels()
            .chunks_exact(4)
            .all(|pixel| pixel == CLEAR),
        "resolved whitespace must not synthesize glyph pixels"
    );
}

fn assert_control_and_input_rejections(engine: &mut StaticPageEngine) {
    let (not_found_url, not_found_server) = serve_response("404 Not Found", b"missing");
    assert!(matches!(
        engine.load(&not_found_url, &CancellationSource::new().token()),
        Err(PipelineError::HttpStatus(404))
    ));
    not_found_server.join().unwrap();

    let (invalid_utf8_url, invalid_utf8_server) = serve_response("200 OK", b"\xff");
    assert!(matches!(
        engine.load(&invalid_utf8_url, &CancellationSource::new().token()),
        Err(PipelineError::NonUtf8Html)
    ));
    invalid_utf8_server.join().unwrap();

    let cancelled = CancellationSource::new();
    assert!(cancelled.cancel());
    assert!(matches!(
        engine.load("http://127.0.0.1:9/never", &cancelled.token()),
        Err(PipelineError::Cancelled {
            stage: PipelineStage::Fetch
        })
    ));

    let expired = Instant::now()
        .checked_sub(Duration::from_secs(1))
        .expect("one second must fit the monotonic clock range");
    assert!(matches!(
        engine.load_with_deadline(
            "http://127.0.0.1:9/never",
            &CancellationSource::new().token(),
            expired
        ),
        Err(PipelineError::DeadlineExceeded {
            stage: PipelineStage::Fetch
        })
    ));
}

fn assert_complete_shutdown(engine: StaticPageEngine) {
    let shutdown = engine.shutdown().expect("renderer must shut down cleanly");
    let renderer = shutdown
        .renderer
        .expect("headless mode constructs and shuts down its renderer");
    assert!(renderer.backend_acknowledged());
    assert!(renderer.context_released());
    assert!(renderer.wake_notifications() > 0);
    assert!(renderer.frame_ready_notifications() > 0);
    assert!(renderer.text_font_templates_released() > 0);
    assert!(renderer.text_font_instances_released() > 0);
    assert!(renderer.text_font_bytes_released() > 0);
    assert!(shutdown.text.cached_shapes_released() > 0);
    assert!(shutdown.text.accounted_cache_bytes_released() > 0);
}

#[test]
fn loopback_pages_publish_one_deterministic_zero_pending_composed_frame() {
    let mut engine = engine();
    let first = assert_composed_text_page(&mut engine);
    let first_version = first.evidence.document_version;
    let first_epoch = first.frame.epoch();
    let first_pixels = first.frame.pixels().to_vec();

    let second = assert_composed_text_page(&mut engine);
    assert_ne!(
        second.evidence.document_version.document_id(),
        first_version.document_id(),
        "repeated pixels must still come from distinct DOM identities"
    );
    assert_eq!(second.frame.epoch(), first_epoch + 1);
    assert_eq!(second.frame.pixels(), first_pixels.as_slice());

    assert_control_and_input_rejections(&mut engine);
    let (no_text_evidence, no_text_epoch) =
        assert_no_text_page(&mut engine, &second.evidence, second.frame.epoch());
    assert_whitespace_only_page(&mut engine, &no_text_evidence, no_text_epoch);
    assert_complete_shutdown(engine);
}

#[test]
fn navigation_worker_publishes_the_real_composed_frame_through_one_lease() {
    let (url, server) = serve_once(DOCUMENT);
    let (mut engine, mut receiver) = NavigationEngine::spawn(config(), EngineLimits::default())
        .expect("the worker must construct its EGL renderer on the owner thread");
    let context = TopLevelContextId::new(7).unwrap();
    let navigation = engine
        .navigate(context, NavigationRequest::new(&url).unwrap())
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
    let ready = receiver.recv().unwrap();
    let EngineEventKind::FrameReady {
        navigation: ready_navigation,
        lease,
        metadata,
    } = ready.kind()
    else {
        panic!("expected one generation-tagged composed frame");
    };
    assert_eq!(ready_navigation, navigation);
    let rgba8 = metadata.rgba8().expect("headless frame has RGBA8 metadata");
    assert_eq!(rgba8.size().width(), WIDTH);
    assert_eq!(rgba8.size().height(), HEIGHT);
    assert_eq!(rgba8.stride(), WIDTH as usize * 4);
    assert_eq!(rgba8.byte_len(), WIDTH as usize * HEIGHT as usize * 4);

    let frame = receiver.take_frame(lease).unwrap();
    assert_eq!(frame.navigation(), navigation);
    assert_eq!(frame.lease_id(), lease);
    assert_eq!(frame.metadata(), metadata);
    let pixels = frame
        .rgba8_pixels()
        .expect("headless lease retains exact RGBA8 pixels");
    assert_eq!(pixel_at(pixels, 0, 0), PANEL);
    assert!(
        (4..36).any(|y| {
            (4..124).any(|x| {
                let pixel = pixel_at(pixels, x, y);
                pixel[0] > PANEL[0]
                    && pixel[0] <= PANEL_TEXT[0]
                    && pixel[1] > PANEL[1]
                    && pixel[1] <= PANEL_TEXT[1]
                    && pixel[2] < PANEL[2]
                    && pixel[2] >= PANEL_TEXT[2]
                    && pixel[3] == u8::MAX
            })
        }),
        "the exact leased frame must contain a panel-region glyph pixel blended toward the declared text color"
    );
    assert_eq!(pixel_at(pixels, 18, 45), BADGE);
    assert_eq!(pixel_at(pixels, 128, 39), CLEAR);
    assert_eq!(receiver.try_recv(), Err(EventReceiveError::Empty));
    server.join().unwrap();

    let status = engine.shutdown();
    assert_eq!(status.reason(), WorkerStopReason::Requested);
    assert_eq!(status.executor(), ExecutorShutdownStatus::Clean);
    assert_eq!(
        receiver.recv().unwrap().kind(),
        EngineEventKind::ShutdownComplete { status }
    );
}

#[test]
fn presentation_mode_owns_the_compiled_scene_without_headless_pixels() {
    let mut presentation = StaticPageEngine::new_for_presentation(config())
        .expect("presentation mode needs no EGL pbuffer renderer");
    assert!(matches!(
        presentation.load(
            "http://127.0.0.1:9/wrong-mode",
            &CancellationSource::new().token()
        ),
        Err(PipelineError::InvalidConfiguration {
            field: "engine_output_mode",
            ..
        })
    ));

    let (url, server) = serve_once(DOCUMENT);
    let rendered = presentation
        .load_for_presentation(&url, &CancellationSource::new().token())
        .expect("the renderer-neutral pipeline succeeds");
    server.join().unwrap();
    let metadata = rendered.scene.metadata();
    assert_eq!(
        metadata.document_version(),
        rendered.evidence.document_version
    );
    assert_eq!(
        rendered.scene.compiled().document_version(),
        rendered.evidence.document_version
    );
    assert_eq!(metadata.pipeline(), rendered.scene.compiled().pipeline());
    assert_eq!(metadata.scene_items(), rendered.evidence.scene_items);
    assert_eq!(metadata.shaped_runs(), rendered.text.shaped_runs);
    assert_eq!(
        metadata.display_list_bytes(),
        rendered
            .scene
            .compiled()
            .built_display_list()
            .size_in_bytes()
    );
    assert_eq!(
        rendered.scene.shaped_text().len(),
        rendered.text.shaped_runs
    );
    assert!(metadata.revision().get() > 0);
    assert!(metadata.retained_charge_bytes() > metadata.display_list_bytes());

    let shutdown = presentation
        .shutdown()
        .expect("presentation shutdown succeeds");
    assert!(
        shutdown.renderer.is_none(),
        "presentation mode must not fabricate headless renderer teardown"
    );

    let mut headless = engine();
    assert!(matches!(
        headless.load_for_presentation(
            "http://127.0.0.1:9/wrong-mode",
            &CancellationSource::new().token()
        ),
        Err(PipelineError::InvalidConfiguration {
            field: "engine_output_mode",
            ..
        })
    ));
    assert!(
        headless
            .shutdown()
            .expect("unused headless owner still shuts down")
            .renderer
            .is_some(),
        "headless mode owns the renderer even when mode rejection precedes a frame"
    );
}

#[test]
fn invalid_configuration_fails_before_renderer_initialization() {
    let zero_timeout = StaticPageConfig {
        operation_timeout: Duration::ZERO,
        ..StaticPageConfig::default()
    };
    assert!(matches!(
        StaticPageEngine::new(zero_timeout),
        Err(PipelineError::InvalidConfiguration {
            field: "operation_timeout",
            detail: _
        })
    ));

    let zero_viewport = StaticPageConfig {
        viewport_width: 0,
        ..StaticPageConfig::default()
    };
    assert!(matches!(
        StaticPageEngine::new(zero_viewport),
        Err(PipelineError::Headless(
            wild_buzzard_headless::HeadlessError::InvalidFrameSize {
                width: 0,
                height: 600
            }
        ))
    ));
}
