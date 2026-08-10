use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use wild_buzzard_dom::bindings::{ScriptMutationBatch, ScriptMutationCommand, ScriptNode};
use wild_buzzard_engine::{
    CancellationSource, ContentTypeInput, DocumentPolicyError, DocumentPolicyField,
    DocumentPolicyLimit, FontSourcePolicy, MAX_CSP_FIELD_BYTES, MAX_ENFORCING_CSP_FIELDS,
    MalformedContentType, PipelineError, PipelineStage, ReferrerPolicyInput, StaticPageConfig,
    StaticPageEngine, TrustStore,
};
use wild_buzzard_headless::HeadlessLimits;
use wild_buzzard_net::{ClientConfig, GeneralWebConfig};

const SERVER_TIMEOUT: Duration = Duration::from_secs(3);
const PAGE: &[u8] = br#"<!doctype html>
<style>
  html, body { margin: 0; background: rgb(19 45 71); }
  #panel { display: block; width: 720px; height: 240px;
    background: rgb(226 237 247); color: rgb(20 45 74); }
</style><main id="panel">Wild Buzzard document policy envelope</main>"#;

fn http_config() -> ClientConfig {
    ClientConfig::default()
        .with_max_body_bytes(512 * 1024)
        .with_connect_timeout(Duration::from_secs(1))
        .with_read_timeout(Duration::from_secs(2))
        .with_write_timeout(Duration::from_secs(2))
}

fn config(width: u32, height: u32) -> StaticPageConfig {
    let pixel_bytes = usize::try_from(width)
        .unwrap()
        .checked_mul(usize::try_from(height).unwrap())
        .and_then(|pixels| pixels.checked_mul(4))
        .unwrap();
    StaticPageConfig {
        viewport_width: width,
        viewport_height: height,
        operation_timeout: Duration::from_secs(20),
        network: http_config(),
        font_source: FontSourcePolicy::EmbeddedOnly,
        headless: HeadlessLimits::default()
            .with_max_width(width)
            .with_max_height(height)
            .with_max_pixel_bytes(pixel_bytes),
        ..StaticPageConfig::default()
    }
}

fn response(status: &str, fields: &[(&str, &[u8])], body: &[u8]) -> Vec<u8> {
    let mut response = format!("HTTP/1.1 {status}\r\n").into_bytes();
    for (name, value) in fields {
        response.extend_from_slice(name.as_bytes());
        response.extend_from_slice(b": ");
        response.extend_from_slice(value);
        response.extend_from_slice(b"\r\n");
    }
    response.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes());
    response.extend_from_slice(b"Connection: close\r\n\r\n");
    response.extend_from_slice(body);
    response
}

fn read_request_head(stream: &mut TcpStream) -> Vec<u8> {
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        assert!(
            head.len() < 64 * 1024,
            "fixture request head exceeded bound"
        );
        stream.read_exact(&mut byte).unwrap();
        head.push(byte[0]);
    }
    head
}

fn serve_script(steps: Vec<(&'static str, Vec<u8>)>) -> (String, JoinHandle<Vec<Vec<u8>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        let mut requests = Vec::new();
        for (expected_target, response) in steps {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(SERVER_TIMEOUT)).unwrap();
            stream.set_write_timeout(Some(SERVER_TIMEOUT)).unwrap();
            let request = read_request_head(&mut stream);
            assert!(
                request.starts_with(format!("GET {expected_target} HTTP/1.1\r\n").as_bytes()),
                "unexpected fixture request: {}",
                String::from_utf8_lossy(&request)
            );
            stream.write_all(&response).unwrap();
            requests.push(request);
        }
        requests
    });
    (format!("http://{address}"), handle)
}

fn serve_once(fields: &[(&str, &[u8])]) -> (String, JoinHandle<Vec<Vec<u8>>>) {
    serve_script(vec![("/page", response("200 OK", fields, PAGE))])
}

#[test]
fn exact_final_response_metadata_preserves_duplicates_and_redacts_secrets() {
    let redirect = response(
        "302 Found",
        &[
            ("Location", b"/final"),
            (
                "Content-Security-Policy",
                b"default-src https://redirect.invalid",
            ),
            ("Set-Cookie", b"redirect-secret=must-not-survive"),
        ],
        b"ignored redirect body",
    );
    let final_response = response(
        "200 OK",
        &[
            ("CONTENT-SECURITY-POLICY", b"default-src 'self'"),
            ("content-security-policy", b"img-src https:"),
            (
                "Content-Security-Policy-Report-Only",
                b"script-src 'none'; report-uri https://reports.invalid/private",
            ),
            ("REFERRER-POLICY", b"unknown, ORIGIN"),
            ("referrer-policy", b"strict-origin-when-cross-origin"),
            ("Content-Type", b"Text/HTML; Charset=\"UTF-8\""),
            ("content-type", b"application/xhtml+xml; charset=iso-8859-1"),
            ("Set-Cookie", b"session=top-secret; HttpOnly"),
            ("set-cookie", b"preference=private; Secure"),
        ],
        PAGE,
    );
    let (origin, server) = serve_script(vec![("/start", redirect), ("/final", final_response)]);
    let mut engine = StaticPageEngine::new_general_web_for_presentation(
        config(640, 360),
        GeneralWebConfig::default().with_http_config(http_config()),
        TrustStore::bundled_web_pki(),
    )
    .unwrap();
    let start = format!("{origin}/start#section");
    let rendered = engine
        .load_general_web_for_presentation(&start, &CancellationSource::new().token())
        .unwrap();
    assert_eq!(server.join().unwrap().len(), 2);

    let live = engine.live_document().unwrap();
    let metadata = live.captured_response_metadata();
    assert_eq!(
        metadata.response_document_version(),
        rendered.evidence.document_version
    );
    assert_eq!(
        metadata.navigation_commit(),
        &rendered.evidence.navigation_commit
    );
    assert_eq!(
        metadata.navigation_commit().final_url(),
        format!("{origin}/final#section")
    );
    assert_eq!(metadata.navigation_commit().redirect_count(), 1);
    assert_eq!(metadata.enforcing_csp_fields().len(), 2);
    assert_eq!(
        metadata.enforcing_csp_fields()[0].as_bytes(),
        b"default-src 'self'"
    );
    assert_eq!(
        metadata.enforcing_csp_fields()[1].as_bytes(),
        b"img-src https:"
    );
    assert_eq!(metadata.report_only_csp_fields().len(), 1);
    assert_eq!(
        metadata.referrer_policy().recognized_inputs(),
        [
            ReferrerPolicyInput::Origin,
            ReferrerPolicyInput::StrictOriginWhenCrossOrigin,
        ]
    );
    assert_eq!(metadata.referrer_policy().ignored_token_count(), 1);
    assert_eq!(
        metadata.referrer_policy().last_recognized_input(),
        Some(ReferrerPolicyInput::StrictOriginWhenCrossOrigin)
    );
    assert_eq!(metadata.content_type_fields().len(), 2);
    let ContentTypeInput::Parsed(first_type) = &metadata.content_type_fields()[0] else {
        panic!("first Content-Type must parse");
    };
    assert_eq!(first_type.media_type(), "text/html");
    assert_eq!(first_type.charsets().collect::<Vec<_>>(), ["utf-8"]);
    let cookies = metadata.set_cookie();
    assert!(cookies.was_present());
    assert_eq!(cookies.field_count(), 2);
    assert_eq!(
        cookies.value_bytes(),
        b"session=top-secret; HttpOnly".len() + b"preference=private; Secure".len()
    );

    let debug = format!("{metadata:?}");
    assert!(!debug.contains("top-secret"));
    assert!(!debug.contains("private"));
    assert!(!debug.contains("reports.invalid"));
    assert!(!debug.contains("redirect-secret"));
    engine.shutdown().unwrap();
}

#[test]
fn malformed_content_type_is_classified_without_changing_document_rendering() {
    let (url, server) = serve_once(&[("cOnTeNt-TyPe", b"not a valid media type")]);
    let mut engine = StaticPageEngine::new_for_presentation(config(640, 360)).unwrap();
    let rendered = engine
        .load_for_presentation(&format!("{url}/page"), &CancellationSource::new().token())
        .unwrap();
    server.join().unwrap();
    let metadata = engine.live_document().unwrap().captured_response_metadata();
    assert_eq!(
        metadata.response_document_version(),
        rendered.evidence.document_version
    );
    assert_eq!(
        metadata.content_type_fields(),
        [ContentTypeInput::Malformed(
            MalformedContentType::InvalidMediaType
        )]
    );
    engine.shutdown().unwrap();
}

#[test]
fn policy_limits_reject_before_body_parse_and_do_not_replace_the_live_document() {
    let (initial_url, initial_server) = serve_once(&[("Content-Type", b"text/html")]);
    let mut engine = StaticPageEngine::new_for_presentation(config(640, 360)).unwrap();
    let initial = engine
        .load_for_presentation(
            &format!("{initial_url}/page"),
            &CancellationSource::new().token(),
        )
        .unwrap();
    initial_server.join().unwrap();

    let oversized = vec![b'a'; MAX_CSP_FIELD_BYTES + 1];
    let (oversized_url, oversized_server) = serve_once(&[("Content-Security-Policy", &oversized)]);
    let oversized_error = engine
        .load_for_presentation(
            &format!("{oversized_url}/page"),
            &CancellationSource::new().token(),
        )
        .unwrap_err();
    oversized_server.join().unwrap();
    assert!(matches!(
        oversized_error,
        PipelineError::DocumentPolicy(DocumentPolicyError::LimitExceeded {
            field: DocumentPolicyField::EnforcingContentSecurityPolicy,
            limit: DocumentPolicyLimit::FieldBytes,
            actual,
            maximum: MAX_CSP_FIELD_BYTES,
        }) if actual == MAX_CSP_FIELD_BYTES + 1
    ));
    assert_eq!(
        engine.live_document().unwrap().live_version(),
        initial.evidence.document_version
    );

    let fields = (0..=MAX_ENFORCING_CSP_FIELDS)
        .map(|_| ("Content-Security-Policy", b"default-src 'self'".as_slice()))
        .collect::<Vec<_>>();
    let (count_url, count_server) = serve_once(&fields);
    let count_error = engine
        .load_for_presentation(
            &format!("{count_url}/page"),
            &CancellationSource::new().token(),
        )
        .unwrap_err();
    count_server.join().unwrap();
    assert!(matches!(
        count_error,
        PipelineError::DocumentPolicy(DocumentPolicyError::LimitExceeded {
            field: DocumentPolicyField::EnforcingContentSecurityPolicy,
            limit: DocumentPolicyLimit::FieldCount,
            actual,
            maximum: MAX_ENFORCING_CSP_FIELDS,
        }) if actual == MAX_ENFORCING_CSP_FIELDS + 1
    ));
    assert_eq!(
        engine.live_document().unwrap().live_version(),
        initial.evidence.document_version
    );
    engine.shutdown().unwrap();
}

#[test]
fn cancellation_then_deadline_precede_target_and_policy_processing() {
    let mut engine = StaticPageEngine::new_for_presentation(config(320, 180)).unwrap();
    let cancellation = CancellationSource::new();
    assert!(cancellation.cancel());
    let elapsed = Instant::now().checked_sub(Duration::from_secs(1)).unwrap();
    let cancelled = engine
        .load_for_presentation_with_deadline(
            "not a URL and never fetched",
            &cancellation.token(),
            elapsed,
        )
        .unwrap_err();
    assert!(matches!(
        cancelled,
        PipelineError::Cancelled {
            stage: PipelineStage::Fetch
        }
    ));

    let expired = engine
        .load_for_presentation_with_deadline(
            "not a URL and never fetched",
            &CancellationSource::new().token(),
            elapsed,
        )
        .unwrap_err();
    assert!(matches!(
        expired,
        PipelineError::DeadlineExceeded {
            stage: PipelineStage::Fetch
        }
    ));
    engine.shutdown().unwrap();
}

#[test]
fn captured_metadata_survives_mutation_and_rerender_with_initial_binding() {
    let fields = [
        ("Content-Security-Policy", b"default-src 'self'".as_slice()),
        ("Set-Cookie", b"never-retained=secret".as_slice()),
        ("Content-Type", b"text/html; charset=utf-8".as_slice()),
    ];
    let (url, server) = serve_once(&fields);
    let mut engine = StaticPageEngine::new(config(640, 360)).unwrap();
    let initial = engine
        .load(&format!("{url}/page"), &CancellationSource::new().token())
        .unwrap();
    server.join().unwrap();
    let response_version = initial.evidence.document_version;
    let final_url = initial.evidence.navigation_commit.final_url().to_owned();
    let panel = engine
        .live_document()
        .unwrap()
        .element_by_id("panel")
        .unwrap()
        .unwrap();
    let update = engine
        .apply_and_render(
            ScriptMutationBatch::new(
                response_version,
                vec![ScriptMutationCommand::RemoveHtmlAttribute {
                    element: ScriptNode::Existing(panel),
                    local_name: "data-never-present".into(),
                }],
            ),
            &CancellationSource::new().token(),
        )
        .unwrap();
    assert_ne!(update.evidence.document_version, response_version);
    {
        let live = engine.live_document().unwrap();
        let metadata = live.captured_response_metadata();
        assert_eq!(metadata.response_document_version(), response_version);
        assert_eq!(metadata.navigation_commit().final_url(), final_url);
        assert_eq!(metadata.enforcing_csp_fields().len(), 1);
        assert_eq!(metadata.set_cookie().field_count(), 1);
        assert_eq!(
            live.live_version().document_id(),
            response_version.document_id()
        );
    }

    let rerendered = engine
        .rerender_live(
            update.evidence.document_version,
            &CancellationSource::new().token(),
        )
        .unwrap();
    assert_eq!(
        rerendered.evidence.document_version,
        update.evidence.document_version
    );
    let metadata = engine.live_document().unwrap().captured_response_metadata();
    assert_eq!(metadata.response_document_version(), response_version);
    assert_eq!(metadata.navigation_commit().final_url(), final_url);
    assert!(!format!("{metadata:?}").contains("never-retained"));
    engine.shutdown().unwrap();
}

#[test]
fn observed_policy_headers_do_not_change_desktop_frame_output() {
    for (width, height) in [(1366, 768), (1920, 1080)] {
        let mut engine = StaticPageEngine::new(config(width, height)).unwrap();
        let (plain_url, plain_server) =
            serve_once(&[("Content-Type", b"text/html; charset=utf-8")]);
        let plain = engine
            .load(
                &format!("{plain_url}/page"),
                &CancellationSource::new().token(),
            )
            .unwrap();
        plain_server.join().unwrap();

        let (policy_url, policy_server) = serve_once(&[
            ("Content-Type", b"text/html; charset=utf-8"),
            ("Content-Security-Policy", b"default-src 'none'"),
            (
                "Content-Security-Policy-Report-Only",
                b"style-src 'none'; report-uri https://reports.invalid/private",
            ),
            ("Referrer-Policy", b"no-referrer"),
            ("Set-Cookie", b"frame-secret=not-retained"),
        ]);
        let policy = engine
            .load(
                &format!("{policy_url}/page"),
                &CancellationSource::new().token(),
            )
            .unwrap();
        policy_server.join().unwrap();

        assert_eq!(plain.frame.size().width(), width);
        assert_eq!(plain.frame.size().height(), height);
        assert_eq!(policy.frame.size(), plain.frame.size());
        assert_eq!(policy.frame.pixels(), plain.frame.pixels());
        assert!(
            plain
                .frame
                .pixels()
                .chunks_exact(4)
                .any(|pixel| pixel != [255, 255, 255, 255])
        );
        engine.shutdown().unwrap();
    }
}
