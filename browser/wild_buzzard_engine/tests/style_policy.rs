use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use wild_buzzard_engine::{
    CancellationSource, FontSourcePolicy, MAX_STYLE_CSP_NONCE_BYTES, MAX_STYLE_CSP_POLICY_MEMBERS,
    StaticPageConfig, StaticPageEngine, StylePolicyError, StylePolicyInput, StylePolicyLimit,
    StylePolicyResource, StylePolicySet, UnsupportedStyleSourceKind,
};
use wild_buzzard_headless::HeadlessLimits;
use wild_buzzard_net::ClientConfig;

const SERVER_TIMEOUT: Duration = Duration::from_secs(3);
const PAGE: &[u8] = br#"<!doctype html>
<style>
  html, body { margin: 0; background: rgb(23 48 73); }
  #panel { display: block; width: 720px; height: 240px;
    background: rgb(229 238 247); color: rgb(18 43 69); }
</style><main id="panel">Wild Buzzard pure CSP style policy</main>"#;

fn network_config() -> ClientConfig {
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
        network: network_config(),
        font_source: FontSourcePolicy::EmbeddedOnly,
        headless: HeadlessLimits::default()
            .with_max_width(width)
            .with_max_height(height)
            .with_max_pixel_bytes(pixel_bytes),
        ..StaticPageConfig::default()
    }
}

fn response(fields: &[(&str, &[u8])]) -> Vec<u8> {
    let mut response = b"HTTP/1.1 200 OK\r\n".to_vec();
    for (name, value) in fields {
        response.extend_from_slice(name.as_bytes());
        response.extend_from_slice(b": ");
        response.extend_from_slice(value);
        response.extend_from_slice(b"\r\n");
    }
    response.extend_from_slice(format!("Content-Length: {}\r\n", PAGE.len()).as_bytes());
    response.extend_from_slice(b"Connection: close\r\n\r\n");
    response.extend_from_slice(PAGE);
    response
}

fn read_request_head(stream: &mut TcpStream) {
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
    assert!(head.starts_with(b"GET /page HTTP/1.1\r\n"));
}

fn serve_once(fields: &[(&str, &[u8])]) -> (String, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let response = response(fields);
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.set_read_timeout(Some(SERVER_TIMEOUT)).unwrap();
        stream.set_write_timeout(Some(SERVER_TIMEOUT)).unwrap();
        read_request_head(&mut stream);
        stream.write_all(&response).unwrap();
    });
    (format!("http://{address}"), handle)
}

fn load_policy(fields: &[(&str, &[u8])]) -> (StylePolicySet, String) {
    let (origin, server) = serve_once(fields);
    let mut engine = StaticPageEngine::new(config(640, 360)).unwrap();
    engine
        .load(
            &format!("{origin}/page"),
            &CancellationSource::new().token(),
        )
        .unwrap();
    server.join().unwrap();
    let policies = StylePolicySet::from_response_metadata(
        engine.live_document().unwrap().captured_response_metadata(),
    )
    .unwrap();
    engine.shutdown().unwrap();
    (policies, origin)
}

#[test]
fn separate_field_lists_duplicate_first_and_report_only_intersect_without_merging() {
    let (policies, origin) = load_policy(&[
        (
            "Content-Security-Policy",
            b"STYLE-SRC-ELEM 'self'; style-src-elem *,,default-src * ,",
        ),
        ("content-security-policy", b"style-src *"),
        (
            "Content-Security-Policy-Report-Only",
            b"style-src-elem 'none'",
        ),
    ]);
    assert_eq!(policies.enforcing_policy_count(), 3);
    assert_eq!(policies.report_only_policy_count(), 1);
    assert_eq!(policies.inspected_policy_member_count(), 5);
    assert_eq!(policies.ignored_duplicate_directive_count(), 1);

    let same_origin = policies
        .evaluate_external_style(&format!("{origin}/sheet.css"), None)
        .unwrap();
    assert!(same_origin.is_allowed());
    assert_eq!(same_origin.resource(), StylePolicyResource::ExternalStyle);
    assert_eq!(same_origin.report_only_would_block_policy_count(), 1);

    let cross_origin = policies
        .evaluate_external_style("https://cross.example/sheet.css", None)
        .unwrap();
    assert!(!cross_origin.is_allowed());
    assert_eq!(cross_origin.enforcing_blocked_policy_count(), 1);
}

#[test]
fn directive_fallback_and_base_uri_no_fallback_are_independent() {
    let (policies, origin) = load_policy(&[(
        "Content-Security-Policy",
        b"default-src 'none'; style-src 'unsafe-inline'; style-src-elem https: 'unsafe-inline'; style-src-attr 'none'",
    )]);
    assert!(
        policies
            .evaluate_external_style("https://cdn.example/sheet.css", None)
            .unwrap()
            .is_allowed()
    );
    assert!(
        policies
            .evaluate_inline_style_element(None)
            .unwrap()
            .is_allowed()
    );
    assert!(
        !policies
            .evaluate_inline_style_attribute()
            .unwrap()
            .is_allowed()
    );
    assert!(
        policies
            .evaluate_base_uri(&format!("{origin}/new-base/"))
            .unwrap()
            .is_allowed(),
        "default-src must not restrict base-uri"
    );
}

#[test]
fn schemes_ports_wildcards_and_address_kinds_use_canonical_url_matching() {
    let (policies, _) = load_policy(&[(
        "Content-Security-Policy",
        b"style-src http://upgrade.example:80 https://secure.example *.cdn.example:* 127.0.0.1 [::1]",
    )]);
    for candidate in [
        "https://upgrade.example/a.css",
        "https://secure.example/a.css",
        "http://assets.cdn.example:8080/a.css",
        "http://127.0.0.1/a.css",
    ] {
        assert!(
            policies
                .evaluate_external_style(candidate, None)
                .unwrap()
                .is_allowed(),
            "expected allowed: {candidate}"
        );
    }
    for candidate in [
        "http://secure.example/a.css",
        "http://cdn.example/a.css",
        "http://127.0.0.2/a.css",
        "http://[::1]/a.css",
        "http://[::2]/a.css",
    ] {
        assert!(
            !policies
                .evaluate_external_style(candidate, None)
                .unwrap()
                .is_allowed(),
            "expected blocked: {candidate}"
        );
    }
    assert_eq!(
        policies.evaluate_external_style("HTTP://SECURE.EXAMPLE/a.css", None),
        Err(StylePolicyError::NonCanonicalCandidateUrl)
    );
    assert_eq!(
        policies.evaluate_external_style("http://user@secure.example/a.css", None),
        Err(StylePolicyError::InvalidCandidateUrl)
    );
}

#[test]
fn pinned_firefox_port_80_to_443_quirk_has_exact_nonupgrade_controls() {
    let (policies, _) = load_policy(&[(
        "Content-Security-Policy",
        b"style-src https://quirk.example:80",
    )]);
    assert!(
        policies
            .evaluate_external_style("https://quirk.example/a.css", None)
            .unwrap()
            .is_allowed()
    );
    for candidate in [
        "http://quirk.example/a.css",
        "https://quirk.example:444/a.css",
    ] {
        assert!(
            !policies
                .evaluate_external_style(candidate, None)
                .unwrap()
                .is_allowed(),
            "expected non-upgrade control to remain blocked: {candidate}"
        );
    }

    let (leading_zero, _) = load_policy(&[(
        "Content-Security-Policy",
        b"style-src https://zero.example:000443",
    )]);
    assert!(
        leading_zero
            .evaluate_external_style("https://zero.example/a.css", None)
            .unwrap()
            .is_allowed(),
        "numeric source-port normalization is a deliberate standards-forward divergence from ESR153"
    );
}

#[test]
fn link_and_style_element_nonces_match_but_never_admit_style_attributes() {
    let (policies, _) = load_policy(&[
        (
            "Content-Security-Policy",
            b"style-src 'nonce-TopSecret+/nonce=' 'unsafe-inline'",
        ),
        ("Content-Security-Policy-Report-Only", b"style-src 'none'"),
    ]);
    let external = policies
        .evaluate_external_style(
            "http://otherwise-blocked.example/a.css",
            Some("TopSecret+/nonce="),
        )
        .unwrap();
    assert!(external.is_allowed());
    assert!(external.report_only_would_block());
    assert!(
        policies
            .evaluate_inline_style_element(Some("TopSecret+/nonce="))
            .unwrap()
            .is_allowed()
    );
    assert!(
        !policies
            .evaluate_inline_style_element(Some("topsecret+/nonce="))
            .unwrap()
            .is_allowed()
    );
    assert!(
        !policies
            .evaluate_inline_style_attribute()
            .unwrap()
            .is_allowed()
    );
    let overlong_nonce = "n".repeat(MAX_STYLE_CSP_NONCE_BYTES + 1);
    let overlong = policies
        .evaluate_external_style(
            "http://otherwise-blocked.example/a.css",
            Some(&overlong_nonce),
        )
        .unwrap();
    assert!(!overlong.is_allowed());
    assert!(overlong.report_only_would_block());
    assert!(overlong.candidate_nonce_ignored_over_limit());
    let debug = format!("{policies:?}");
    assert!(!debug.contains("TopSecret"));
    assert!(!debug.contains("otherwise-blocked"));
}

#[test]
fn report_only_member_failure_is_transactional_at_captured_metadata_boundary() {
    let report_only = std::iter::repeat_n("style-src *", MAX_STYLE_CSP_POLICY_MEMBERS)
        .collect::<Vec<_>>()
        .join(",");
    let (policies, _) = load_policy(&[
        ("Content-Security-Policy", b"style-src 'none'"),
        (
            "Content-Security-Policy-Report-Only",
            report_only.as_bytes(),
        ),
    ]);
    assert!(matches!(
        policies.report_only_parse_failure(),
        Some(StylePolicyError::LimitExceeded {
            input: StylePolicyInput::Aggregate,
            limit: StylePolicyLimit::PolicyMemberCount,
            actual,
            maximum: MAX_STYLE_CSP_POLICY_MEMBERS,
        }) if actual == MAX_STYLE_CSP_POLICY_MEMBERS + 1
    ));
    assert_eq!(policies.enforcing_policy_count(), 1);
    assert_eq!(policies.report_only_policy_count(), 0);
    assert_eq!(policies.inspected_policy_member_count(), 1);
    let decision = policies.evaluate_inline_style_element(None).unwrap();
    assert!(!decision.is_allowed());
    assert_eq!(decision.enforcing_blocked_policy_count(), 1);
    assert_eq!(decision.report_only_would_block_policy_count(), 0);
}

#[test]
fn unsupported_paths_and_hashes_are_redacted_nonmatching_inputs() {
    let (policies, _) = load_policy(&[(
        "Content-Security-Policy",
        b"style-src 'unsafe-inline' 'sha384-Abc_-' https://private.example/restricted/path",
    )]);
    assert_eq!(policies.unsupported_sources().len(), 2);
    assert_eq!(
        policies.unsupported_sources()[0].kind(),
        UnsupportedStyleSourceKind::Hash
    );
    assert_eq!(
        policies.unsupported_sources()[1].kind(),
        UnsupportedStyleSourceKind::HostPath
    );
    assert!(
        !policies
            .evaluate_inline_style_element(None)
            .unwrap()
            .is_allowed(),
        "a valid hash source disables unsafe-inline even before hash matching exists"
    );
    assert!(
        !policies
            .evaluate_external_style("https://private.example/restricted/path", None)
            .unwrap()
            .is_allowed()
    );
    assert!(!format!("{policies:?}").contains("private"));
}

#[test]
fn inspected_member_count_overflow_fails_typed_instead_of_truncating() {
    let serialized = std::iter::repeat_n("style-src *", MAX_STYLE_CSP_POLICY_MEMBERS + 1)
        .collect::<Vec<_>>()
        .join(",");
    let (origin, server) = serve_once(&[("Content-Security-Policy", serialized.as_bytes())]);
    let mut engine = StaticPageEngine::new(config(640, 360)).unwrap();
    engine
        .load(
            &format!("{origin}/page"),
            &CancellationSource::new().token(),
        )
        .unwrap();
    server.join().unwrap();
    assert!(matches!(
        StylePolicySet::from_response_metadata(
            engine.live_document().unwrap().captured_response_metadata()
        ),
        Err(StylePolicyError::LimitExceeded {
            input: StylePolicyInput::Aggregate,
            limit: StylePolicyLimit::PolicyMemberCount,
            actual,
            maximum: MAX_STYLE_CSP_POLICY_MEMBERS,
        }) if actual == MAX_STYLE_CSP_POLICY_MEMBERS + 1
    ));
    engine.shutdown().unwrap();
}

#[test]
fn disconnected_policy_parser_cannot_change_desktop_frames() {
    for (width, height) in [(1366, 768), (1920, 1080)] {
        let mut engine = StaticPageEngine::new(config(width, height)).unwrap();
        let (plain_origin, plain_server) = serve_once(&[]);
        let plain = engine
            .load(
                &format!("{plain_origin}/page"),
                &CancellationSource::new().token(),
            )
            .unwrap();
        plain_server.join().unwrap();

        let (policy_origin, policy_server) = serve_once(&[
            ("Content-Security-Policy", b"style-src 'none'"),
            (
                "Content-Security-Policy-Report-Only",
                b"style-src 'nonce-never-print'",
            ),
        ]);
        let with_policy = engine
            .load(
                &format!("{policy_origin}/page"),
                &CancellationSource::new().token(),
            )
            .unwrap();
        policy_server.join().unwrap();
        let parsed = StylePolicySet::from_response_metadata(
            engine.live_document().unwrap().captured_response_metadata(),
        )
        .unwrap();
        assert!(
            !parsed
                .evaluate_inline_style_element(None)
                .unwrap()
                .is_allowed()
        );

        assert_eq!(with_policy.frame.size(), plain.frame.size());
        assert_eq!(with_policy.frame.pixels(), plain.frame.pixels());
        assert_eq!(with_policy.frame.size().width(), width);
        assert_eq!(with_policy.frame.size().height(), height);
        assert!(
            with_policy
                .frame
                .pixels()
                .chunks_exact(4)
                .any(|pixel| pixel != [255, 255, 255, 255])
        );
        engine.shutdown().unwrap();
    }
}
