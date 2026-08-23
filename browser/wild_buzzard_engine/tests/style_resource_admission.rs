use std::fs;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use wild_buzzard_engine::{
    CancellationSource, FontSourcePolicy, MAX_STYLE_RESOURCE_ATTRIBUTE_BYTES,
    MAX_STYLE_RESOURCE_CANDIDATES, MAX_STYLE_RESOURCE_DIAGNOSTICS, MAX_STYLE_RESOURCE_URL_BYTES,
    StaticPageConfig, StaticPageEngine, StyleBaseCandidateStatus, StyleResourceAttribute,
    StyleResourceCandidateStatus, StyleResourceDiagnosticKind, StyleResourceDiagnosticSubject,
    StyleResourceLimit, StyleResourcePlan, StyleResourcePlanError, StyleResourceRequestIdentity,
    TrustStore,
};
use wild_buzzard_html::parse_document;
use wild_buzzard_net::{ClientConfig, GeneralWebConfig};

const SERVER_TIMEOUT: Duration = Duration::from_secs(3);
const PLAN_BODY_LIMIT: usize = 2 * 1024 * 1024;
static NEXT_TLS_FIXTURE: AtomicU64 = AtomicU64::new(1);

fn http_config() -> ClientConfig {
    ClientConfig::default()
        .with_max_body_bytes(PLAN_BODY_LIMIT)
        .with_connect_timeout(Duration::from_secs(1))
        .with_read_timeout(Duration::from_secs(2))
        .with_write_timeout(Duration::from_secs(2))
}

fn engine_config() -> StaticPageConfig {
    StaticPageConfig {
        viewport_width: 320,
        viewport_height: 200,
        operation_timeout: Duration::from_secs(20),
        network: http_config(),
        font_source: FontSourcePolicy::EmbeddedOnly,
        ..StaticPageConfig::default()
    }
}

fn general_web_config() -> GeneralWebConfig {
    GeneralWebConfig::default()
        .with_http_config(http_config())
        .with_dns_timeout(Duration::from_secs(2))
        .with_tls_handshake_timeout(Duration::from_secs(2))
        .with_max_dns_candidates(8)
        .with_max_connection_attempts(8)
}

fn html(head: &str) -> String {
    format!("<!doctype html><html><head>{head}</head><body></body></html>")
}

fn http_response(headers: &[(&str, &str)], body: &str) -> Vec<u8> {
    let mut response = b"HTTP/1.1 200 OK\r\n".to_vec();
    for (name, value) in headers {
        response.extend_from_slice(name.as_bytes());
        response.extend_from_slice(b": ");
        response.extend_from_slice(value.as_bytes());
        response.extend_from_slice(b"\r\n");
    }
    response.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes());
    response.extend_from_slice(b"Connection: close\r\n\r\n");
    response.extend_from_slice(body.as_bytes());
    response
}

fn read_request_head(stream: &mut TcpStream) -> io::Result<Vec<u8>> {
    let mut request = Vec::new();
    let mut byte = [0_u8; 1];
    while !request.ends_with(b"\r\n\r\n") {
        if request.len() == 128 * 1024 {
            return Err(io::Error::other("fixture request head exceeded bound"));
        }
        stream.read_exact(&mut byte)?;
        request.push(byte[0]);
    }
    Ok(request)
}

struct ArmedHttpFixture {
    origin: String,
    url: String,
    done: Option<Sender<()>>,
    server: Option<JoinHandle<Vec<Vec<u8>>>>,
}

impl ArmedHttpFixture {
    fn start(path_and_fragment: &str, headers: &[(&str, &str)], body: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind HTTP fixture");
        listener
            .set_nonblocking(true)
            .expect("arm HTTP fixture listener");
        let address = listener.local_addr().expect("read fixture address");
        let origin = format!("http://{address}");
        let url = format!("{origin}{path_and_fragment}");
        let expected_target = path_and_fragment
            .split_once('#')
            .map_or(path_and_fragment, |(target, _)| target)
            .to_owned();
        let response = http_response(headers, body);
        let (done_sender, done_receiver) = mpsc::channel();
        let server = thread::spawn(move || {
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        match done_receiver.try_recv() {
                            Ok(()) | Err(TryRecvError::Disconnected) => return Vec::new(),
                            Err(TryRecvError::Empty) => {
                                thread::sleep(Duration::from_millis(2));
                            }
                        }
                    }
                    Err(error) => panic!("accept document request: {error}"),
                }
            };
            stream
                .set_read_timeout(Some(SERVER_TIMEOUT))
                .expect("set document read timeout");
            stream
                .set_write_timeout(Some(SERVER_TIMEOUT))
                .expect("set document write timeout");
            let request = read_request_head(&mut stream).expect("read document request");
            assert!(
                request.starts_with(format!("GET {expected_target} HTTP/1.1\r\n").as_bytes()),
                "unexpected document request target"
            );
            stream
                .write_all(&response)
                .expect("write document response");
            drop(stream);

            let mut requests = vec![request];
            monitor_for_forbidden_requests(&listener, &done_receiver, &mut requests);
            requests
        });
        Self {
            origin,
            url,
            done: Some(done_sender),
            server: Some(server),
        }
    }

    fn finish(mut self) {
        self.done
            .take()
            .expect("fixture completion sender")
            .send(())
            .expect("signal fixture completion");
        let requests = self
            .server
            .take()
            .expect("fixture server handle")
            .join()
            .expect("join fixture server");
        assert_eq!(
            requests.len(),
            1,
            "style-resource planning must issue zero subresource requests"
        );
    }
}

impl Drop for ArmedHttpFixture {
    fn drop(&mut self) {
        if let Some(done) = self.done.take() {
            let _ = done.send(());
        }
        if let Some(server) = self.server.take() {
            let _ = server.join();
        }
    }
}

fn monitor_for_forbidden_requests(
    listener: &TcpListener,
    done: &Receiver<()>,
    requests: &mut Vec<Vec<u8>>,
) {
    loop {
        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream
                        .set_read_timeout(Some(Duration::from_millis(100)))
                        .expect("set forbidden-request timeout");
                    requests.push(read_request_head(&mut stream).unwrap_or_default());
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("inspect forbidden stylesheet request: {error}"),
            }
        }
        match done.try_recv() {
            Ok(()) | Err(TryRecvError::Disconnected) => return,
            Err(TryRecvError::Empty) => thread::sleep(Duration::from_millis(2)),
        }
    }
}

fn with_http_document<ResultValue>(
    path_and_fragment: &str,
    headers: &[(&str, &str)],
    body: &str,
    inspect: impl FnOnce(&StaticPageEngine, &str, &str) -> ResultValue,
) -> ResultValue {
    let fixture = ArmedHttpFixture::start(path_and_fragment, headers, body);
    let mut engine = StaticPageEngine::new_general_web_for_presentation(
        engine_config(),
        general_web_config(),
        TrustStore::bundled_web_pki(),
    )
    .expect("create presentation engine");
    engine
        .load_general_web_for_presentation(&fixture.url, &CancellationSource::new().token())
        .expect("load deterministic document");
    let result = inspect(&engine, &fixture.origin, &fixture.url);
    engine.shutdown().expect("shut down fixture engine");
    fixture.finish();
    result
}

fn load_plan(
    path_and_fragment: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> (Result<StyleResourcePlan, StyleResourcePlanError>, String) {
    with_http_document(path_and_fragment, headers, body, |engine, origin, _| {
        (
            StyleResourcePlan::from_live_document(
                engine.live_document().expect("retained live document"),
            ),
            origin.to_owned(),
        )
    })
}

fn diagnostic_kinds(plan: &StyleResourcePlan) -> Vec<StyleResourceDiagnosticKind> {
    plan.diagnostics()
        .iter()
        .copied()
        .map(wild_buzzard_engine::StyleResourceDiagnostic::kind)
        .collect()
}

#[test]
fn exact_response_version_and_final_url_fallback_are_mandatory() {
    let body = html(
        r##"<link rel="stylesheet" href="sheet.css"><link rel="stylesheet" href="?theme=dark#ignored"><link rel="stylesheet" href="#only"><link rel="stylesheet" href="/root.css">"##,
    );
    with_http_document(
        "/dir/page?doc=1#section",
        &[],
        &body,
        |engine, origin, final_url| {
            let page = engine.live_document().expect("retained live document");
            let plan = StyleResourcePlan::from_live_document(page).expect("construct exact plan");
            assert_eq!(plan.document_version(), page.live_version());
            assert_eq!(
                plan.navigation_commit(),
                page.captured_response_metadata().navigation_commit()
            );
            assert_eq!(plan.fallback_base_url(), final_url);
            assert_eq!(plan.document_base_url(), final_url);
            assert_eq!(
                plan.requests()
                    .iter()
                    .map(StyleResourceRequestIdentity::canonical_url)
                    .collect::<Vec<_>>(),
                [
                    format!("{origin}/dir/sheet.css"),
                    format!("{origin}/dir/page?theme=dark"),
                    format!("{origin}/dir/page?doc=1"),
                    format!("{origin}/root.css"),
                ]
            );

            let foreign = parse_document("<!doctype html><link rel=stylesheet href=x.css>")
                .expect("parse foreign document")
                .document
                .snapshot()
                .expect("snapshot foreign document");
            assert!(matches!(
                StyleResourcePlan::from_snapshot(&foreign, page.captured_response_metadata()),
                Err(StyleResourcePlanError::DocumentVersionMismatch { .. })
            ));
        },
    );
}

#[test]
fn only_first_base_href_can_replace_the_fallback() {
    let valid = html(
        r#"<base href="/first/?q=1#base"><base href="/later/"><link rel=stylesheet href="sheet.css#request-fragment">"#,
    );
    let (valid, origin) = load_plan("/dir/page", &[], &valid);
    let valid = valid.expect("valid first base plan");
    assert_eq!(
        valid.base_candidate().expect("base evidence").status(),
        StyleBaseCandidateStatus::Selected
    );
    assert_eq!(
        valid.document_base_url(),
        format!("{origin}/first/?q=1#base")
    );
    assert_eq!(
        valid.requests()[0].canonical_url(),
        format!("{origin}/first/sheet.css")
    );

    let invalid = html(
        r#"<base href="data:text/plain,blocked"><base href="/later/"><link rel=stylesheet href=sheet.css>"#,
    );
    let (invalid, origin) = load_plan("/dir/page", &[], &invalid);
    let invalid = invalid.expect("invalid first base remains a nonfatal rejection");
    assert_eq!(
        invalid.base_candidate().expect("base evidence").status(),
        StyleBaseCandidateStatus::Rejected
    );
    assert_eq!(invalid.document_base_url(), format!("{origin}/dir/page"));
    assert_eq!(
        invalid.requests()[0].canonical_url(),
        format!("{origin}/dir/sheet.css")
    );
    assert!(diagnostic_kinds(&invalid).contains(&StyleResourceDiagnosticKind::UnsupportedScheme));

    let blocked = html(
        r#"<base href="https://blocked.example/private/"><base href="/later/"><link rel=stylesheet href=sheet.css>"#,
    );
    let (blocked, origin) = load_plan(
        "/dir/page",
        &[("Content-Security-Policy", "base-uri 'self'")],
        &blocked,
    );
    let blocked = blocked.expect("CSP-blocked base remains a nonfatal rejection");
    let evidence = blocked.base_candidate().expect("base evidence");
    assert_eq!(evidence.status(), StyleBaseCandidateStatus::Rejected);
    assert_eq!(
        evidence
            .policy_decision()
            .expect("base reached policy")
            .enforcing_blocked_policy_count(),
        1
    );
    assert_eq!(blocked.document_base_url(), format!("{origin}/dir/page"));
    assert_eq!(
        blocked.requests()[0].canonical_url(),
        format!("{origin}/dir/sheet.css")
    );
}

#[test]
fn rel_tokens_and_type_essence_follow_html_ascii_rules() {
    let vertical_tab = '\u{000b}';
    let body = html(&format!(
        "<link rel=\"&#x09;StYlEsHeEt&#x0c;\" href=/a.css>\
         <link rel=\"test{vertical_tab}stylesheet\" href=/not-a-candidate.css>\
         <link rel=\"alternate stylesheet\" href=/alternate.css>\
         <link rel=stylesheet href=/empty.css type=\"\">\
         <link rel=stylesheet href=/typed.css type=\" TEXT/CSS ; charset=utf-8\">\
         <link rel=stylesheet href=/wrong.css type=application/css>\
         <link rel=\"ſtyleſheet\" href=/unicode.css>"
    ));
    let (plan, origin) = load_plan("/page", &[], &body);
    let plan = plan.expect("token/type plan");
    assert_eq!(plan.candidates().len(), 5);
    assert_eq!(
        plan.requests()
            .iter()
            .map(StyleResourceRequestIdentity::canonical_url)
            .collect::<Vec<_>>(),
        [
            format!("{origin}/a.css"),
            format!("{origin}/empty.css"),
            format!("{origin}/typed.css"),
        ]
    );
    let kinds = diagnostic_kinds(&plan);
    assert!(kinds.contains(&StyleResourceDiagnosticKind::AlternateStylesheet));
    assert!(kinds.contains(&StyleResourceDiagnosticKind::WrongType));
}

#[test]
fn first_gate_security_attributes_reject_without_fetching() {
    let body = html(
        r#"<link rel=stylesheet href=/missing-disabled.css disabled>
           <link rel=stylesheet href=/cross.css crossorigin="">
           <link rel=stylesheet href=/empty-integrity.css integrity="">
           <link rel=stylesheet href=/integrity.css integrity="sha256-private">
           <link rel=stylesheet href=/empty-title.css title="">
           <link rel=stylesheet href=/title.css title="preferred">
           <link rel=stylesheet>
           <link rel=stylesheet href="">"#,
    );
    let (plan, origin) = load_plan("/page", &[], &body);
    let plan = plan.expect("attribute plan");
    assert_eq!(plan.candidates().len(), 8);
    assert_eq!(
        plan.requests()
            .iter()
            .map(StyleResourceRequestIdentity::canonical_url)
            .collect::<Vec<_>>(),
        [
            format!("{origin}/empty-integrity.css"),
            format!("{origin}/empty-title.css"),
        ]
    );
    let kinds = diagnostic_kinds(&plan);
    for expected in [
        StyleResourceDiagnosticKind::Disabled,
        StyleResourceDiagnosticKind::CrossOrigin,
        StyleResourceDiagnosticKind::Integrity,
        StyleResourceDiagnosticKind::Titled,
        StyleResourceDiagnosticKind::EmptyHref,
    ] {
        assert!(
            kinds.contains(&expected),
            "missing diagnostic: {expected:?}"
        );
    }
}

#[test]
fn canonical_http_identities_reject_credentials_schemes_and_malformed_urls() {
    let body = html(
        r#"<link rel=stylesheet href="same.css#drop">
           <link rel=stylesheet href="http://cross.example/a.css?x=1#drop">
           <link rel=stylesheet href="https://user:secret@private.example/a.css">
           <link rel=stylesheet href="data:text/css,body{}">
           <link rel=stylesheet href="javascript:alert(1)">
           <link rel=stylesheet href="ftp://files.example/a.css">
           <link rel=stylesheet href="http://[invalid">"#,
    );
    let (plan, origin) = load_plan("/dir/page", &[], &body);
    let plan = plan.expect("URL classification plan");
    assert_eq!(
        plan.requests()
            .iter()
            .map(StyleResourceRequestIdentity::canonical_url)
            .collect::<Vec<_>>(),
        [
            format!("{origin}/dir/same.css"),
            "http://cross.example/a.css?x=1".to_owned(),
        ]
    );
    let kinds = diagnostic_kinds(&plan);
    assert!(kinds.contains(&StyleResourceDiagnosticKind::CredentialsNotAllowed));
    assert_eq!(
        kinds
            .iter()
            .filter(|kind| **kind == StyleResourceDiagnosticKind::UnsupportedScheme)
            .count(),
        3
    );
    assert!(kinds.contains(&StyleResourceDiagnosticKind::InvalidUrl));
    let debug = format!("{plan:?}");
    assert!(!debug.contains("secret"));
    assert!(!debug.contains("private.example"));
    assert!(!debug.contains("cross.example"));
    assert!(!format!("{:?}", plan.requests()[0]).contains("same.css"));
}

#[test]
fn nonce_intersection_and_report_only_evidence_never_retain_nonce_text() {
    let nonce = "TopSecretNonceValue";
    let body = html(&format!(
        "<link rel=stylesheet href=\"http://otherwise-blocked.example/allowed.css\" nonce=\"{nonce}\">\
         <link rel=stylesheet href=\"http://otherwise-blocked.example/rejected.css\" nonce=\"wrong\">"
    ));
    let headers = [
        (
            "Content-Security-Policy",
            "style-src-elem 'nonce-TopSecretNonceValue'",
        ),
        (
            "Content-Security-Policy",
            "style-src https: 'nonce-TopSecretNonceValue'",
        ),
        ("Content-Security-Policy-Report-Only", "style-src 'none'"),
        (
            "Content-Security-Policy-Report-Only",
            "style-src http://never.example",
        ),
    ];
    let (plan, _) = load_plan("/page", &headers, &body);
    let plan = plan.expect("nonce policy plan");
    assert_eq!(plan.enforcing_policy_count(), 2);
    assert_eq!(plan.report_only_policy_count(), 2);
    assert_eq!(plan.requests().len(), 1);
    assert_eq!(
        plan.requests()[0]
            .policy_decision()
            .report_only_would_block_policy_count(),
        2
    );
    assert_eq!(plan.enforcing_policy_block_count(), 2);
    assert_eq!(plan.report_only_policy_block_count(), 4);
    assert!(matches!(
        plan.candidates()[1].status(),
        StyleResourceCandidateStatus::Rejected
    ));
    let debug = format!("{plan:?}");
    assert!(!debug.contains(nonce));
    assert!(!debug.contains("wrong"));
    assert!(!debug.contains("otherwise-blocked"));
    assert!(!format!("{:?}", plan.diagnostics()).contains(nonce));
}

#[test]
fn candidate_order_and_exact_record_cap_are_enforced() {
    let mut exact_links = String::new();
    for index in 0..MAX_STYLE_RESOURCE_CANDIDATES {
        exact_links.push_str("<link rel=stylesheet href=/sheet-");
        exact_links.push_str(&index.to_string());
        exact_links.push_str(".css>");
    }
    let (exact, origin) = load_plan("/page", &[], &html(&exact_links));
    let exact = exact.expect("exact candidate cap");
    assert_eq!(exact.candidates().len(), MAX_STYLE_RESOURCE_CANDIDATES);
    assert_eq!(exact.requests().len(), MAX_STYLE_RESOURCE_CANDIDATES);
    for (index, (candidate, request)) in exact.candidates().iter().zip(exact.requests()).enumerate()
    {
        assert_eq!(
            candidate.status(),
            StyleResourceCandidateStatus::Admitted {
                request_index: index
            }
        );
        assert_eq!(request.owner(), candidate.owner());
        assert_eq!(request.document_version(), exact.document_version());
        assert_eq!(
            request.canonical_url(),
            format!("{origin}/sheet-{index}.css")
        );
    }

    let over_links = format!("{exact_links}<link rel=stylesheet href=/overflow.css>");
    let (over, _) = load_plan("/page", &[], &html(&over_links));
    assert_eq!(
        over.expect_err("next candidate must reject the complete plan"),
        StyleResourcePlanError::LimitExceeded {
            limit: StyleResourceLimit::CandidateRecords,
            actual: MAX_STYLE_RESOURCE_CANDIDATES + 1,
            maximum: MAX_STYLE_RESOURCE_CANDIDATES,
        }
    );
}

#[test]
fn diagnostic_cap_admits_exact_edge_and_rejects_next_record() {
    let four_diagnostics = "<link rel=stylesheet href disabled crossorigin integrity=present>";
    let links = four_diagnostics.repeat(MAX_STYLE_RESOURCE_CANDIDATES);
    let (exact, _) = load_plan("/page", &[], &html(&links));
    let exact = exact.expect("exact diagnostic cap");
    assert_eq!(exact.candidates().len(), MAX_STYLE_RESOURCE_CANDIDATES);
    assert_eq!(exact.diagnostics().len(), MAX_STYLE_RESOURCE_DIAGNOSTICS);
    assert!(exact.requests().is_empty());

    let with_base_diagnostic = format!("<base href=data:text/plain,blocked>{links}");
    let (over, _) = load_plan("/page", &[], &html(&with_base_diagnostic));
    assert_eq!(
        over.expect_err("next diagnostic must reject the complete plan"),
        StyleResourcePlanError::LimitExceeded {
            limit: StyleResourceLimit::Diagnostics,
            actual: MAX_STYLE_RESOURCE_DIAGNOSTICS + 1,
            maximum: MAX_STYLE_RESOURCE_DIAGNOSTICS,
        }
    );
}

#[test]
fn attribute_cap_admits_exact_edge_and_rejects_or_fails_closed_next_unit() {
    let exact_nonce = "n".repeat(MAX_STYLE_RESOURCE_ATTRIBUTE_BYTES);
    let exact_body = html(&format!(
        "<link rel=stylesheet href=/exact.css nonce=\"{exact_nonce}\">"
    ));
    let (exact, _) = load_plan("/page", &[], &exact_body);
    let exact = exact.expect("exact attribute cap");
    assert_eq!(exact.requests().len(), 1);
    assert!(diagnostic_kinds(&exact).contains(&StyleResourceDiagnosticKind::NonceIgnoredOverLimit));

    let over_nonce = "n".repeat(MAX_STYLE_RESOURCE_ATTRIBUTE_BYTES + 1);
    let over_body = html(&format!(
        "<link rel=stylesheet href=/over.css nonce=\"{over_nonce}\">"
    ));
    let (over, _) = load_plan("/page", &[], &over_body);
    let over = over.expect("oversized candidate attribute is a typed candidate rejection");
    assert!(over.requests().is_empty());
    assert!(over.diagnostics().iter().any(|diagnostic| {
        matches!(
            diagnostic.kind(),
            StyleResourceDiagnosticKind::AttributeTooLong {
                attribute: StyleResourceAttribute::Nonce,
                actual,
                maximum: MAX_STYLE_RESOURCE_ATTRIBUTE_BYTES,
            } if actual == MAX_STYLE_RESOURCE_ATTRIBUTE_BYTES + 1
        )
    }));

    let exact_rel = format!(
        "stylesheet{}",
        " ".repeat(MAX_STYLE_RESOURCE_ATTRIBUTE_BYTES - "stylesheet".len())
    );
    let (exact_rel_plan, _) = load_plan(
        "/page",
        &[],
        &html(&format!("<link rel=\"{exact_rel}\" href=/edge.css>")),
    );
    assert_eq!(exact_rel_plan.expect("exact rel cap").requests().len(), 1);

    let over_rel = format!("{exact_rel} ");
    let (over_rel_plan, _) = load_plan(
        "/page",
        &[],
        &html(&format!("<link rel=\"{over_rel}\" href=/edge.css>")),
    );
    assert_eq!(
        over_rel_plan.expect_err("rel discovery above its cap must fail closed"),
        StyleResourcePlanError::LimitExceeded {
            limit: StyleResourceLimit::AttributeBytes,
            actual: MAX_STYLE_RESOURCE_ATTRIBUTE_BYTES + 1,
            maximum: MAX_STYLE_RESOURCE_ATTRIBUTE_BYTES,
        }
    );
}

#[test]
fn canonical_url_and_fallback_caps_cover_exact_edge_and_next_unit() {
    let seed_body = html("<link rel=stylesheet href=/placeholder.css>");
    let (_, origin) = load_plan("/page", &[], &seed_body);
    let exact_href = format!(
        "/{}",
        "u".repeat(MAX_STYLE_RESOURCE_URL_BYTES - origin.len() - 1)
    );
    let exact_body = html(&format!("<link rel=stylesheet href=\"{exact_href}\">"));
    let (exact, _) = load_plan("/page", &[], &exact_body);
    let exact = exact.expect("exact canonical request URL cap");
    assert_eq!(
        exact.requests()[0].canonical_url().len(),
        MAX_STYLE_RESOURCE_URL_BYTES
    );

    let over_href = format!("{exact_href}u");
    let over_body = html(&format!("<link rel=stylesheet href=\"{over_href}\">"));
    let (over, _) = load_plan("/page", &[], &over_body);
    let over = over.expect("oversized request URL is a per-candidate rejection");
    assert!(over.requests().is_empty());
    assert!(over.diagnostics().iter().any(|diagnostic| {
        matches!(
            diagnostic.kind(),
            StyleResourceDiagnosticKind::CanonicalUrlTooLong {
                actual,
                maximum: MAX_STYLE_RESOURCE_URL_BYTES,
            } if actual == MAX_STYLE_RESOURCE_URL_BYTES + 1
        )
    }));

    let exact_path = format!(
        "/{}",
        "f".repeat(MAX_STYLE_RESOURCE_URL_BYTES - origin.len() - 1)
    );
    let (exact_fallback, _) = load_plan(&exact_path, &[], &html(""));
    assert_eq!(
        exact_fallback
            .expect("exact fallback cap")
            .fallback_base_url()
            .len(),
        MAX_STYLE_RESOURCE_URL_BYTES
    );
}

struct OpenSslTlsFixture {
    directory: PathBuf,
    certificate_der: Vec<u8>,
    origin: String,
    child: Option<Child>,
}

impl OpenSslTlsFixture {
    fn start(headers: &[(&str, &str)], build_document: impl FnOnce(&str) -> String) -> Self {
        let directory = unique_tls_directory();
        fs::create_dir_all(&directory).expect("create external TLS fixture directory");
        let certificate_pem = directory.join("certificate.pem");
        let certificate_der = directory.join("certificate.der");
        let private_key = directory.join("private-key.pem");
        let page = directory.join("page.html");
        let address = reserve_address();
        let origin = format!("https://localhost:{}", address.port());
        let document = build_document(&origin);
        fs::write(&page, http_response(headers, &document)).expect("write TLS fixture response");

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
            .expect("generate TLS fixture certificate");
        assert_command_success("openssl req", &output);

        let output = Command::new("openssl")
            .args(["x509", "-in"])
            .arg(&certificate_pem)
            .args(["-outform", "DER", "-out"])
            .arg(&certificate_der)
            .output()
            .expect("convert TLS fixture certificate");
        assert_command_success("openssl x509", &output);

        let mut child = Command::new("openssl")
            .args([
                "s_server", "-quiet", "-HTTP", "-tls1_3", "-alpn", "http/1.1", "-accept",
            ])
            .arg(address.to_string())
            .arg("-cert")
            .arg(&certificate_pem)
            .arg("-key")
            .arg(&private_key)
            .args(["-naccept", "1"])
            .current_dir(&directory)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn TLS fixture");
        wait_for_listener(&mut child, address);

        Self {
            directory,
            certificate_der: fs::read(certificate_der).expect("read DER certificate"),
            origin,
            child: Some(child),
        }
    }

    fn finish(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        let deadline = Instant::now() + SERVER_TIMEOUT;
        loop {
            match child.try_wait().expect("inspect TLS fixture") {
                Some(status) => {
                    assert!(status.success(), "TLS fixture failed: {status}");
                    return;
                }
                None if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
                None => {
                    child.kill().expect("terminate TLS fixture");
                    let _ = child.wait();
                    panic!("TLS fixture did not terminate");
                }
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

fn unique_tls_directory() -> PathBuf {
    let base = std::env::var_os("CARGO_TARGET_DIR").map_or_else(std::env::temp_dir, PathBuf::from);
    let sequence = NEXT_TLS_FIXTURE.fetch_add(1, Ordering::Relaxed);
    base.join(format!(
        "wild-buzzard-style-resource-tls-{}-{sequence}",
        std::process::id()
    ))
}

fn reserve_address() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("reserve TLS address");
    listener.local_addr().expect("read TLS address")
}

fn wait_for_listener(child: &mut Child, address: SocketAddr) {
    let deadline = Instant::now() + SERVER_TIMEOUT;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().expect("inspect TLS startup") {
            panic!("TLS fixture exited during startup: {status}");
        }
        match TcpListener::bind(address) {
            Err(error) if error.kind() == io::ErrorKind::AddrInUse => return,
            Ok(listener) => drop(listener),
            Err(error) => panic!("inspect TLS listener: {error}"),
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("TLS fixture did not bind");
}

fn remove_fixture_file(directory: &Path, name: &str) {
    let _ = fs::remove_file(directory.join(name));
}

#[test]
fn https_document_blocks_direct_http_before_policy_and_admits_http_s_peers() {
    let cleartext_probe = TcpListener::bind("127.0.0.1:0").expect("bind mixed-content probe");
    cleartext_probe
        .set_nonblocking(true)
        .expect("arm mixed-content probe");
    let cleartext_url = format!(
        "http://{}/must-not-connect.css",
        cleartext_probe.local_addr().expect("probe address")
    );
    let mut fixture = OpenSslTlsFixture::start(
        &[(
            "Content-Security-Policy",
            "style-src 'self' https://cross.example",
        )],
        |origin| {
            html(&format!(
                "<link rel=stylesheet href=\"{cleartext_url}\">\
                 <link rel=stylesheet href=\"{origin}/same.css#drop\">\
                 <link rel=stylesheet href=\"https://cross.example/cross.css\">"
            ))
        },
    );
    let trust = TrustStore::bundled_web_pki()
        .with_der_certificate(&fixture.certificate_der)
        .expect("admit fixture trust anchor");
    let mut engine = StaticPageEngine::new_general_web_for_presentation(
        engine_config(),
        general_web_config(),
        trust,
    )
    .expect("create TLS presentation engine");
    let url = format!("{}/page.html", fixture.origin);
    engine
        .load_general_web_for_presentation(&url, &CancellationSource::new().token())
        .expect("load authenticated TLS fixture");
    let plan = StyleResourcePlan::from_live_document(
        engine.live_document().expect("retained TLS document"),
    )
    .expect("construct TLS style-resource plan");
    assert_eq!(plan.requests().len(), 2);
    assert_eq!(
        plan.requests()[0].canonical_url(),
        format!("{}/same.css", fixture.origin)
    );
    assert_eq!(
        plan.requests()[1].canonical_url(),
        "https://cross.example/cross.css"
    );
    assert!(diagnostic_kinds(&plan).contains(&StyleResourceDiagnosticKind::MixedContent));
    assert!(matches!(
        cleartext_probe.accept(),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock
    ));
    assert_eq!(plan.enforcing_policy_count(), 1);
    assert_eq!(plan.enforcing_policy_block_count(), 0);
    engine.shutdown().expect("shut down TLS engine");
    fixture.finish();
}

#[test]
fn plan_surface_is_send_sync_and_all_debug_and_errors_are_redacted() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<StyleResourcePlan>();

    let secret = "NeverRetainThisNonce";
    let body = html(&format!(
        "<link rel=stylesheet href=\"https://user:password@private.example/secret.css\" nonce=\"{secret}\">"
    ));
    let (plan, _) = load_plan("/private/document?secret=query#fragment", &[], &body);
    let plan = plan.expect("redaction plan");
    let debug = format!("{plan:?}");
    assert!(!debug.contains(secret));
    assert!(!debug.contains("password"));
    assert!(!debug.contains("private.example"));
    assert!(!debug.contains("secret.css"));
    assert!(!debug.contains("secret=query"));
    assert!(plan.requests().is_empty());
    assert!(plan.diagnostics().iter().all(|diagnostic| {
        diagnostic.document_version() == plan.document_version()
            && diagnostic.subject() != StyleResourceDiagnosticSubject::DocumentPolicy
            && diagnostic.owner().is_some()
    }));

    let oversized_rel = "r".repeat(MAX_STYLE_RESOURCE_ATTRIBUTE_BYTES + 1);
    let (error, _) = load_plan(
        "/sensitive/path?token=hidden",
        &[],
        &html(&format!("<link rel=\"{oversized_rel}\" href=/x.css>")),
    );
    let error = error.expect_err("oversized rel fails closed");
    let debug = format!("{error:?}");
    let display = error.to_string();
    assert!(!debug.contains("sensitive"));
    assert!(!debug.contains("hidden"));
    assert!(!display.contains("sensitive"));
    assert!(!display.contains("hidden"));
}
