use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::ops::Deref;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use wild_buzzard_engine::{
    CancellationSource, FontSourcePolicy, IpAddressSpace, MAX_STYLE_FETCH_AGGREGATE_BODY_BYTES,
    MAX_STYLE_FETCH_CHUNK_LINE_BYTES, MAX_STYLE_FETCH_DIAGNOSTICS, MAX_STYLE_FETCH_DURATION,
    MAX_STYLE_FETCH_HTTP_EXCHANGES, MAX_STYLE_FETCH_REDIRECTS, MAX_STYLE_FETCH_RESPONSE_BODY_BYTES,
    MAX_STYLE_FETCH_RESPONSES, NavigationGeneration, NavigationId, NonProductStyleFetchOwner,
    StaticPageConfig, StaticPageEngine, StyleFetchDiagnostic, StyleFetchDiagnosticKind,
    StyleFetchLimit, StyleFetchLimits, StyleFetchMime, StyleFetchNetworkFailure,
    StyleFetchOriginCleanliness, StyleFetchOwnerError, StyleFetchRejection,
    StyleFetchTransportPolicy, StyleResourcePlan, TopLevelContextId, TrustStore,
};
use wild_buzzard_net::{ClientConfig, GeneralWebConfig};

const TEST_TIMEOUT: Duration = Duration::from_secs(4);
const NETWORK_BODY_LIMIT: usize = 2 * 1024 * 1024;
static NEXT_TLS_FIXTURE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
struct ResponseSpec {
    status: u16,
    headers: Vec<(Vec<u8>, Vec<u8>)>,
    body: Vec<u8>,
    body_delay: Duration,
    body_gate: Option<Arc<ResponseBodyGate>>,
}

#[derive(Default)]
struct ResponseBodyGate {
    released: Mutex<bool>,
    ready: Condvar,
}

impl ResponseBodyGate {
    fn release(&self) {
        *self
            .released
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        self.ready.notify_all();
    }

    fn wait(&self) {
        let released = self
            .released
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        drop(
            self.ready
                .wait_while(released, |released| !*released)
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
    }
}

impl ResponseSpec {
    fn new(status: u16, headers: &[(&str, &str)], body: impl AsRef<[u8]>) -> Self {
        Self {
            status,
            headers: headers
                .iter()
                .map(|(name, value)| (name.as_bytes().to_vec(), value.as_bytes().to_vec()))
                .collect(),
            body: body.as_ref().to_vec(),
            body_delay: Duration::ZERO,
            body_gate: None,
        }
    }

    fn css(body: &str) -> Self {
        Self::new(200, &[("Content-Type", "text/css")], body)
    }

    fn redirect(status: u16, location: &str) -> Self {
        Self::new(status, &[("Location", location)], [])
    }

    fn with_body_delay(mut self, delay: Duration) -> Self {
        self.body_delay = delay;
        self
    }

    fn with_body_gate(mut self, gate: Arc<ResponseBodyGate>) -> Self {
        self.body_gate = Some(gate);
        self
    }
}

struct HttpFixture {
    address: SocketAddr,
    origin: String,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl HttpFixture {
    fn start(build_routes: impl FnOnce(&str) -> BTreeMap<String, ResponseSpec>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind HTTP fixture");
        listener
            .set_nonblocking(true)
            .expect("make HTTP fixture nonblocking");
        let address = listener.local_addr().expect("read HTTP fixture address");
        let origin = format!("http://{address}");
        let routes = build_routes(&origin);
        let requests = Arc::new(Mutex::new(Vec::new()));
        let request_log = Arc::clone(&requests);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_worker = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            while !stop_worker.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream
                            .set_read_timeout(Some(TEST_TIMEOUT))
                            .expect("set fixture read timeout");
                        stream
                            .set_write_timeout(Some(TEST_TIMEOUT))
                            .expect("set fixture write timeout");
                        let Some(path) = read_request_path(&mut stream) else {
                            continue;
                        };
                        request_log
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(path.clone());
                        let response = routes
                            .get(&path)
                            .cloned()
                            .unwrap_or_else(|| ResponseSpec::new(404, &[], "missing"));
                        write_response(&mut stream, &response);
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("HTTP fixture accept failed: {error}"),
                }
            }
        });
        Self {
            address,
            origin,
            requests,
            stop,
            worker: Some(worker),
        }
    }

    fn request_count(&self, path: &str) -> usize {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|request| request.as_str() == path)
            .count()
    }
}

impl Drop for HttpFixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = TcpStream::connect(self.address);
        if let Some(worker) = self.worker.take() {
            worker.join().expect("join HTTP fixture");
        }
    }
}

fn read_request_path(stream: &mut TcpStream) -> Option<String> {
    let mut request = Vec::new();
    let mut chunk = [0_u8; 1024];
    while request.len() <= 64 * 1024 {
        let count = stream.read(&mut chunk).ok()?;
        if count == 0 {
            return None;
        }
        request.extend_from_slice(&chunk[..count]);
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    let line_end = request.windows(2).position(|window| window == b"\r\n")?;
    let line = std::str::from_utf8(&request[..line_end]).ok()?;
    line.split_ascii_whitespace().nth(1).map(str::to_owned)
}

fn write_response(stream: &mut TcpStream, response: &ResponseSpec) {
    let reason = match response.status {
        200 => "OK",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        305 => "Use Proxy",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        404 => "Not Found",
        _ => "Fixture",
    };
    write!(stream, "HTTP/1.1 {} {reason}\r\n", response.status).expect("write fixture status");
    let has_length = response
        .headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case(b"content-length"));
    let has_transfer_encoding = response
        .headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case(b"transfer-encoding"));
    for (name, value) in &response.headers {
        stream.write_all(name).expect("write fixture header name");
        stream.write_all(b": ").expect("write fixture separator");
        stream.write_all(value).expect("write fixture header value");
        stream.write_all(b"\r\n").expect("write fixture CRLF");
    }
    if !has_length && !has_transfer_encoding && response.status != 204 {
        write!(stream, "Content-Length: {}\r\n", response.body.len())
            .expect("write fixture content length");
    }
    stream
        .write_all(b"Connection: close\r\n\r\n")
        .expect("finish fixture head");
    stream.flush().expect("flush fixture head");
    if !response.body_delay.is_zero() {
        thread::sleep(response.body_delay);
    }
    if let Some(gate) = &response.body_gate {
        gate.wait();
    }
    let _ = stream.write_all(&response.body);
}

fn http_config() -> ClientConfig {
    ClientConfig::default()
        .with_max_body_bytes(NETWORK_BODY_LIMIT)
        .with_connect_timeout(Duration::from_secs(1))
        .with_read_timeout(Duration::from_secs(2))
        .with_write_timeout(Duration::from_secs(2))
}

fn general_web_config() -> GeneralWebConfig {
    GeneralWebConfig::default()
        .with_http_config(http_config())
        .with_dns_timeout(Duration::from_secs(2))
        .with_tls_handshake_timeout(Duration::from_secs(2))
        .with_max_dns_candidates(8)
        .with_max_connection_attempts(8)
}

fn engine_config() -> StaticPageConfig {
    StaticPageConfig {
        viewport_width: 320,
        viewport_height: 200,
        operation_timeout: Duration::from_secs(10),
        network: http_config(),
        font_source: FontSourcePolicy::EmbeddedOnly,
        ..StaticPageConfig::default()
    }
}

fn document(head: &str) -> String {
    format!("<!doctype html><html><head>{head}</head><body>fixture</body></html>")
}

fn document_response(head: &str, policy_headers: &[(&str, &str)]) -> ResponseSpec {
    let mut headers = vec![("Content-Type", "text/html")];
    headers.extend_from_slice(policy_headers);
    ResponseSpec::new(200, &headers, document(head))
}

fn diagnostic_priority_fixture(report_only: bool, reject_final: bool) -> HttpFixture {
    HttpFixture::start(move |_| {
        let policy = if report_only {
            vec![("Content-Security-Policy-Report-Only", "style-src 'none'")]
        } else {
            Vec::new()
        };
        let final_response = if reject_final {
            ResponseSpec::new(200, &[("Content-Type", "text/html")], "not css")
        } else {
            ResponseSpec::new(200, &[], "a{}")
        };
        BTreeMap::from([
            (
                "/page".to_owned(),
                document_response("<link rel=stylesheet href=/start.css>", policy.as_slice()),
            ),
            (
                "/start.css".to_owned(),
                ResponseSpec::redirect(302, "/final.css"),
            ),
            ("/final.css".to_owned(), final_response),
        ])
    })
}

struct PlannedStyleFetch {
    plan: StyleResourcePlan,
    engine: StaticPageEngine,
}

impl Deref for PlannedStyleFetch {
    type Target = StyleResourcePlan;

    fn deref(&self) -> &Self::Target {
        &self.plan
    }
}

fn plan_from_http(fixture: &HttpFixture) -> PlannedStyleFetch {
    let mut engine = StaticPageEngine::new_general_web_for_presentation(
        engine_config(),
        general_web_config(),
        TrustStore::bundled_web_pki(),
    )
    .expect("create HTTP document engine");
    let url = format!("{}/page", fixture.origin);
    engine
        .load_general_web_for_presentation(&url, &CancellationSource::new().token())
        .expect("load HTTP document fixture");
    let plan = StyleResourcePlan::from_live_document(
        engine.live_document().expect("retained HTTP document"),
    )
    .expect("construct stylesheet plan");
    PlannedStyleFetch { plan, engine }
}

fn owner(plan: &PlannedStyleFetch, limits: StyleFetchLimits) -> NonProductStyleFetchOwner {
    let authority = plan
        .engine
        .delegate_non_product_style_fetch_authority(StyleFetchTransportPolicy::default())
        .expect("delegate exact stylesheet authority");
    NonProductStyleFetchOwner::new(authority, limits).expect("construct stylesheet fetch owner")
}

fn default_fetch(
    owner: &mut NonProductStyleFetchOwner,
    plan: &StyleResourcePlan,
) -> wild_buzzard_engine::StyleFetchSet {
    owner
        .fetch_plan(
            plan,
            &CancellationSource::new().token(),
            Instant::now() + TEST_TIMEOUT,
        )
        .expect("fetch stylesheet plan")
}

#[test]
fn admitted_http_responses_preserve_order_identity_headers_and_bodies() {
    let fixture = HttpFixture::start(|_| {
        BTreeMap::from([
            (
                "/page".to_owned(),
                document_response(
                    "<link rel=stylesheet href='/a.css?private=query'>\
                     <link rel=stylesheet href='/b.css'>",
                    &[("Content-Security-Policy-Report-Only", "style-src 'none'")],
                ),
            ),
            (
                "/a.css?private=query".to_owned(),
                ResponseSpec::new(
                    200,
                    &[
                        ("Content-Type", " TeXt/CsS ; charset=utf-8 "),
                        ("X-Content-Type-Options", " NoSniff , ignored "),
                    ],
                    "body{color:super-secret}",
                ),
            ),
            (
                "/b.css".to_owned(),
                ResponseSpec::new(200, &[], "p{display:block}"),
            ),
        ])
    });
    let plan = plan_from_http(&fixture);
    let set = default_fetch(&mut owner(&plan, StyleFetchLimits::default()), &plan);

    assert_eq!(set.document_version(), plan.document_version());
    assert_eq!(set.navigation_commit(), plan.navigation_commit());
    assert_eq!(
        plan.navigation_commit().address_space(),
        Some(IpAddressSpace::Local)
    );
    assert_eq!(set.responses().len(), 2);
    assert_eq!(set.http_exchanges(), 2);
    assert_eq!(set.responses()[0].request_index(), 0);
    assert_eq!(set.responses()[1].request_index(), 1);
    assert_eq!(set.responses()[0].owner(), plan.requests()[0].owner());
    assert_eq!(set.responses()[1].owner(), plan.requests()[1].owner());
    assert_eq!(
        set.responses()[0].final_url(),
        format!("{}/a.css?private=query", fixture.origin)
    );
    assert_eq!(set.responses()[0].status(), 200);
    assert_eq!(set.responses()[0].redirect_count(), 0);
    assert_eq!(
        set.responses()[0].origin_cleanliness(),
        StyleFetchOriginCleanliness::Clean
    );
    assert_eq!(set.responses()[0].headers().mime(), StyleFetchMime::Css);
    assert!(set.responses()[0].headers().nosniff());
    assert_eq!(
        set.responses()[0].headers().content_type(),
        Some(b"TeXt/CsS ; charset=utf-8".as_slice())
    );
    assert_eq!(
        set.responses()[0].headers().charset(),
        Some(b"utf-8".as_slice())
    );
    assert_eq!(set.responses()[0].body(), b"body{color:super-secret}");
    assert_eq!(set.responses()[1].headers().mime(), StyleFetchMime::Unknown);
    assert_eq!(set.responses()[1].headers().content_type(), None);
    assert_eq!(set.responses()[1].headers().charset(), None);
    assert_eq!(
        set.aggregate_body_bytes(),
        set.responses()
            .iter()
            .map(|response| response.body().len())
            .sum()
    );
    assert!(set.diagnostics().iter().any(|diagnostic| {
        diagnostic.request_index() == 1
            && diagnostic.kind() == StyleFetchDiagnosticKind::UnknownMimeAccepted
    }));
    assert_eq!(
        set.diagnostics()
            .iter()
            .filter(|diagnostic| matches!(
                diagnostic.kind(),
                StyleFetchDiagnosticKind::ReportOnlyWouldBlock { policies: 1 }
            ))
            .count(),
        2
    );

    let debug = format!("{set:?}");
    for secret in ["a.css", "private=query", "charset=utf-8", "super-secret"] {
        assert!(!debug.contains(secret));
    }
}

#[test]
fn replaced_document_revokes_old_owner_and_plan_before_network() {
    let fixture = HttpFixture::start(|_| {
        BTreeMap::from([
            (
                "/page".to_owned(),
                document_response("<link rel=stylesheet href=/sheet.css>", &[]),
            ),
            (
                "/other".to_owned(),
                document_response("<link rel=stylesheet href=/sheet.css>", &[]),
            ),
            (
                "/third".to_owned(),
                document_response("<link rel=stylesheet href=/sheet.css>", &[]),
            ),
            ("/sheet.css".to_owned(), ResponseSpec::css("a{}")),
        ])
    });
    let mut original = plan_from_http(&fixture);
    let mut stale_owner = owner(&original, StyleFetchLimits::default());

    for path in ["other", "third"] {
        original
            .engine
            .load_general_web_for_presentation(
                &format!("{}/{path}", fixture.origin),
                &CancellationSource::new().token(),
            )
            .expect("replace the direct document");
    }

    let failure = stale_owner
        .fetch_plan(
            &original.plan,
            &CancellationSource::new().token(),
            Instant::now() + TEST_TIMEOUT,
        )
        .expect_err("a replaced document's owner must be revoked");
    assert_eq!(
        failure.error().rejection(),
        StyleFetchRejection::DocumentNotCurrent
    );
    assert_eq!(fixture.request_count("/sheet.css"), 0);
}

#[test]
fn dropping_the_current_document_revokes_its_detached_owner_before_network() {
    let fixture = HttpFixture::start(|_| {
        BTreeMap::from([
            (
                "/page".to_owned(),
                document_response("<link rel=stylesheet href=/sheet.css>", &[]),
            ),
            ("/sheet.css".to_owned(), ResponseSpec::css("a{}")),
        ])
    });
    let planned = plan_from_http(&fixture);
    let mut stale_owner = owner(&planned, StyleFetchLimits::default());
    let PlannedStyleFetch { plan, engine } = planned;
    drop(engine);

    let failure = stale_owner
        .fetch_plan(
            &plan,
            &CancellationSource::new().token(),
            Instant::now() + TEST_TIMEOUT,
        )
        .expect_err("dropping the current-document owner must revoke stylesheet admission");
    assert_eq!(
        failure.error().rejection(),
        StyleFetchRejection::DocumentNotCurrent
    );
    assert_eq!(fixture.request_count("/sheet.css"), 0);
}

#[test]
fn replacement_during_admission_linearizes_before_new_document_publication() {
    let body_gate = Arc::new(ResponseBodyGate::default());
    let response_gate = Arc::clone(&body_gate);
    let original_fixture = HttpFixture::start(move |_| {
        BTreeMap::from([
            (
                "/page".to_owned(),
                document_response("<link rel=stylesheet href=/slow.css>", &[]),
            ),
            (
                "/slow.css".to_owned(),
                ResponseSpec::css("a{}").with_body_gate(response_gate),
            ),
        ])
    });
    let replacement_fixture = HttpFixture::start(|_| {
        BTreeMap::from([(
            "/page".to_owned(),
            document_response("<p>replacement</p>", &[]),
        )])
    });
    let planned = plan_from_http(&original_fixture);
    let fetch_owner = owner(&planned, StyleFetchLimits::default());
    let PlannedStyleFetch { plan, engine } = planned;
    let plan = Arc::new(plan);
    let fetch_plan = Arc::clone(&plan);
    let fetch_worker = thread::spawn(move || {
        let mut fetch_owner = fetch_owner;
        let result = fetch_owner.fetch_plan(
            &fetch_plan,
            &CancellationSource::new().token(),
            Instant::now() + TEST_TIMEOUT,
        );
        (fetch_owner, result)
    });

    let wait_deadline = Instant::now() + TEST_TIMEOUT;
    while original_fixture.request_count("/slow.css") == 0 && Instant::now() < wait_deadline {
        thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(original_fixture.request_count("/slow.css"), 1);

    let replacement_requests = Arc::clone(&replacement_fixture.requests);
    let release_gate = Arc::clone(&body_gate);
    let release_worker = thread::spawn(move || {
        let wait_deadline = Instant::now() + TEST_TIMEOUT;
        while !replacement_requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .any(|path| path == "/page")
            && Instant::now() < wait_deadline
        {
            thread::sleep(Duration::from_millis(2));
        }
        thread::sleep(Duration::from_millis(40));
        release_gate.release();
    });
    let mut engine = engine;
    let replacement_started = Instant::now();
    engine
        .load_general_web_for_presentation(
            &format!("{}/page", replacement_fixture.origin),
            &CancellationSource::new().token(),
        )
        .expect("replacement succeeds after retiring the old document");
    let replacement_elapsed = replacement_started.elapsed();
    release_worker.join().expect("join response release");
    assert_eq!(replacement_fixture.request_count("/page"), 1);
    assert!(
        replacement_elapsed >= Duration::from_millis(35),
        "replacement must not publish while the exact old transaction is active"
    );

    let (mut stale_owner, fetched) = fetch_worker.join().expect("join admitted fetch");
    let fetched = fetched.expect("the transaction which linearized first remains admitted");
    assert_eq!(fetched.responses()[0].body(), b"a{}");

    let failure = stale_owner
        .fetch_plan(
            &plan,
            &CancellationSource::new().token(),
            Instant::now() + TEST_TIMEOUT,
        )
        .expect_err("retired owner cannot start another transaction");
    assert_eq!(
        failure.error().rejection(),
        StyleFetchRejection::DocumentNotCurrent
    );
    assert_eq!(original_fixture.request_count("/slow.css"), 1);
}

#[test]
fn current_owner_rejects_same_client_old_response_graft_before_network() {
    let fixture = HttpFixture::start(|_| {
        BTreeMap::from([
            (
                "/page".to_owned(),
                document_response("<link rel=stylesheet href=/sheet.css>", &[]),
            ),
            (
                "/other".to_owned(),
                document_response("<link rel=stylesheet href=/sheet.css>", &[]),
            ),
            ("/sheet.css".to_owned(), ResponseSpec::css("a{}")),
        ])
    });
    let mut current = plan_from_http(&fixture);
    current
        .engine
        .load_general_web_for_presentation(
            &format!("{}/other", fixture.origin),
            &CancellationSource::new().token(),
        )
        .expect("load a second response through the same client");
    let replacement_plan = StyleResourcePlan::from_live_document(
        current
            .engine
            .live_document()
            .expect("replacement document"),
    )
    .expect("plan replacement response");
    let old_plan = std::mem::replace(&mut current.plan, replacement_plan);
    let mut current_owner = owner(&current, StyleFetchLimits::default());

    let failure = current_owner
        .fetch_plan(
            &old_plan,
            &CancellationSource::new().token(),
            Instant::now() + TEST_TIMEOUT,
        )
        .expect_err("an old response plan must not graft onto the current owner");
    assert_eq!(
        failure.error().rejection(),
        StyleFetchRejection::PlanOwnership
    );
    assert_eq!(fixture.request_count("/sheet.css"), 0);
}

#[test]
fn current_owner_rejects_cross_client_response_graft_before_network() {
    let fixture = HttpFixture::start(|_| {
        BTreeMap::from([
            (
                "/page".to_owned(),
                document_response("<link rel=stylesheet href=/sheet.css>", &[]),
            ),
            ("/sheet.css".to_owned(), ResponseSpec::css("a{}")),
        ])
    });
    let original = plan_from_http(&fixture);
    let mut exact_owner = owner(&original, StyleFetchLimits::default());
    let foreign_fixture = HttpFixture::start(|_| {
        BTreeMap::from([
            (
                "/page".to_owned(),
                document_response("<link rel=stylesheet href=/sheet.css>", &[]),
            ),
            ("/sheet.css".to_owned(), ResponseSpec::css("foreign{}")),
        ])
    });
    let foreign = plan_from_http(&foreign_fixture);
    let failure = exact_owner
        .fetch_plan(
            &foreign,
            &CancellationSource::new().token(),
            Instant::now() + TEST_TIMEOUT,
        )
        .expect_err("cross-client document graft must fail");
    assert_eq!(
        failure.error().rejection(),
        StyleFetchRejection::PlanOwnership
    );
    assert_eq!(foreign_fixture.request_count("/sheet.css"), 0);
    assert_eq!(fixture.request_count("/sheet.css"), 0);
}

#[test]
fn report_only_diagnostics_are_bounded_lossy_while_enforcing_csp_still_blocks() {
    let report_only = HttpFixture::start(|_| {
        BTreeMap::from([
            (
                "/page".to_owned(),
                document_response(
                    "<link rel=stylesheet href=/one.css><link rel=stylesheet href=/two.css>",
                    &[("Content-Security-Policy-Report-Only", "style-src 'none'")],
                ),
            ),
            ("/one.css".to_owned(), ResponseSpec::css("a{}")),
            ("/two.css".to_owned(), ResponseSpec::css("b{}")),
        ])
    });
    let report_only_plan = plan_from_http(&report_only);

    let zero_diagnostics =
        StyleFetchLimits::new(2, 16, 32, 0, 2, 0).expect("zero diagnostic budget is valid");
    let zero_set = default_fetch(
        &mut owner(&report_only_plan, zero_diagnostics),
        &report_only_plan,
    );
    assert_eq!(zero_set.responses().len(), 2);
    assert!(zero_set.diagnostics().is_empty());
    assert_eq!(std::mem::size_of_val(zero_set.diagnostics()), 0);
    assert_eq!(report_only.request_count("/one.css"), 1);
    assert_eq!(report_only.request_count("/two.css"), 1);

    let one_diagnostic =
        StyleFetchLimits::new(2, 16, 32, 0, 2, 1).expect("one diagnostic budget is valid");
    let report_only_plan = plan_from_http(&report_only);
    let exhausted_set = default_fetch(
        &mut owner(&report_only_plan, one_diagnostic),
        &report_only_plan,
    );
    assert_eq!(exhausted_set.responses().len(), 2);
    assert_eq!(exhausted_set.diagnostics().len(), 1);
    assert_eq!(
        exhausted_set.diagnostics()[0].kind(),
        StyleFetchDiagnosticKind::ReportOnlyWouldBlock { policies: 1 }
    );
    assert_eq!(exhausted_set.diagnostics()[0].request_index(), 0);
    assert_eq!(
        std::mem::size_of_val(exhausted_set.diagnostics()),
        std::mem::size_of::<StyleFetchDiagnostic>()
    );
    assert_eq!(report_only.request_count("/one.css"), 2);
    assert_eq!(report_only.request_count("/two.css"), 2);

    let enforcing = HttpFixture::start(|_| {
        BTreeMap::from([
            (
                "/page".to_owned(),
                document_response(
                    "<link rel=stylesheet href=/blocked.css>",
                    &[("Content-Security-Policy", "style-src 'none'")],
                ),
            ),
            ("/blocked.css".to_owned(), ResponseSpec::css("secret{}")),
        ])
    });
    let enforcing_plan = plan_from_http(&enforcing);
    assert!(enforcing_plan.requests().is_empty());
    let enforcing_set = default_fetch(
        &mut owner(&enforcing_plan, zero_diagnostics),
        &enforcing_plan,
    );
    assert!(enforcing_set.responses().is_empty());
    assert!(enforcing_set.diagnostics().is_empty());
    assert_eq!(enforcing.request_count("/blocked.css"), 0);
}

#[test]
fn report_only_input_cannot_change_redirect_unknown_mime_or_rejection_results() {
    for max_diagnostics in [0, 1] {
        let plain_success = diagnostic_priority_fixture(false, false);
        let report_success = diagnostic_priority_fixture(true, false);
        let plain_plan = plan_from_http(&plain_success);
        let report_plan = plan_from_http(&report_success);
        let limits = StyleFetchLimits::new(1, 16, 16, 2, 2, max_diagnostics)
            .expect("diagnostic-priority success limits");
        let plain_set = default_fetch(&mut owner(&plain_plan, limits), &plain_plan);
        let report_set = default_fetch(&mut owner(&report_plan, limits), &report_plan);

        assert_eq!(
            plain_set.responses()[0].body(),
            report_set.responses()[0].body()
        );
        assert_eq!(plain_set.responses()[0].body(), b"a{}");
        assert_eq!(plain_set.http_exchanges(), report_set.http_exchanges());
        assert_eq!(plain_set.http_exchanges(), 2);
        assert_eq!(plain_success.request_count("/start.css"), 1);
        assert_eq!(plain_success.request_count("/final.css"), 1);
        assert_eq!(report_success.request_count("/start.css"), 1);
        assert_eq!(report_success.request_count("/final.css"), 1);
        let plain_kinds = plain_set
            .diagnostics()
            .iter()
            .map(|diagnostic| diagnostic.kind())
            .collect::<Vec<_>>();
        let report_kinds = report_set
            .diagnostics()
            .iter()
            .map(|diagnostic| diagnostic.kind())
            .collect::<Vec<_>>();
        assert_eq!(plain_kinds, report_kinds);
        if max_diagnostics == 0 {
            assert!(plain_kinds.is_empty());
        } else {
            assert_eq!(
                plain_kinds,
                vec![StyleFetchDiagnosticKind::RedirectFollowed { status: 302 }]
            );
        }

        let plain_rejection = diagnostic_priority_fixture(false, true);
        let report_rejection = diagnostic_priority_fixture(true, true);
        let plain_plan = plan_from_http(&plain_rejection);
        let report_plan = plan_from_http(&report_rejection);
        let plain_failure = owner(&plain_plan, limits)
            .fetch_plan(
                &plain_plan,
                &CancellationSource::new().token(),
                Instant::now() + TEST_TIMEOUT,
            )
            .expect_err("explicit non-CSS must reject without report-only input");
        let report_failure = owner(&report_plan, limits)
            .fetch_plan(
                &report_plan,
                &CancellationSource::new().token(),
                Instant::now() + TEST_TIMEOUT,
            )
            .expect_err("explicit non-CSS must reject with report-only input");
        assert_eq!(
            plain_failure.error().rejection(),
            StyleFetchRejection::MimeNotCss
        );
        assert_eq!(
            plain_failure.error().rejection(),
            report_failure.error().rejection()
        );
        assert_eq!(
            plain_failure.error().request_index(),
            report_failure.error().request_index()
        );
        assert_eq!(
            plain_failure.error().redirect_index(),
            report_failure.error().redirect_index()
        );
        let plain_failure_kinds = plain_failure
            .diagnostics()
            .iter()
            .map(|diagnostic| diagnostic.kind())
            .collect::<Vec<_>>();
        let report_failure_kinds = report_failure
            .diagnostics()
            .iter()
            .map(|diagnostic| diagnostic.kind())
            .collect::<Vec<_>>();
        assert_eq!(plain_failure_kinds, report_failure_kinds);
        assert_eq!(plain_rejection.request_count("/start.css"), 1);
        assert_eq!(plain_rejection.request_count("/final.css"), 1);
        assert_eq!(report_rejection.request_count("/start.css"), 1);
        assert_eq!(report_rejection.request_count("/final.css"), 1);
    }
}

#[test]
fn restricted_stylesheet_initial_and_redirect_ports_are_typed_and_never_connected() {
    let blocked_listener = [10080_u16, 6697, 6679, 6669, 6668, 6667, 6666, 6665]
        .into_iter()
        .find_map(|port| TcpListener::bind(("127.0.0.1", port)).ok())
        .expect("bind one high restricted stylesheet test port");
    blocked_listener
        .set_nonblocking(true)
        .expect("make restricted stylesheet listener nonblocking");
    let blocked_address = blocked_listener
        .local_addr()
        .expect("read restricted stylesheet listener address");
    let link = format!("<link rel=stylesheet href=http://{blocked_address}/blocked.css>");
    let fixture = HttpFixture::start(move |_| {
        BTreeMap::from([("/page".to_owned(), document_response(&link, &[]))])
    });
    let plan = plan_from_http(&fixture);
    let failure = owner(&plan, StyleFetchLimits::default())
        .fetch_plan(
            &plan,
            &CancellationSource::new().token(),
            Instant::now() + TEST_TIMEOUT,
        )
        .expect_err("restricted stylesheet port must fail before connect");
    assert_eq!(
        failure.error().rejection(),
        StyleFetchRejection::Network(StyleFetchNetworkFailure::RestrictedPort)
    );
    assert!(matches!(
        blocked_listener.accept(),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock
    ));

    let redirect_location = format!("http://{blocked_address}/redirect-blocked.css");
    let redirect_fixture = HttpFixture::start(move |_| {
        BTreeMap::from([
            (
                "/page".to_owned(),
                document_response("<link rel=stylesheet href=/start.css>", &[]),
            ),
            (
                "/start.css".to_owned(),
                ResponseSpec::redirect(302, &redirect_location),
            ),
        ])
    });
    let redirect_plan = plan_from_http(&redirect_fixture);
    let redirect_failure = owner(&redirect_plan, StyleFetchLimits::default())
        .fetch_plan(
            &redirect_plan,
            &CancellationSource::new().token(),
            Instant::now() + TEST_TIMEOUT,
        )
        .expect_err("restricted redirect port must fail before connect");
    assert_eq!(
        redirect_failure.error().rejection(),
        StyleFetchRejection::Network(StyleFetchNetworkFailure::RestrictedPort)
    );
    assert_eq!(redirect_failure.error().redirect_index(), 1);
    assert_eq!(redirect_fixture.request_count("/start.css"), 1);
    assert!(matches!(
        blocked_listener.accept(),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock
    ));
}

#[test]
fn same_origin_redirects_revalidate_and_retain_exact_final_identity() {
    let fixture = HttpFixture::start(|_| {
        BTreeMap::from([
            (
                "/page".to_owned(),
                document_response(
                    "<link rel=stylesheet href=/start.css>",
                    &[("Content-Security-Policy", "style-src 'self'")],
                ),
            ),
            (
                "/start.css".to_owned(),
                ResponseSpec::redirect(301, " /middle.css "),
            ),
            (
                "/middle.css".to_owned(),
                ResponseSpec::redirect(307, "final.css#discard"),
            ),
            (
                "/final.css".to_owned(),
                ResponseSpec::css("html{background:black}"),
            ),
        ])
    });
    let plan = plan_from_http(&fixture);
    let set = default_fetch(&mut owner(&plan, StyleFetchLimits::default()), &plan);
    assert_eq!(set.responses().len(), 1);
    assert_eq!(set.responses()[0].redirect_count(), 2);
    assert_eq!(
        set.responses()[0].origin_cleanliness(),
        StyleFetchOriginCleanliness::Clean
    );
    assert_eq!(
        set.responses()[0].final_url(),
        format!("{}/final.css", fixture.origin)
    );
    assert_eq!(set.http_exchanges(), 3);
    assert_eq!(
        set.diagnostics()
            .iter()
            .filter(|diagnostic| matches!(
                diagnostic.kind(),
                StyleFetchDiagnosticKind::RedirectFollowed { .. }
            ))
            .count(),
        2
    );
    assert_eq!(fixture.request_count("/start.css"), 1);
    assert_eq!(fixture.request_count("/middle.css"), 1);
    assert_eq!(fixture.request_count("/final.css"), 1);
}

#[test]
fn no_csp_allows_cross_origin_redirect_but_enforcing_csp_blocks_before_connection() {
    let destination = HttpFixture::start(|_| {
        BTreeMap::from([("/final.css".to_owned(), ResponseSpec::css("body{margin:0}"))])
    });
    let allowed_location = format!("{}/final.css", destination.origin);
    let allowed = HttpFixture::start(|_| {
        BTreeMap::from([
            (
                "/page".to_owned(),
                document_response("<link rel=stylesheet href=/start.css>", &[]),
            ),
            (
                "/start.css".to_owned(),
                ResponseSpec::redirect(302, &allowed_location),
            ),
        ])
    });
    let allowed_plan = plan_from_http(&allowed);
    let allowed_set = default_fetch(
        &mut owner(&allowed_plan, StyleFetchLimits::default()),
        &allowed_plan,
    );
    assert_eq!(allowed_set.responses()[0].final_url(), allowed_location);
    assert_eq!(
        allowed_set.responses()[0].origin_cleanliness(),
        StyleFetchOriginCleanliness::Tainted
    );
    assert_eq!(destination.request_count("/final.css"), 1);

    let blocked_location = format!("{}/blocked.css", destination.origin);
    let blocked = HttpFixture::start(|_| {
        BTreeMap::from([
            (
                "/page".to_owned(),
                document_response(
                    "<link rel=stylesheet href=/start.css>",
                    &[("Content-Security-Policy", "style-src 'self'")],
                ),
            ),
            (
                "/start.css".to_owned(),
                ResponseSpec::redirect(302, &blocked_location),
            ),
        ])
    });
    let blocked_plan = plan_from_http(&blocked);
    let failure = owner(&blocked_plan, StyleFetchLimits::default())
        .fetch_plan(
            &blocked_plan,
            &CancellationSource::new().token(),
            Instant::now() + TEST_TIMEOUT,
        )
        .expect_err("enforcing CSP must reject an unproven cross-origin hop");
    assert_eq!(
        failure.error().rejection(),
        StyleFetchRejection::CrossOriginPolicyUnproven
    );
    assert_eq!(destination.request_count("/blocked.css"), 0);
}

#[test]
fn cssom_origin_taint_tracks_initial_origin_and_every_redirect_hop() {
    let initial_cross_origin = HttpFixture::start(|origin| {
        let port = origin
            .rsplit_once(':')
            .expect("fixture origin has a port")
            .1;
        let cross_origin = format!("http://localhost:{port}/direct.css");
        BTreeMap::from([
            (
                "/page".to_owned(),
                document_response(&format!("<link rel=stylesheet href='{cross_origin}'>"), &[]),
            ),
            (
                "/direct.css".to_owned(),
                ResponseSpec::css("body{color:purple}"),
            ),
        ])
    });
    let plan = plan_from_http(&initial_cross_origin);
    let set = default_fetch(&mut owner(&plan, StyleFetchLimits::default()), &plan);
    assert_eq!(
        set.responses()[0].origin_cleanliness(),
        StyleFetchOriginCleanliness::Tainted
    );

    let cross_origin_round_trip = HttpFixture::start(|origin| {
        let port = origin
            .rsplit_once(':')
            .expect("fixture origin has a port")
            .1;
        let cross_hop = format!("http://localhost:{port}/cross.css");
        let same_origin_final = format!("{origin}/final.css");
        BTreeMap::from([
            (
                "/page".to_owned(),
                document_response("<link rel=stylesheet href=/start.css>", &[]),
            ),
            (
                "/start.css".to_owned(),
                ResponseSpec::redirect(302, &cross_hop),
            ),
            (
                "/cross.css".to_owned(),
                ResponseSpec::redirect(302, &same_origin_final),
            ),
            (
                "/final.css".to_owned(),
                ResponseSpec::css("body{color:teal}"),
            ),
        ])
    });
    let plan = plan_from_http(&cross_origin_round_trip);
    let set = default_fetch(&mut owner(&plan, StyleFetchLimits::default()), &plan);
    assert_eq!(
        set.responses()[0].final_url(),
        format!("{}/final.css", cross_origin_round_trip.origin)
    );
    assert_eq!(set.responses()[0].redirect_count(), 2);
    assert_eq!(
        set.responses()[0].origin_cleanliness(),
        StyleFetchOriginCleanliness::Tainted
    );
}

#[derive(Debug, Eq, PartialEq)]
struct FetchedMimeEvidence {
    mime: StyleFetchMime,
    content_type: Option<Vec<u8>>,
    charset: Option<Vec<u8>>,
    nosniff: bool,
}

fn fetch_single_case(
    status: u16,
    headers: &[(&str, &str)],
    body: &str,
) -> Result<FetchedMimeEvidence, StyleFetchRejection> {
    let response = ResponseSpec::new(status, headers, body);
    let fixture = HttpFixture::start(|_| {
        BTreeMap::from([
            (
                "/page".to_owned(),
                document_response("<link rel=stylesheet href=/sheet.css>", &[]),
            ),
            ("/sheet.css".to_owned(), response),
        ])
    });
    let plan = plan_from_http(&fixture);
    owner(&plan, StyleFetchLimits::default())
        .fetch_plan(
            &plan,
            &CancellationSource::new().token(),
            Instant::now() + TEST_TIMEOUT,
        )
        .map(|set| {
            let headers = set.responses()[0].headers();
            FetchedMimeEvidence {
                mime: headers.mime(),
                content_type: headers.content_type().map(<[u8]>::to_vec),
                charset: headers.charset().map(<[u8]>::to_vec),
                nosniff: headers.nosniff(),
            }
        })
        .map_err(|failure| failure.error().rejection())
}

fn fetch_single_mime(
    status: u16,
    headers: &[(&str, &str)],
    body: &str,
) -> Result<StyleFetchMime, StyleFetchRejection> {
    fetch_single_case(status, headers, body).map(|evidence| evidence.mime)
}

#[test]
fn status_mime_and_nosniff_follow_firefox_response_admission() {
    assert_eq!(
        fetch_single_mime(200, &[("Content-Type", "text/css; charset=utf-8")], "a{}"),
        Ok(StyleFetchMime::Css)
    );
    assert_eq!(
        fetch_single_mime(204, &[("Content-Type", "TEXT/CSS")], ""),
        Ok(StyleFetchMime::Css)
    );
    assert_eq!(
        fetch_single_mime(200, &[], "a{}"),
        Ok(StyleFetchMime::Unknown)
    );
    assert_eq!(
        fetch_single_mime(200, &[("Content-Type", "oops")], "a{}"),
        Ok(StyleFetchMime::Unknown)
    );
    assert_eq!(
        fetch_single_mime(404, &[("Content-Type", "text/css")], "a{}"),
        Err(StyleFetchRejection::HttpStatus)
    );
    assert_eq!(
        fetch_single_mime(200, &[("Content-Type", "text/plain")], "a{}"),
        Err(StyleFetchRejection::MimeNotCss)
    );
    assert_eq!(
        fetch_single_mime(
            200,
            &[("X-Content-Type-Options", "nosniff, ignored")],
            "a{}"
        ),
        Err(StyleFetchRejection::NoSniff)
    );
    assert_eq!(
        fetch_single_mime(
            200,
            &[("Content-Type", "text/css"), ("Content-Type", "text/css"),],
            "a{}"
        ),
        Ok(StyleFetchMime::Css)
    );
}

#[test]
fn merged_content_type_matches_firefox_fetch_extraction_table() {
    let cases = [
        ("contentTypes1", ",text/css", true, None),
        ("contentTypes2", "text/css,", true, None),
        ("contentTypes3", "text/html,text/css", true, None),
        (
            "contentTypes4",
            "text/plain;charset=gbk,text/css",
            true,
            None,
        ),
        (
            "contentTypes5",
            "text/plain;charset=gbk,text/css;charset=windows-1254",
            true,
            Some("windows-1254"),
        ),
        (
            "contentTypes6",
            "text/css;charset=gbk,text/css",
            true,
            Some("gbk"),
        ),
        (
            "contentTypes7",
            "text/css;charset=gbk,text/css;charset=windows-1252",
            true,
            Some("windows-1252"),
        ),
        (
            "contentTypes8",
            "text/css;charset=gbk,text/css;x=\",text/plain",
            true,
            Some("gbk"),
        ),
        (
            "contentTypes9",
            "text/css;charset=gbk;x=foo,text/css",
            true,
            Some("gbk"),
        ),
        (
            "contentTypes10",
            "text/css;charset=gbk,text/plain,text/css",
            true,
            None,
        ),
        ("contentTypes11", "text/css,*/*", true, None),
        ("contentTypes12", "text/html,*/*", false, None),
        ("contentTypes13", "*/*,text/css", true, None),
        ("contentTypes14", "text/css,*/*;charset=gbk", true, None),
        ("contentTypes15", "text/html,*/*;charset=gbk", false, None),
        ("contentTypes16", "text/css;x=\",text/plain", true, None),
        ("contentTypes17", "text/css;\",text/plain", true, None),
        ("contentTypes18", "text/css;\",\\\",text/plain", true, None),
        (
            "contentTypes19",
            "text/css;\",\\\",text/plain,\";charset=GBK",
            true,
            Some("GBK"),
        ),
        ("contentTypes20", "text/css;\",\",text/plain", false, None),
    ];

    for (name, value, admits_css, expected_charset) in cases {
        let result = fetch_single_case(200, &[("Content-Type", value)], "a{}");
        if admits_css {
            let evidence = result.unwrap_or_else(|error| panic!("{name} rejected: {error:?}"));
            assert_eq!(evidence.mime, StyleFetchMime::Css, "{name}");
            assert_eq!(
                evidence.charset.as_deref(),
                expected_charset.map(str::as_bytes),
                "{name}"
            );
        } else {
            assert_eq!(result, Err(StyleFetchRejection::MimeNotCss), "{name}");
        }
    }

    let merged = fetch_single_case(
        200,
        &[
            ("Content-Type", "text/html"),
            ("Content-Type", "text/css;charset=utf-8"),
        ],
        "a{}",
    )
    .expect("multiple Content-Type fields merge before extraction");
    assert_eq!(
        merged.content_type.as_deref(),
        Some(b"text/html,text/css;charset=utf-8".as_slice())
    );
    assert_eq!(merged.charset.as_deref(), Some(b"utf-8".as_slice()));

    for explicit_non_css in [
        "text/html,text/plain",
        "invalid,text/plain",
        "text/html;charset=\"unterminated,text/css",
    ] {
        assert_eq!(
            fetch_single_case(200, &[("Content-Type", explicit_non_css)], "a{}"),
            Err(StyleFetchRejection::MimeNotCss),
            "explicit non-CSS list must not degrade to unknown: {explicit_non_css}"
        );
    }
}

#[test]
fn firefox_legacy_content_type_fallback_uses_only_the_latest_original_field() {
    assert_eq!(
        fetch_single_case(200, &[("Content-Type", "text/html garbage")], "a{}"),
        Err(StyleFetchRejection::MimeNotCss),
        "legacy fallback must preserve an explicit non-CSS type"
    );

    let css = fetch_single_case(
        200,
        &[
            ("Content-Type", "text/css garbage"),
            ("X-Content-Type-Options", "nosniff"),
        ],
        "a{}",
    )
    .expect("legacy fallback must recover text/css before nosniff admission");
    assert_eq!(css.mime, StyleFetchMime::Css);
    assert_eq!(
        css.content_type.as_deref(),
        Some(b"text/css garbage".as_slice())
    );
    assert!(css.nosniff);

    let latest_css = fetch_single_case(
        200,
        &[
            ("Content-Type", "text/html garbage"),
            ("Content-Type", "text/css garbage"),
            ("X-Content-Type-Options", "nosniff"),
        ],
        "a{}",
    )
    .expect("fallback must parse the latest original field");
    assert_eq!(latest_css.mime, StyleFetchMime::Css);
    assert_eq!(
        latest_css.content_type.as_deref(),
        Some(b"text/html garbage,text/css garbage".as_slice())
    );

    assert_eq!(
        fetch_single_case(
            200,
            &[
                ("Content-Type", "text/css garbage"),
                ("Content-Type", "text/html garbage"),
            ],
            "a{}",
        ),
        Err(StyleFetchRejection::MimeNotCss),
        "fallback must not reuse an earlier original field"
    );

    let modern_wins = fetch_single_case(
        200,
        &[
            ("Content-Type", "text/css"),
            ("Content-Type", "text/html garbage"),
            ("X-Content-Type-Options", "nosniff"),
        ],
        "a{}",
    )
    .expect("a successful merged extraction must suppress legacy fallback");
    assert_eq!(modern_wins.mime, StyleFetchMime::Css);
}

#[test]
fn modern_content_type_retains_leading_unquoted_charset_whitespace() {
    for (value, expected_charset) in [
        ("text/css;charset= utf-8", b" utf-8".as_slice()),
        (r#"text/css;charset= g\"bk"#, b" g\\\"bk".as_slice()),
        (r#"text/css;charset= "g\bk""#, b" \"g\\bk\"".as_slice()),
        ("text/css;charset= utf-8 \t", b" utf-8".as_slice()),
    ] {
        let evidence = fetch_single_case(200, &[("Content-Type", value)], "a{}")
            .unwrap_or_else(|error| panic!("charset case rejected: {error:?}"));
        assert_eq!(evidence.mime, StyleFetchMime::Css, "{value:?}");
        assert_eq!(
            evidence.charset.as_deref(),
            Some(expected_charset),
            "{value:?}"
        );
    }
}

#[test]
fn empty_then_css_content_type_fields_merge_without_a_leading_comma() {
    let fixture = HttpFixture::start(|_| {
        BTreeMap::from([
            (
                "/page".to_owned(),
                document_response("<link rel=stylesheet href=/sheet.css>", &[]),
            ),
            (
                "/sheet.css".to_owned(),
                ResponseSpec::new(
                    200,
                    &[("Content-Type", ""), ("Content-Type", "text/css")],
                    "a{}",
                ),
            ),
        ])
    });
    let plan = plan_from_http(&fixture);
    let set = default_fetch(&mut owner(&plan, StyleFetchLimits::default()), &plan);
    assert_eq!(
        set.responses()[0].headers().content_type(),
        Some(b"text/css".as_slice())
    );
    assert_eq!(set.responses()[0].headers().charset(), None);
    assert_eq!(set.aggregate_header_bytes(), b"text/css".len());
}

#[test]
fn xcto_uses_only_the_first_merged_comma_value_in_wire_order() {
    for (headers, expected) in [
        (
            vec![
                ("Content-Type", "text/css"),
                ("X-Content-Type-Options", "  NoSniff , foo"),
            ],
            true,
        ),
        (
            vec![
                ("Content-Type", "text/css"),
                ("X-Content-Type-Options", "foo, nosniff"),
            ],
            false,
        ),
        (
            vec![
                ("Content-Type", "text/css"),
                ("X-Content-Type-Options", "foo"),
                ("X-Content-Type-Options", "nosniff"),
            ],
            false,
        ),
        (
            vec![
                ("Content-Type", "text/css"),
                ("X-Content-Type-Options", "nosniff"),
                ("X-Content-Type-Options", "foo"),
            ],
            true,
        ),
    ] {
        let evidence = fetch_single_case(200, &headers, "a{}").expect("CSS response admitted");
        assert_eq!(evidence.nosniff, expected, "headers: {headers:?}");
    }

    assert_eq!(
        fetch_single_mime(200, &[("X-Content-Type-Options", "nosniff,foo")], "a{}"),
        Err(StyleFetchRejection::NoSniff)
    );
    assert_eq!(
        fetch_single_mime(200, &[("X-Content-Type-Options", "foo,nosniff")], "a{}"),
        Ok(StyleFetchMime::Unknown)
    );
}

#[test]
fn redirect_targets_reject_scheme_credentials_loops_and_unsupported_status() {
    for (response, expected, secret) in [
        (
            ResponseSpec::redirect(302, "data:text/css,a{}"),
            StyleFetchRejection::RedirectScheme,
            "data:text",
        ),
        (
            ResponseSpec::redirect(302, "http://user:password@127.0.0.1/private.css"),
            StyleFetchRejection::RedirectCredentials,
            "password",
        ),
        (
            ResponseSpec::redirect(302, "/start.css"),
            StyleFetchRejection::RedirectLoop,
            "start.css",
        ),
        (
            ResponseSpec::new(305, &[("Location", "/final.css")], []),
            StyleFetchRejection::UnsupportedRedirectStatus,
            "final.css",
        ),
    ] {
        let fixture = HttpFixture::start(|_| {
            BTreeMap::from([
                (
                    "/page".to_owned(),
                    document_response("<link rel=stylesheet href=/start.css>", &[]),
                ),
                ("/start.css".to_owned(), response),
            ])
        });
        let plan = plan_from_http(&fixture);
        let failure = owner(&plan, StyleFetchLimits::default())
            .fetch_plan(
                &plan,
                &CancellationSource::new().token(),
                Instant::now() + TEST_TIMEOUT,
            )
            .expect_err("redirect case must fail closed");
        assert_eq!(failure.error().rejection(), expected);
        assert!(!format!("{failure:?}").contains(secret));
        assert!(!failure.to_string().contains(secret));
    }
}

#[test]
fn repeated_location_accepts_identical_values_and_rejects_conflicts() {
    let identical = HttpFixture::start(|_| {
        BTreeMap::from([
            (
                "/page".to_owned(),
                document_response("<link rel=stylesheet href=/start.css>", &[]),
            ),
            (
                "/start.css".to_owned(),
                ResponseSpec::new(
                    302,
                    &[("Location", " /final.css "), ("Location", "/final.css")],
                    [],
                ),
            ),
            ("/final.css".to_owned(), ResponseSpec::css("a{}")),
        ])
    });
    let plan = plan_from_http(&identical);
    let set = default_fetch(&mut owner(&plan, StyleFetchLimits::default()), &plan);
    assert_eq!(set.responses()[0].redirect_count(), 1);
    assert_eq!(identical.request_count("/final.css"), 1);

    let conflicting = HttpFixture::start(|_| {
        BTreeMap::from([
            (
                "/page".to_owned(),
                document_response("<link rel=stylesheet href=/start.css>", &[]),
            ),
            (
                "/start.css".to_owned(),
                ResponseSpec::new(
                    302,
                    &[("Location", "/first.css"), ("Location", "/second.css")],
                    [],
                ),
            ),
            ("/first.css".to_owned(), ResponseSpec::css("a{}")),
            ("/second.css".to_owned(), ResponseSpec::css("b{}")),
        ])
    });
    let plan = plan_from_http(&conflicting);
    let failure = owner(&plan, StyleFetchLimits::default())
        .fetch_plan(
            &plan,
            &CancellationSource::new().token(),
            Instant::now() + TEST_TIMEOUT,
        )
        .expect_err("differing Location fields must fail closed");
    assert_eq!(
        failure.error().rejection(),
        StyleFetchRejection::RedirectLocationConflict
    );
    assert_eq!(conflicting.request_count("/first.css"), 0);
    assert_eq!(conflicting.request_count("/second.css"), 0);
}

#[test]
fn cancellation_and_deadline_stop_without_returning_response_state() {
    let fixture = HttpFixture::start(|_| {
        BTreeMap::from([
            (
                "/page".to_owned(),
                document_response("<link rel=stylesheet href=/slow.css>", &[]),
            ),
            (
                "/slow.css".to_owned(),
                ResponseSpec::css("body{color:green}").with_body_delay(Duration::from_millis(250)),
            ),
        ])
    });
    let pre_cancelled = plan_from_http(&fixture);
    let mut pre_cancelled_owner = owner(&pre_cancelled, StyleFetchLimits::default());
    let cancelled = CancellationSource::new();
    assert!(cancelled.cancel());
    let failure = pre_cancelled_owner
        .fetch_plan(
            &pre_cancelled,
            &cancelled.token(),
            Instant::now() + TEST_TIMEOUT,
        )
        .expect_err("pre-cancelled transaction must fail");
    assert_eq!(failure.error().rejection(), StyleFetchRejection::Cancelled);
    assert_eq!(fixture.request_count("/slow.css"), 0);
    let replay = pre_cancelled_owner
        .fetch_plan(
            &pre_cancelled,
            &CancellationSource::new().token(),
            Instant::now() + TEST_TIMEOUT,
        )
        .expect_err("pre-cancelled transaction must consume its issuance");
    assert_eq!(
        replay.error().rejection(),
        StyleFetchRejection::TransactionConsumed
    );
    assert_eq!(fixture.request_count("/slow.css"), 0);

    let expired = plan_from_http(&fixture);
    let failure = owner(&expired, StyleFetchLimits::default())
        .fetch_plan(&expired, &CancellationSource::new().token(), Instant::now())
        .expect_err("expired transaction must fail");
    assert_eq!(
        failure.error().rejection(),
        StyleFetchRejection::DeadlineExceeded
    );
    assert_eq!(fixture.request_count("/slow.css"), 0);

    let body_deadline = plan_from_http(&fixture);
    let failure = owner(&body_deadline, StyleFetchLimits::default())
        .fetch_plan(
            &body_deadline,
            &CancellationSource::new().token(),
            Instant::now() + Duration::from_millis(40),
        )
        .expect_err("body deadline must fail");
    assert_eq!(
        failure.error().rejection(),
        StyleFetchRejection::DeadlineExceeded
    );

    let planned = plan_from_http(&fixture);
    let fetch_owner = owner(&planned, StyleFetchLimits::default());
    let PlannedStyleFetch { plan, engine } = planned;
    let plan = Arc::new(plan);
    let cancellation = CancellationSource::new();
    let token = cancellation.token();
    let thread_plan = Arc::clone(&plan);
    let worker = thread::spawn(move || {
        let mut fetch_owner = fetch_owner;
        let result = fetch_owner.fetch_plan(&thread_plan, &token, Instant::now() + TEST_TIMEOUT);
        (fetch_owner, result)
    });
    let wait_deadline = Instant::now() + TEST_TIMEOUT;
    while fixture.request_count("/slow.css") < 2 && Instant::now() < wait_deadline {
        thread::sleep(Duration::from_millis(2));
    }
    assert!(cancellation.cancel());
    let (mut fetch_owner, result) = worker.join().expect("join cancelled fetch");
    let failure = result.expect_err("in-flight cancellation must fail");
    assert_eq!(failure.error().rejection(), StyleFetchRejection::Cancelled);
    let replay = fetch_owner
        .fetch_plan(
            &plan,
            &CancellationSource::new().token(),
            Instant::now() + TEST_TIMEOUT,
        )
        .expect_err("in-flight cancellation must consume its issuance");
    assert_eq!(
        replay.error().rejection(),
        StyleFetchRejection::TransactionConsumed
    );
    drop(engine);
}

#[test]
fn exact_owner_is_exclusive_and_rejects_deadlines_beyond_the_hard_horizon() {
    let fixture = HttpFixture::start(|_| {
        BTreeMap::from([
            (
                "/page".to_owned(),
                document_response("<link rel=stylesheet href=/sheet.css>", &[]),
            ),
            ("/sheet.css".to_owned(), ResponseSpec::css("a{}")),
        ])
    });
    let over_horizon = plan_from_http(&fixture);
    let mut over_horizon_owner = owner(&over_horizon, StyleFetchLimits::default());

    let failure = over_horizon_owner
        .fetch_plan(
            &over_horizon,
            &CancellationSource::new().token(),
            Instant::now() + MAX_STYLE_FETCH_DURATION + Duration::from_secs(1),
        )
        .expect_err("deadline beyond hard horizon must fail before network access");
    assert_eq!(
        failure.error().rejection(),
        StyleFetchRejection::DeadlineTooFar
    );
    assert_eq!(fixture.request_count("/sheet.css"), 0);
    let replay = over_horizon_owner
        .fetch_plan(
            &over_horizon,
            &CancellationSource::new().token(),
            Instant::now() + TEST_TIMEOUT,
        )
        .expect_err("an invalid first transaction still consumes the ledger");
    assert_eq!(
        replay.error().rejection(),
        StyleFetchRejection::TransactionConsumed
    );
    assert_eq!(fixture.request_count("/sheet.css"), 0);

    let plan = plan_from_http(&fixture);
    let mut fetch_owner = owner(&plan, StyleFetchLimits::default());
    let first = default_fetch(&mut fetch_owner, &plan);
    assert_eq!(first.responses()[0].body(), b"a{}");
    let replay = fetch_owner
        .fetch_plan(
            &plan,
            &CancellationSource::new().token(),
            Instant::now() + TEST_TIMEOUT,
        )
        .expect_err("a successful transaction cannot be replayed");
    assert_eq!(
        replay.error().rejection(),
        StyleFetchRejection::TransactionConsumed
    );
    assert_eq!(fixture.request_count("/sheet.css"), 1);
}

#[test]
fn response_and_aggregate_limits_fail_transactionally() {
    let fixture = HttpFixture::start(|_| {
        BTreeMap::from([
            (
                "/page".to_owned(),
                document_response(
                    "<link rel=stylesheet href=/one.css><link rel=stylesheet href=/two.css>",
                    &[],
                ),
            ),
            ("/one.css".to_owned(), ResponseSpec::css("1234")),
            ("/two.css".to_owned(), ResponseSpec::css("5678")),
        ])
    });
    let plan = plan_from_http(&fixture);

    let count_limits = StyleFetchLimits::new(1, 4, 8, 2, 4, 8).expect("lower count limits");
    let failure = owner(&plan, count_limits)
        .fetch_plan(
            &plan,
            &CancellationSource::new().token(),
            Instant::now() + TEST_TIMEOUT,
        )
        .expect_err("response count must reject before fetching");
    assert_eq!(
        failure.error().rejection(),
        StyleFetchRejection::Limit(StyleFetchLimit::Responses)
    );
    assert_eq!(fixture.request_count("/one.css"), 0);

    let plan = plan_from_http(&fixture);
    let aggregate_limits = StyleFetchLimits::new(2, 4, 7, 2, 4, 8).expect("lower aggregate limits");
    let failure = owner(&plan, aggregate_limits)
        .fetch_plan(
            &plan,
            &CancellationSource::new().token(),
            Instant::now() + TEST_TIMEOUT,
        )
        .expect_err("second body must reject complete transaction");
    assert_eq!(failure.error().request_index(), 1);
    assert_eq!(
        failure.error().rejection(),
        StyleFetchRejection::Limit(StyleFetchLimit::AggregateBodyBytes)
    );
    assert_eq!(fixture.request_count("/one.css"), 1);
    assert_eq!(fixture.request_count("/two.css"), 1);
    assert!(!format!("{failure:?}").contains("1234"));

    let plan = plan_from_http(&fixture);
    let response_limits = StyleFetchLimits::new(2, 3, 7, 2, 4, 8).expect("lower response limits");
    let failure = owner(&plan, response_limits)
        .fetch_plan(
            &plan,
            &CancellationSource::new().token(),
            Instant::now() + TEST_TIMEOUT,
        )
        .expect_err("first body must exceed per-response limit");
    assert_eq!(failure.error().request_index(), 0);
    assert_eq!(
        failure.error().rejection(),
        StyleFetchRejection::Limit(StyleFetchLimit::ResponseBodyBytes)
    );
}

#[test]
fn redirect_and_exchange_limits_are_exact_while_diagnostics_are_lossy() {
    let fixture = HttpFixture::start(|_| {
        BTreeMap::from([
            (
                "/page".to_owned(),
                document_response(
                    "<link rel=stylesheet href=/one.css>",
                    &[("Content-Security-Policy", "style-src 'self'")],
                ),
            ),
            (
                "/one.css".to_owned(),
                ResponseSpec::redirect(302, "/two.css"),
            ),
            (
                "/two.css".to_owned(),
                ResponseSpec::redirect(302, "/three.css"),
            ),
            ("/three.css".to_owned(), ResponseSpec::css("a{}")),
        ])
    });
    let plan = plan_from_http(&fixture);

    let redirect_limits = StyleFetchLimits::new(1, 16, 16, 1, 3, 4).expect("one redirect limit");
    let failure = owner(&plan, redirect_limits)
        .fetch_plan(
            &plan,
            &CancellationSource::new().token(),
            Instant::now() + TEST_TIMEOUT,
        )
        .expect_err("second redirect must exceed limit");
    assert_eq!(
        failure.error().rejection(),
        StyleFetchRejection::Limit(StyleFetchLimit::Redirects)
    );
    assert_eq!(fixture.request_count("/three.css"), 0);

    let plan = plan_from_http(&fixture);
    let exchange_limits = StyleFetchLimits::new(1, 16, 16, 2, 1, 4).expect("one exchange limit");
    let before_two = fixture.request_count("/two.css");
    let failure = owner(&plan, exchange_limits)
        .fetch_plan(
            &plan,
            &CancellationSource::new().token(),
            Instant::now() + TEST_TIMEOUT,
        )
        .expect_err("second exchange must fail before connection");
    assert_eq!(
        failure.error().rejection(),
        StyleFetchRejection::Limit(StyleFetchLimit::HttpExchanges)
    );
    assert_eq!(fixture.request_count("/two.css"), before_two);

    let plan = plan_from_http(&fixture);
    let diagnostic_limits =
        StyleFetchLimits::new(1, 16, 16, 2, 3, 0).expect("zero diagnostic limit");
    let before_two = fixture.request_count("/two.css");
    let set = owner(&plan, diagnostic_limits)
        .fetch_plan(
            &plan,
            &CancellationSource::new().token(),
            Instant::now() + TEST_TIMEOUT,
        )
        .expect("diagnostic capacity must not control transaction semantics");
    assert_eq!(set.responses()[0].body(), b"a{}");
    assert!(set.diagnostics().is_empty());
    assert_eq!(fixture.request_count("/two.css"), before_two + 1);
    assert_eq!(fixture.request_count("/three.css"), 1);
}

#[test]
fn hard_limits_and_transport_policy_cannot_be_enlarged() {
    let fixture = HttpFixture::start(|_| {
        BTreeMap::from([(
            "/page".to_owned(),
            document_response("<p>authority</p>", &[]),
        )])
    });
    let plan = plan_from_http(&fixture);
    let direct_navigation = NavigationId::new(
        TopLevelContextId::new(1).expect("nonzero context"),
        NavigationGeneration::INITIAL,
    );
    assert!(matches!(
        plan.engine.delegate_style_fetch_authority(
            direct_navigation,
            StyleFetchTransportPolicy::default()
        ),
        Err(StyleFetchOwnerError::ProductNavigationRequired)
    ));
    let authority = plan
        .engine
        .delegate_non_product_style_fetch_authority(StyleFetchTransportPolicy::default())
        .expect("delegate constructor authority");
    assert!(matches!(
        plan.engine
            .delegate_non_product_style_fetch_authority(StyleFetchTransportPolicy::default()),
        Err(StyleFetchOwnerError::AuthorityAlreadyIssued)
    ));
    assert!(
        NonProductStyleFetchOwner::new(authority, StyleFetchLimits::default()).is_ok(),
        "the exact delegated constructor contract must remain usable"
    );

    assert_eq!(
        StyleFetchLimits::new(
            MAX_STYLE_FETCH_RESPONSES + 1,
            MAX_STYLE_FETCH_RESPONSE_BODY_BYTES,
            MAX_STYLE_FETCH_AGGREGATE_BODY_BYTES,
            MAX_STYLE_FETCH_REDIRECTS,
            MAX_STYLE_FETCH_HTTP_EXCHANGES,
            MAX_STYLE_FETCH_DIAGNOSTICS,
        ),
        Err(StyleFetchOwnerError::LimitWouldEnlarge(
            StyleFetchLimit::Responses
        ))
    );
    assert_eq!(
        StyleFetchTransportPolicy::new(MAX_STYLE_FETCH_CHUNK_LINE_BYTES + 1),
        Err(StyleFetchOwnerError::LimitWouldEnlarge(
            StyleFetchLimit::WireChunkLineBytes
        ))
    );
    assert_eq!(
        StyleFetchTransportPolicy::new(MAX_STYLE_FETCH_CHUNK_LINE_BYTES)
            .expect("exact hard transport limit")
            .max_chunk_line_bytes(),
        MAX_STYLE_FETCH_CHUNK_LINE_BYTES
    );
}

#[test]
fn unterminated_chunk_size_line_hits_the_sealed_owner_bound() {
    let oversized_line = vec![b'a'; MAX_STYLE_FETCH_CHUNK_LINE_BYTES + 2];
    let fixture = HttpFixture::start(move |_| {
        BTreeMap::from([
            (
                "/page".to_owned(),
                document_response("<link rel=stylesheet href=/chunked.css>", &[]),
            ),
            (
                "/chunked.css".to_owned(),
                ResponseSpec::new(
                    200,
                    &[
                        ("Content-Type", "text/css"),
                        ("Transfer-Encoding", "chunked"),
                    ],
                    oversized_line,
                ),
            ),
        ])
    });
    let plan = plan_from_http(&fixture);
    let failure = owner(&plan, StyleFetchLimits::default())
        .fetch_plan(
            &plan,
            &CancellationSource::new().token(),
            Instant::now() + TEST_TIMEOUT,
        )
        .expect_err("over-limit unterminated chunk line must reject the transaction");
    assert_eq!(
        failure.error().rejection(),
        StyleFetchRejection::Network(StyleFetchNetworkFailure::ResourceLimit)
    );
    assert_eq!(fixture.request_count("/chunked.css"), 1);
    assert_eq!(failure.diagnostics().len(), 1);
    assert!(matches!(
        failure.diagnostics()[0].kind(),
        StyleFetchDiagnosticKind::Rejected(StyleFetchRejection::Network(
            StyleFetchNetworkFailure::ResourceLimit
        ))
    ));
}

struct TlsFixture {
    directory: PathBuf,
    certificate_der: Vec<u8>,
    origin: String,
    child: Option<Child>,
}

impl TlsFixture {
    fn start(accepts: usize, build_files: impl FnOnce(&str) -> Vec<(String, Vec<u8>)>) -> Self {
        let directory = unique_tls_directory();
        fs::create_dir_all(&directory).expect("create TLS fixture directory");
        let certificate_pem = directory.join("certificate.pem");
        let certificate_der_path = directory.join("certificate.der");
        let private_key = directory.join("private-key.pem");
        let address = reserve_address();
        let origin = format!("https://localhost:{}", address.port());

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
            .expect("generate TLS certificate");
        assert_command_success("openssl req", &output);
        let output = Command::new("openssl")
            .args(["x509", "-in"])
            .arg(&certificate_pem)
            .args(["-outform", "DER", "-out"])
            .arg(&certificate_der_path)
            .output()
            .expect("convert TLS certificate");
        assert_command_success("openssl x509", &output);

        for (name, response) in build_files(&origin) {
            fs::write(directory.join(name), response).expect("write TLS response file");
        }
        let accepts = accepts.to_string();
        let mut child = Command::new("openssl")
            .args([
                "s_server", "-quiet", "-HTTP", "-tls1_3", "-alpn", "http/1.1", "-accept",
            ])
            .arg(address.to_string())
            .arg("-cert")
            .arg(&certificate_pem)
            .arg("-key")
            .arg(&private_key)
            .args(["-naccept", &accepts])
            .current_dir(&directory)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn TLS fixture");
        wait_for_listener(&mut child, address);
        Self {
            directory,
            certificate_der: fs::read(certificate_der_path).expect("read DER certificate"),
            origin,
            child: Some(child),
        }
    }

    fn trust_store(&self) -> TrustStore {
        TrustStore::bundled_web_pki()
            .with_der_certificate(&self.certificate_der)
            .expect("admit TLS fixture root")
    }

    fn finish(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        let deadline = Instant::now() + TEST_TIMEOUT;
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

impl Drop for TlsFixture {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = fs::remove_dir_all(&self.directory);
    }
}

fn tls_response(status: u16, headers: &[(&str, &str)], body: &str) -> Vec<u8> {
    let reason = if status == 200 { "OK" } else { "Found" };
    let mut response = format!("HTTP/1.1 {status} {reason}\r\n").into_bytes();
    for (name, value) in headers {
        response.extend_from_slice(name.as_bytes());
        response.extend_from_slice(b": ");
        response.extend_from_slice(value.as_bytes());
        response.extend_from_slice(b"\r\n");
    }
    response.extend_from_slice(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes());
    response.extend_from_slice(body.as_bytes());
    response
}

fn plan_from_tls(fixture: &TlsFixture) -> PlannedStyleFetch {
    let mut engine = StaticPageEngine::new_general_web_for_presentation(
        engine_config(),
        general_web_config(),
        fixture.trust_store(),
    )
    .expect("create TLS document engine");
    let url = format!("{}/page.html", fixture.origin);
    engine
        .load_general_web_for_presentation(&url, &CancellationSource::new().token())
        .expect("load TLS document fixture");
    let plan = StyleResourcePlan::from_live_document(
        engine.live_document().expect("retained TLS document"),
    )
    .expect("construct TLS stylesheet plan");
    PlannedStyleFetch { plan, engine }
}

#[test]
fn authenticated_tls_fetches_and_trustworthy_loopback_redirects_are_admitted() {
    let mut admitted = TlsFixture::start(2, |_| {
        vec![
            (
                "page.html".to_owned(),
                tls_response(
                    200,
                    &[("Content-Type", "text/html")],
                    &document("<link rel=stylesheet href=/sheet.css>"),
                ),
            ),
            (
                "sheet.css".to_owned(),
                tls_response(
                    200,
                    &[
                        ("Content-Type", "text/css"),
                        ("X-Content-Type-Options", "nosniff"),
                    ],
                    "body{color:navy}",
                ),
            ),
        ]
    });
    let plan = plan_from_tls(&admitted);
    let mut fetch_owner = owner(&plan, StyleFetchLimits::default());
    let set = default_fetch(&mut fetch_owner, &plan);
    assert_eq!(set.responses()[0].body(), b"body{color:navy}");
    assert!(matches!(
        set.responses()[0].security(),
        wild_buzzard_net::ConnectionSecurity::Tls { .. }
    ));
    admitted.finish();

    let cleartext = HttpFixture::start(|_| {
        BTreeMap::from([(
            "/loopback.css".to_owned(),
            ResponseSpec::css("body{color:olive}"),
        )])
    });
    let cleartext_location = format!("http://localhost:{}/loopback.css", cleartext.address.port());
    let mut loopback = TlsFixture::start(2, |_| {
        vec![
            (
                "page.html".to_owned(),
                tls_response(
                    200,
                    &[("Content-Type", "text/html")],
                    &document("<link rel=stylesheet href=/redirect.css>"),
                ),
            ),
            (
                "redirect.css".to_owned(),
                tls_response(302, &[("Location", &cleartext_location)], ""),
            ),
        ]
    });
    let plan = plan_from_tls(&loopback);
    let mut fetch_owner = owner(&plan, StyleFetchLimits::default());
    let set = default_fetch(&mut fetch_owner, &plan);
    assert_eq!(set.responses()[0].body(), b"body{color:olive}");
    assert_eq!(
        set.responses()[0].origin_cleanliness(),
        StyleFetchOriginCleanliness::Tainted
    );
    assert_eq!(cleartext.request_count("/loopback.css"), 1);
    loopback.finish();
}

#[test]
fn nonloopback_and_ipv4_mapped_cleartext_redirects_never_connect() {
    let cleartext_probe = TcpListener::bind("0.0.0.0:0").expect("bind cleartext probe");
    cleartext_probe
        .set_nonblocking(true)
        .expect("arm cleartext probe");
    let port = cleartext_probe
        .local_addr()
        .expect("read probe address")
        .port();

    for (label, cleartext_location) in [
        (
            "nonloopback",
            format!("http://0.0.0.0:{port}/must-not-connect.css"),
        ),
        (
            "ipv4-mapped-ipv6",
            format!("http://[::ffff:127.0.0.1]:{port}/must-not-connect.css"),
        ),
    ] {
        let mut mixed = TlsFixture::start(2, |_| {
            vec![
                (
                    "page.html".to_owned(),
                    tls_response(
                        200,
                        &[("Content-Type", "text/html")],
                        &document("<link rel=stylesheet href=/redirect.css>"),
                    ),
                ),
                (
                    "redirect.css".to_owned(),
                    tls_response(302, &[("Location", &cleartext_location)], ""),
                ),
            ]
        });
        let plan = plan_from_tls(&mixed);
        let mut fetch_owner = owner(&plan, StyleFetchLimits::default());
        let failure = fetch_owner
            .fetch_plan(
                &plan,
                &CancellationSource::new().token(),
                Instant::now() + TEST_TIMEOUT,
            )
            .expect_err("untrustworthy cleartext redirect must fail before connection");
        assert_eq!(
            failure.error().rejection(),
            StyleFetchRejection::MixedContent,
            "{label}"
        );
        assert!(
            matches!(
                cleartext_probe.accept(),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock
            ),
            "{label} unexpectedly connected"
        );
        mixed.finish();
    }
}

fn unique_tls_directory() -> PathBuf {
    let base = std::env::var_os("CARGO_TARGET_DIR").map_or_else(std::env::temp_dir, PathBuf::from);
    let sequence = NEXT_TLS_FIXTURE.fetch_add(1, Ordering::Relaxed);
    base.join(format!(
        "wild-buzzard-style-fetch-tls-{}-{sequence}",
        std::process::id()
    ))
}

fn reserve_address() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("reserve TLS address");
    listener.local_addr().expect("read TLS address")
}

fn wait_for_listener(child: &mut Child, address: SocketAddr) {
    let deadline = Instant::now() + TEST_TIMEOUT;
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

fn assert_command_success(name: &str, output: &std::process::Output) {
    assert!(
        output.status.success(),
        "{name} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
