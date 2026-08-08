// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Deterministic loopback integration tests for the HTTP/1.1 nucleus.

use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    sync::mpsc,
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use wild_buzzard_net::{
    BodyFraming, CancellationSource, ClientConfig, Error, HeaderName, HeaderValue, HttpClient,
    LimitKind, LoopbackTarget, Method, Operation, RedirectPolicy, Request,
};

struct TestServer {
    address: SocketAddr,
    thread: JoinHandle<()>,
}

impl TestServer {
    fn join(self) {
        self.thread.join().expect("loopback server must not panic");
    }
}

fn spawn_server(handler: impl FnOnce(TcpStream) + Send + 'static) -> TestServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
    let address = listener.local_addr().expect("read listener address");
    let thread = thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept loopback client");
        handler(stream);
    });
    TestServer { address, thread }
}

fn read_request_head(stream: &mut TcpStream) -> Vec<u8> {
    let mut request = Vec::new();
    let mut byte = [0_u8; 1];
    while !request.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).expect("read request head");
        request.push(byte[0]);
        assert!(
            request.len() <= 16 * 1024,
            "request head unexpectedly large"
        );
    }
    request
}

fn spawn_raw_response(response: impl Into<Vec<u8>>) -> TestServer {
    let response = response.into();
    spawn_server(move |mut stream| {
        let _request = read_request_head(&mut stream);
        stream.write_all(&response).expect("write raw response");
    })
}

fn target(address: SocketAddr, path: &str) -> LoopbackTarget {
    LoopbackTarget::parse(&format!("http://{address}{path}")).expect("valid loopback target")
}

fn idle_listener_and_target(path: &str) -> (TcpListener, LoopbackTarget) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind idle loopback listener");
    listener
        .set_nonblocking(true)
        .expect("make idle listener nonblocking");
    let address = listener.local_addr().expect("read idle listener address");
    (listener, target(address, path))
}

fn assert_no_connection(listener: &TcpListener) {
    match listener.accept() {
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
        Err(error) => panic!("unexpected listener error: {error}"),
        Ok(_) => panic!("request validation unexpectedly opened a TCP connection"),
    }
}

fn get(
    address: SocketAddr,
    config: ClientConfig,
) -> wild_buzzard_net::Result<wild_buzzard_net::Response> {
    HttpClient::new(config).execute(&Request::get(
        target(address, "/resource?key=value"),
        RedirectPolicy::Manual,
    ))
}

#[test]
fn content_length_response_and_request_contract_succeed() {
    let (request_tx, request_rx) = mpsc::channel();
    let server = spawn_server(move |mut stream| {
        let request = read_request_head(&mut stream);
        request_tx.send(request).expect("send captured request");
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nX-Test: yes\r\n\r\nhello")
            .expect("write response");
    });

    let mut response = get(server.address, ClientConfig::default()).expect("request succeeds");
    assert_eq!(response.head().status().as_u16(), 200);
    assert_eq!(
        response.head().body_framing(),
        BodyFraming::ContentLength(5)
    );
    assert_eq!(
        response
            .head()
            .headers()
            .get("x-test")
            .and_then(HeaderValue::to_str),
        Some("yes")
    );
    assert_eq!(
        response.body_mut().read_to_end().expect("read body"),
        b"hello"
    );

    let request = request_rx.recv().expect("receive captured request");
    assert!(request.starts_with(b"GET /resource?key=value HTTP/1.1\r\n"));
    assert!(
        request
            .windows(19)
            .any(|window| window == b"Connection: close\r\n")
    );
    server.join();
}

#[test]
fn fragmented_status_headers_and_body_are_reassembled() {
    let server = spawn_server(move |mut stream| {
        let _request = read_request_head(&mut stream);
        stream.set_nodelay(true).expect("set TCP_NODELAY");
        for fragment in [
            &b"HTTP/1."[..],
            &b"1 200 O"[..],
            &b"K\r"[..],
            &b"\nContent-Len"[..],
            &b"gth: 6\r\n\r"[..],
            &b"\nabc"[..],
            &b"def"[..],
        ] {
            stream.write_all(fragment).expect("write fragment");
            thread::yield_now();
        }
    });
    let response = get(server.address, ClientConfig::default()).expect("parse fragments");
    assert_eq!(
        response.read_body_to_end().expect("body succeeds"),
        b"abcdef"
    );
    server.join();
}

#[test]
fn duplicate_identical_content_lengths_are_accepted() {
    let server = spawn_raw_response(
        b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nContent-Length: 3\r\n\r\nabc".to_vec(),
    );
    let response =
        get(server.address, ClientConfig::default()).expect("identical lengths accepted");
    assert_eq!(response.read_body_to_end().expect("read body"), b"abc");
    server.join();
}

#[test]
fn chunked_body_extensions_and_trailers_are_streamed() {
    let server = spawn_raw_response(
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nTrailer: X-Checksum\r\n\r\n\
          3;kind=first\r\nabc\r\n2;note=\"ok value\"\r\nde\r\n0\r\nX-Checksum: 123\r\n\r\n"
            .to_vec(),
    );
    let response = get(server.address, ClientConfig::default()).expect("chunked head succeeds");
    assert_eq!(response.head().body_framing(), BodyFraming::Chunked);
    let (_head, mut body) = response.into_parts();
    let mut decoded = Vec::new();
    let mut buffer = [0_u8; 2];
    loop {
        let count = body.read_chunk(&mut buffer).expect("decode chunk");
        if count == 0 {
            break;
        }
        decoded.extend_from_slice(&buffer[..count]);
    }
    assert_eq!(decoded, b"abcde");
    assert_eq!(
        body.trailers()
            .get("x-checksum")
            .and_then(HeaderValue::to_str),
        Some("123")
    );
    server.join();
}

#[test]
fn cancellation_interrupts_a_partially_read_body() {
    let (first_byte_tx, first_byte_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let server = spawn_server(move |mut stream| {
        let _request = read_request_head(&mut stream);
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\na")
            .expect("write first byte");
        first_byte_tx.send(()).expect("signal first byte");
        release_rx.recv().expect("wait for cancellation assertion");
        let _ignored = stream.write_all(b"b");
    });
    let cancellation_source = CancellationSource::new();
    let cancellation = cancellation_source.token();
    let request = Request::get(target(server.address, "/cancel"), RedirectPolicy::Manual)
        .with_cancellation(cancellation.clone());
    let mut response = HttpClient::default()
        .execute(&request)
        .expect("head succeeds");
    first_byte_rx.recv().expect("first byte was sent");
    let mut one = [0_u8; 1];
    assert_eq!(
        response
            .body_mut()
            .read_chunk(&mut one)
            .expect("first byte"),
        1
    );
    assert_eq!(one, *b"a");
    assert!(cancellation_source.cancel());
    assert!(matches!(
        response.body_mut().read_chunk(&mut one),
        Err(Error::Cancelled)
    ));
    release_tx.send(()).expect("release server");
    server.join();
}

#[test]
fn foundation_cancellation_token_interoperates_without_an_adapter() {
    let source = wild_buzzard_runtime::CancellationSource::new();
    let shared_token: wild_buzzard_runtime::CancellationToken = source.token();
    let target = LoopbackTarget::parse("http://127.0.0.1:9/").expect("valid target");
    let request = Request::get(target, RedirectPolicy::Manual).with_cancellation(shared_token);
    assert!(source.cancel());
    assert!(matches!(
        HttpClient::default().execute(&request),
        Err(Error::Cancelled)
    ));
}

#[test]
fn cancellation_prevents_delivery_of_body_bytes_buffered_with_the_head() {
    let server = spawn_raw_response(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\ndata".to_vec());
    let source = CancellationSource::new();
    let request = Request::get(target(server.address, "/buffered"), RedirectPolicy::Manual)
        .with_cancellation(source.token());
    let mut response = HttpClient::default()
        .execute(&request)
        .expect("response head succeeds");
    assert!(source.cancel());
    let mut output = [0_u8; 4];
    assert!(matches!(
        response.body_mut().read_chunk(&mut output),
        Err(Error::Cancelled)
    ));
    server.join();
}

#[test]
fn deadline_prevents_delivery_of_body_bytes_buffered_with_the_head() {
    let server = spawn_raw_response(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\ndata".to_vec());
    let deadline = Instant::now() + Duration::from_millis(100);
    let request = Request::get(target(server.address, "/buffered"), RedirectPolicy::Manual)
        .with_deadline(deadline);
    let mut response = HttpClient::default()
        .execute(&request)
        .expect("response head succeeds");
    let remaining = deadline.saturating_duration_since(Instant::now());
    thread::sleep(remaining + Duration::from_millis(5));
    let mut output = [0_u8; 4];
    assert!(matches!(
        response.body_mut().read_chunk(&mut output),
        Err(Error::Timeout(Operation::ReadBody))
    ));
    server.join();
}

#[test]
fn body_inactivity_timeout_does_not_poison_a_later_successful_read() {
    let (release_tx, release_rx) = mpsc::channel();
    let server = spawn_server(move |mut stream| {
        let _request = read_request_head(&mut stream);
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\n")
            .expect("write response head");
        release_rx.recv().expect("wait for timeout assertion");
        stream.write_all(b"a").expect("write delayed body");
    });
    let config = ClientConfig::default().with_read_timeout(Duration::from_millis(30));
    let mut response = get(server.address, config).expect("head succeeds");
    let mut output = [0_u8; 1];
    assert_eq!(
        response.body_mut().read_chunk(&mut output),
        Err(Error::Timeout(Operation::ReadBody))
    );

    release_tx.send(()).expect("release delayed body");
    assert_eq!(
        response
            .body_mut()
            .read_chunk(&mut output)
            .expect("retry succeeds"),
        1
    );
    assert_eq!(output, *b"a");
    server.join();
}

#[test]
fn head_read_inactivity_timeout_is_reported() {
    let (release_tx, release_rx) = mpsc::channel();
    let server = spawn_server(move |mut stream| {
        let _request = read_request_head(&mut stream);
        let _ignored = release_rx.recv_timeout(Duration::from_secs(2));
    });
    let config = ClientConfig::default().with_read_timeout(Duration::from_millis(30));
    let result = get(server.address, config);
    assert!(matches!(result, Err(Error::Timeout(Operation::ReadHead))));
    release_tx.send(()).expect("release server");
    server.join();
}

#[test]
fn expired_absolute_deadline_is_reported_before_connect() {
    let target = LoopbackTarget::parse("http://127.0.0.1:9/").expect("valid target");
    let expired = Instant::now()
        .checked_sub(Duration::from_millis(1))
        .expect("one millisecond before now is representable");
    let request = Request::get(target, RedirectPolicy::Manual).with_deadline(expired);
    assert!(matches!(
        HttpClient::default().execute(&request),
        Err(Error::Timeout(Operation::Connect))
    ));
}

#[test]
fn bare_lf_and_malformed_status_are_rejected() {
    for raw in [
        &b"HTTP/1.1 200 OK\nContent-Length: 0\n\n"[..],
        &b"HTTP/1.1 20 OK\r\nContent-Length: 0\r\n\r\n"[..],
        &b"HTTP/2 200 OK\r\n\r\n"[..],
    ] {
        let server = spawn_raw_response(raw.to_vec());
        let result = get(server.address, ClientConfig::default());
        assert!(matches!(
            result,
            Err(Error::InvalidLineEnding | Error::MalformedStatusLine)
        ));
        server.join();
    }
}

#[test]
fn malformed_and_folded_headers_are_rejected() {
    for (raw, folded) in [
        (&b"HTTP/1.1 200 OK\r\nMissing-Colon\r\n\r\n"[..], false),
        (
            &b"HTTP/1.1 200 OK\r\nX-One: value\r\n continuation\r\n\r\n"[..],
            true,
        ),
        (&b"HTTP/1.1 200 OK\r\nBad Name: value\r\n\r\n"[..], false),
        (
            &b"HTTP/1.1 200 OK\r\nX-Test: bad\x01value\r\n\r\n"[..],
            false,
        ),
    ] {
        let server = spawn_raw_response(raw.to_vec());
        let result = get(server.address, ClientConfig::default());
        if folded {
            assert!(matches!(result, Err(Error::ObsoleteLineFolding)));
        } else {
            assert!(matches!(result, Err(Error::MalformedHeader)));
        }
        server.join();
    }
}

#[test]
fn transfer_encoding_with_content_length_is_rejected() {
    let server = spawn_raw_response(
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Length: 3\r\n\r\n0\r\n\r\n"
            .to_vec(),
    );
    assert!(matches!(
        get(server.address, ClientConfig::default()),
        Err(Error::AmbiguousBodyFraming)
    ));
    server.join();
}

#[test]
fn no_content_response_rejects_forbidden_framing_fields() {
    for raw in [
        &b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n"[..],
        &b"HTTP/1.1 204 No Content\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n"[..],
    ] {
        let server = spawn_raw_response(raw.to_vec());
        assert!(matches!(
            get(server.address, ClientConfig::default()),
            Err(Error::MalformedHeader)
        ));
        server.join();
    }
}

#[test]
fn conflicting_content_lengths_are_rejected() {
    for header in [
        "Content-Length: 3\r\nContent-Length: 4\r\n",
        "Content-Length: 3, 4\r\n",
        "Content-Length: 3\r\nContent-Length:\r\n",
    ] {
        let server = spawn_raw_response(format!("HTTP/1.1 200 OK\r\n{header}\r\nabc").into_bytes());
        let result = get(server.address, ClientConfig::default());
        assert!(matches!(
            result,
            Err(Error::ConflictingContentLength | Error::InvalidContentLength)
        ));
        server.join();
    }
}

#[test]
fn declared_body_over_limit_is_rejected_before_delivery() {
    let server = spawn_raw_response(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello".to_vec());
    let config = ClientConfig::default().with_max_body_bytes(4);
    assert!(matches!(
        get(server.address, config),
        Err(Error::LimitExceeded {
            kind: LimitKind::BodyBytes,
            limit: 4
        })
    ));
    server.join();
}

#[test]
fn close_delimited_body_limit_detects_extra_byte_but_accepts_exact_limit() {
    for (body, succeeds) in [(&b"abcd"[..], true), (&b"abcde"[..], false)] {
        let mut response_bytes = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".to_vec();
        response_bytes.extend_from_slice(body);
        let server = spawn_raw_response(response_bytes);
        let response = get(
            server.address,
            ClientConfig::default().with_max_body_bytes(4),
        )
        .expect("head succeeds");
        let result = response.read_body_to_end();
        if succeeds {
            assert_eq!(result.expect("exact limit accepted"), b"abcd");
        } else {
            assert!(matches!(
                result,
                Err(Error::LimitExceeded {
                    kind: LimitKind::BodyBytes,
                    limit: 4
                })
            ));
        }
        server.join();
    }
}

#[test]
fn aggregate_header_byte_and_count_limits_are_enforced() {
    let server = spawn_raw_response(
        b"HTTP/1.1 200 OK\r\nX-One: 1\r\nX-Two: 2\r\nContent-Length: 0\r\n\r\n".to_vec(),
    );
    let count_result = get(
        server.address,
        ClientConfig::default().with_max_header_count(2),
    );
    assert!(matches!(
        count_result,
        Err(Error::LimitExceeded {
            kind: LimitKind::HeaderCount,
            limit: 2
        })
    ));
    server.join();

    let server = spawn_raw_response(
        b"HTTP/1.1 200 OK\r\nX-Very-Long: abcdefghijklmnopqrstuvwxyz\r\n\r\n".to_vec(),
    );
    let byte_result = get(
        server.address,
        ClientConfig::default().with_max_header_bytes(32),
    );
    assert!(matches!(
        byte_result,
        Err(Error::LimitExceeded {
            kind: LimitKind::HeaderBytes,
            limit: 32
        })
    ));
    server.join();
}

#[test]
fn chunk_line_and_decoded_chunk_limits_are_enforced() {
    let server = spawn_raw_response(
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1;extension=long\r\na\r\n0\r\n\r\n"
            .to_vec(),
    );
    let response = get(
        server.address,
        ClientConfig::default().with_max_chunk_line_bytes(4),
    )
    .expect("head succeeds");
    assert!(matches!(
        response.read_body_to_end(),
        Err(Error::LimitExceeded {
            kind: LimitKind::ChunkLineBytes,
            limit: 4
        })
    ));
    server.join();

    let server = spawn_raw_response(
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n".to_vec(),
    );
    let response = get(
        server.address,
        ClientConfig::default().with_max_body_bytes(4),
    )
    .expect("head succeeds");
    assert!(matches!(
        response.read_body_to_end(),
        Err(Error::LimitExceeded {
            kind: LimitKind::BodyBytes,
            limit: 4
        })
    ));
    server.join();
}

#[test]
fn premature_eof_is_rejected_for_fixed_and_chunked_bodies() {
    for raw in [
        &b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nabc"[..],
        &b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nabc"[..],
        &b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1\r\na\r\n"[..],
    ] {
        let server = spawn_raw_response(raw.to_vec());
        let response = get(server.address, ClientConfig::default()).expect("head succeeds");
        assert!(matches!(
            response.read_body_to_end(),
            Err(Error::PrematureEof)
        ));
        server.join();
    }
}

#[test]
fn malformed_chunk_syntax_and_prohibited_trailer_are_rejected() {
    for (raw, trailer) in [
        (
            &b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\ng\r\na\r\n0\r\n\r\n"[..],
            false,
        ),
        (
            &b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1;bad=\"unterminated\r\na\r\n0\r\n\r\n"[..],
            false,
        ),
        (
            &b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n0\r\nContent-Length: 2\r\n\r\n"[..],
            true,
        ),
    ] {
        let server = spawn_raw_response(raw.to_vec());
        let response = get(server.address, ClientConfig::default()).expect("head succeeds");
        let result = response.read_body_to_end();
        if trailer {
            assert!(matches!(result, Err(Error::ProhibitedTrailer(_))));
        } else {
            assert!(matches!(result, Err(Error::MalformedChunkSize)));
        }
        server.join();
    }
}

#[test]
fn malformed_chunk_error_is_latched_and_later_chunk_bytes_stay_hidden() {
    let server = spawn_raw_response(
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\ng\r\n1\r\na\r\n0\r\n\r\n".to_vec(),
    );
    let response = get(server.address, ClientConfig::default()).expect("head succeeds");
    let (_head, mut body) = response.into_parts();
    let mut output = [0xa5_u8; 8];

    let first = body
        .read_chunk(&mut output)
        .expect_err("malformed chunk poisons body");
    assert_eq!(first, Error::MalformedChunkSize);
    assert_eq!(output, [0xa5; 8]);

    let second = body
        .read_chunk(&mut output)
        .expect_err("poisoned body stays failed");
    assert_eq!(second, first);
    assert_eq!(output, [0xa5; 8]);
    assert_eq!(body.decoded_bytes(), 0);
    server.join();
}

#[test]
fn trailer_failure_is_latched_and_partial_trailers_are_not_exposed() {
    let server = spawn_raw_response(
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n0\r\nX-Safe: yes\r\nContent-Length: 1\r\n\r\n"
            .to_vec(),
    );
    let response = get(server.address, ClientConfig::default()).expect("head succeeds");
    let (_head, mut body) = response.into_parts();
    let mut output = [0xa5_u8; 1];

    let first = body
        .read_chunk(&mut output)
        .expect_err("prohibited trailer poisons body");
    assert_eq!(first, Error::ProhibitedTrailer("content-length".to_owned()));
    assert!(body.trailers().is_empty());

    let second = body
        .read_chunk(&mut output)
        .expect_err("poisoned trailer parser stays failed");
    assert_eq!(second, first);
    assert_eq!(output, [0xa5]);
    assert!(body.trailers().is_empty());
    server.join();
}

#[test]
fn premature_eof_and_body_limit_failures_are_latched() {
    for (raw, config, expected) in [
        (
            &b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\na"[..],
            ClientConfig::default(),
            Error::PrematureEof,
        ),
        (
            &b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nab"[..],
            ClientConfig::default().with_max_body_bytes(1),
            Error::LimitExceeded {
                kind: LimitKind::BodyBytes,
                limit: 1,
            },
        ),
    ] {
        let server = spawn_raw_response(raw.to_vec());
        let response = get(server.address, config).expect("head succeeds");
        let (_head, mut body) = response.into_parts();
        let mut output = [0_u8; 1];
        assert_eq!(body.read_chunk(&mut output).expect("first byte"), 1);
        assert_eq!(output, *b"a");

        output[0] = 0xa5;
        let first = body
            .read_chunk(&mut output)
            .expect_err("terminal body failure");
        assert_eq!(first, expected);
        assert_eq!(output, [0xa5]);
        let second = body
            .read_chunk(&mut output)
            .expect_err("terminal body failure remains latched");
        assert_eq!(second, first);
        assert_eq!(output, [0xa5]);
        assert_eq!(body.decoded_bytes(), 1);
        server.join();
    }
}

#[test]
fn unsupported_transfer_and_content_codings_are_rejected() {
    for (raw, content_coding) in [
        (
            &b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip\r\n\r\n"[..],
            false,
        ),
        (
            &b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip, chunked\r\n\r\n"[..],
            false,
        ),
        (
            &b"HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: 0\r\n\r\n"[..],
            true,
        ),
    ] {
        let server = spawn_raw_response(raw.to_vec());
        let result = get(server.address, ClientConfig::default());
        if content_coding {
            assert!(matches!(result, Err(Error::UnsupportedContentCoding(_))));
        } else {
            assert!(matches!(result, Err(Error::UnsupportedTransferCoding(_))));
        }
        server.join();
    }
}

#[test]
fn numeric_loopback_policy_refuses_domains_and_non_loopback_ips() {
    assert!(LoopbackTarget::parse("http://127.0.0.1:8000/").is_ok());
    assert!(LoopbackTarget::parse("http://127.255.1.2:8000/").is_ok());
    assert!(LoopbackTarget::parse("http://[::1]:8000/").is_ok());
    for url in [
        "http://localhost:8000/",
        "http://192.0.2.1:8000/",
        "http://8.8.8.8/",
        "http://[2001:db8::1]/",
    ] {
        assert!(matches!(
            LoopbackTarget::parse(url),
            Err(Error::NonLoopbackTarget)
        ));
    }
}

#[test]
fn unsupported_scheme_credentials_and_fragment_are_refused() {
    assert!(matches!(
        LoopbackTarget::parse("https://127.0.0.1/"),
        Err(Error::UnsupportedScheme(_))
    ));
    assert!(matches!(
        LoopbackTarget::parse("http://user:pass@127.0.0.1/"),
        Err(Error::CredentialsNotAllowed)
    ));
    assert!(matches!(
        LoopbackTarget::parse("http://127.0.0.1/#fragment"),
        Err(Error::FragmentNotAllowed)
    ));
}

#[test]
fn method_header_and_request_target_validation_blocks_injection() {
    assert!(Method::new("POST").is_ok());
    assert!(matches!(
        Method::new("GET\r\nInjected"),
        Err(Error::InvalidMethod)
    ));
    assert!(HeaderName::new("x-safe").is_ok());
    assert!(matches!(
        HeaderName::new("x bad"),
        Err(Error::InvalidHeaderName)
    ));
    assert!(matches!(
        HeaderValue::from_text("safe\r\ninjected: yes"),
        Err(Error::InvalidHeaderValue)
    ));

    let target = LoopbackTarget::parse("http://127.0.0.1:8000/").expect("valid target");
    let mut request = Request::get(target, RedirectPolicy::Manual);
    assert!(matches!(
        request.append_header(
            HeaderName::new("content-length").expect("valid name"),
            HeaderValue::from_text("5").expect("valid value")
        ),
        Err(Error::ReservedRequestHeader(_))
    ));
}

#[test]
fn redirect_policy_never_follows_and_can_reject() {
    let server = spawn_raw_response(
        b"HTTP/1.1 302 Found\r\nLocation: http://8.8.8.8/\r\nContent-Length: 0\r\n\r\n".to_vec(),
    );
    let manual = HttpClient::default().execute(&Request::get(
        target(server.address, "/redirect"),
        RedirectPolicy::Manual,
    ));
    assert_eq!(
        manual
            .expect("manual redirect exposed")
            .head()
            .status()
            .as_u16(),
        302
    );
    server.join();

    let server = spawn_raw_response(
        b"HTTP/1.1 307 Temporary Redirect\r\nLocation: /elsewhere\r\nContent-Length: 0\r\n\r\n"
            .to_vec(),
    );
    let rejected = HttpClient::default().execute(&Request::get(
        target(server.address, "/redirect"),
        RedirectPolicy::Reject,
    ));
    assert!(matches!(rejected, Err(Error::RedirectRejected(307))));
    server.join();
}

#[test]
fn bounded_informational_responses_precede_final_response() {
    let server = spawn_raw_response(
        b"HTTP/1.1 103 Early Hints\r\nLink: </style.css>; rel=preload\r\n\r\n\
          HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok"
            .to_vec(),
    );
    let response = get(server.address, ClientConfig::default()).expect("1xx then final succeeds");
    assert_eq!(response.head().informational_response_count(), 1);
    assert_eq!(response.read_body_to_end().expect("read final body"), b"ok");
    server.join();

    let server = spawn_raw_response(
        b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n".to_vec(),
    );
    let result = get(
        server.address,
        ClientConfig::default().with_max_informational_responses(0),
    );
    assert!(matches!(
        result,
        Err(Error::LimitExceeded {
            kind: LimitKind::InformationalResponses,
            limit: 0
        })
    ));
    server.join();
}

#[test]
fn head_response_has_no_transport_body() {
    let server = spawn_raw_response(b"HTTP/1.1 200 OK\r\nContent-Length: 5000\r\n\r\n".to_vec());
    let request = Request::new(
        Method::head(),
        target(server.address, "/head"),
        RedirectPolicy::Manual,
    );
    let response = HttpClient::new(ClientConfig::default().with_max_body_bytes(1))
        .execute(&request)
        .expect("HEAD ignores advertised representation length");
    assert_eq!(response.head().body_framing(), BodyFraming::None);
    assert!(response.read_body_to_end().expect("empty body").is_empty());
    server.join();
}

#[test]
fn request_body_is_bounded_and_framed_by_transport() {
    let (captured_tx, captured_rx) = mpsc::channel();
    let server = spawn_server(move |mut stream| {
        let head = read_request_head(&mut stream);
        let mut body = [0_u8; 3];
        stream.read_exact(&mut body).expect("read request body");
        captured_tx
            .send((head, body))
            .expect("send captured request");
        stream
            .write_all(b"HTTP/1.1 204 No Content\r\n\r\n")
            .expect("write response");
    });
    let mut positive = Request::new(
        Method::new("POST").expect("valid POST"),
        target(server.address, "/submit"),
        RedirectPolicy::Manual,
    )
    .with_body(b"abc".to_vec());
    positive
        .append_header(
            HeaderName::new("content-type").expect("valid name"),
            HeaderValue::from_text("text/plain").expect("valid value"),
        )
        .expect("caller-owned field accepted");
    let response = HttpClient::default()
        .execute(&positive)
        .expect("request body succeeds");
    assert_eq!(response.head().status().as_u16(), 204);
    let (head, body) = captured_rx.recv().expect("receive captured request");
    assert!(
        head.windows(19)
            .any(|window| window == b"Content-Length: 3\r\n")
    );
    assert!(
        head.windows(26)
            .any(|window| window == b"content-type: text/plain\r\n")
    );
    assert_eq!(body, *b"abc");
    server.join();

    let target = LoopbackTarget::parse("http://127.0.0.1:9/").expect("valid target");
    let request = Request::new(
        Method::new("POST").expect("valid POST"),
        target,
        RedirectPolicy::Manual,
    )
    .with_body(b"abc".to_vec());
    let result =
        HttpClient::new(ClientConfig::default().with_max_request_body_bytes(2)).execute(&request);
    assert!(matches!(
        result,
        Err(Error::LimitExceeded {
            kind: LimitKind::RequestBodyBytes,
            limit: 2
        })
    ));
}

#[test]
fn oversized_request_target_is_rejected_before_connect() {
    let path = format!("/{}", "a".repeat(256));
    let (listener, target) = idle_listener_and_target(&path);
    let request = Request::get(target, RedirectPolicy::Manual);
    let config = ClientConfig::default().with_max_request_head_bytes(96);
    assert_eq!(config.max_request_head_bytes(), 96);
    assert!(matches!(
        HttpClient::new(config).execute(&request),
        Err(Error::LimitExceeded {
            kind: LimitKind::RequestHeadBytes,
            limit: 96
        })
    ));
    assert_no_connection(&listener);
}

#[test]
fn oversized_request_header_is_rejected_before_connect() {
    let (listener, target) = idle_listener_and_target("/large-header");
    let mut request = Request::get(target, RedirectPolicy::Manual);
    request
        .append_header(
            HeaderName::new("x-large").expect("valid name"),
            HeaderValue::from_bytes(vec![b'a'; 256]).expect("valid large value"),
        )
        .expect("caller-owned header accepted");
    let config = ClientConfig::default().with_max_request_head_bytes(128);
    assert!(matches!(
        HttpClient::new(config).execute(&request),
        Err(Error::LimitExceeded {
            kind: LimitKind::RequestHeadBytes,
            limit: 128
        })
    ));
    assert_no_connection(&listener);
}

#[test]
fn excessive_request_header_count_is_rejected_before_connect() {
    let (listener, target) = idle_listener_and_target("/many-headers");
    let mut request = Request::get(target, RedirectPolicy::Manual);
    for name in ["x-one", "x-two"] {
        request
            .append_header(
                HeaderName::new(name).expect("valid name"),
                HeaderValue::from_text("value").expect("valid value"),
            )
            .expect("caller-owned header accepted");
    }
    let config = ClientConfig::default().with_max_request_header_count(1);
    assert_eq!(config.max_request_header_count(), 1);
    assert!(matches!(
        HttpClient::new(config).execute(&request),
        Err(Error::LimitExceeded {
            kind: LimitKind::RequestHeaderCount,
            limit: 1
        })
    ));
    assert_no_connection(&listener);
}

#[test]
fn exact_request_head_boundary_serializes_every_owned_and_caller_field() {
    let (captured_tx, captured_rx) = mpsc::channel();
    let server = spawn_server(move |mut stream| {
        let head = read_request_head(&mut stream);
        let mut body = [0_u8; 3];
        stream.read_exact(&mut body).expect("read boundary body");
        captured_tx
            .send((head, body))
            .expect("send boundary request");
        stream
            .write_all(b"HTTP/1.1 204 No Content\r\n\r\n")
            .expect("write boundary response");
    });
    let mut request = Request::new(
        Method::new("POST").expect("valid POST"),
        target(server.address, "/boundary"),
        RedirectPolicy::Manual,
    )
    .with_body(b"abc".to_vec());
    request
        .append_header(
            HeaderName::new("x-edge").expect("valid name"),
            HeaderValue::from_text("yes").expect("valid value"),
        )
        .expect("caller-owned header accepted");
    let expected = format!(
        "POST /boundary HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nx-edge: yes\r\nContent-Length: 3\r\n\r\n",
        server.address
    )
    .into_bytes();
    let config = ClientConfig::default()
        .with_max_request_head_bytes(expected.len())
        .with_max_request_header_count(1);
    HttpClient::new(config)
        .execute(&request)
        .expect("exact request-head boundary succeeds");
    let (captured, body) = captured_rx.recv().expect("receive boundary request");
    assert_eq!(captured, expected);
    assert_eq!(body, *b"abc");
    server.join();

    let (listener, target) = idle_listener_and_target("/one-byte-over");
    let request = Request::get(target, RedirectPolicy::Manual);
    let authority = listener
        .local_addr()
        .expect("read boundary listener address");
    let exact =
        format!("GET /one-byte-over HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n")
            .len();
    assert!(matches!(
        HttpClient::new(ClientConfig::default().with_max_request_head_bytes(exact - 1))
            .execute(&request),
        Err(Error::LimitExceeded {
            kind: LimitKind::RequestHeadBytes,
            limit
        }) if limit == exact - 1
    ));
    assert_no_connection(&listener);
}
