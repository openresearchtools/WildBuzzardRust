use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use http::Method;
use serde_json::json;
use webdriver::command::{WebDriverCommand, WebDriverMessage};
use webdriver::error::{ErrorStatus, WebDriverError, WebDriverResult};
use webdriver::httpapi::VoidWebDriverExtensionRoute;
use webdriver::response::{NewSessionResponse, ValueResponse, WebDriverResponse};
use webdriver::server::{
    BearerToken, DispatchLifetime, ServerSecurityPolicy, Session, SessionTeardownKind,
    WebDriverHandler, start_authenticated,
};

const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

struct CountingHandler {
    calls: Arc<AtomicUsize>,
    gate: Option<Arc<(Mutex<bool>, Condvar)>>,
    delay: Duration,
    entered: Option<mpsc::Sender<()>>,
}

impl WebDriverHandler<VoidWebDriverExtensionRoute> for CountingHandler {
    fn handle_command(
        &mut self,
        _: &Option<Session>,
        message: WebDriverMessage<VoidWebDriverExtensionRoute>,
    ) -> WebDriverResult<WebDriverResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(entered) = self.entered.take() {
            let _ = entered.send(());
        }
        if let Some(gate) = &self.gate {
            let (lock, wake) = &**gate;
            let mut released = lock.lock().unwrap();
            while !*released {
                released = wake.wait(released).unwrap();
            }
        }
        if !self.delay.is_zero() {
            thread::sleep(self.delay);
        }
        match message.command {
            WebDriverCommand::Status => Ok(WebDriverResponse::Generic(ValueResponse(json!({
                "ready": true,
                "message": "ready",
            })))),
            _ => Err(WebDriverError::new(
                ErrorStatus::UnsupportedOperation,
                "test handler supports status only",
            )),
        }
    }

    fn teardown_session(&mut self, _: SessionTeardownKind) {}
}

struct BlockingStatefulHandler {
    gate: Option<Arc<(Mutex<bool>, Condvar)>>,
    entered: Option<mpsc::Sender<()>>,
    new_session_calls: Arc<AtomicUsize>,
    next_session: usize,
}

impl WebDriverHandler<VoidWebDriverExtensionRoute> for BlockingStatefulHandler {
    fn handle_command(
        &mut self,
        _: &Option<Session>,
        message: WebDriverMessage<VoidWebDriverExtensionRoute>,
    ) -> WebDriverResult<WebDriverResponse> {
        match message.command {
            WebDriverCommand::Status => {
                if let Some(entered) = self.entered.take() {
                    let _ = entered.send(());
                }
                if let Some(gate) = &self.gate {
                    let (lock, wake) = &**gate;
                    let mut released = lock.lock().unwrap();
                    while !*released {
                        released = wake.wait(released).unwrap();
                    }
                }
                Ok(WebDriverResponse::Generic(ValueResponse(json!({
                    "ready": true,
                    "message": "ready",
                }))))
            }
            WebDriverCommand::NewSession(_) => {
                self.new_session_calls.fetch_add(1, Ordering::SeqCst);
                self.next_session += 1;
                Ok(WebDriverResponse::NewSession(NewSessionResponse::new(
                    format!("recovery-session-{}", self.next_session),
                    json!({}),
                )))
            }
            WebDriverCommand::DeleteSession => Ok(WebDriverResponse::DeleteSession),
            _ => Err(WebDriverError::new(
                ErrorStatus::UnsupportedOperation,
                "test handler does not support this command",
            )),
        }
    }

    fn teardown_session(&mut self, _: SessionTeardownKind) {}
}

struct NearExpiryHandler {
    gate: Arc<(Mutex<bool>, Condvar)>,
    entered: Option<mpsc::Sender<()>>,
    remaining: Option<mpsc::Sender<Duration>>,
    expire_first_session: bool,
    mutations: Arc<AtomicUsize>,
}

impl WebDriverHandler<VoidWebDriverExtensionRoute> for NearExpiryHandler {
    fn handle_command(
        &mut self,
        _: &Option<Session>,
        message: WebDriverMessage<VoidWebDriverExtensionRoute>,
    ) -> WebDriverResult<WebDriverResponse> {
        if matches!(message.command, WebDriverCommand::DeleteSession) {
            Ok(WebDriverResponse::DeleteSession)
        } else {
            Err(WebDriverError::new(
                ErrorStatus::UnknownError,
                "authenticated test command lacked its dispatch lifetime",
            ))
        }
    }

    fn handle_command_with_lifetime(
        &mut self,
        _: &Option<Session>,
        message: WebDriverMessage<VoidWebDriverExtensionRoute>,
        lifetime: &DispatchLifetime,
    ) -> WebDriverResult<WebDriverResponse> {
        match message.command {
            WebDriverCommand::Status => {
                if let Some(entered) = self.entered.take() {
                    let _ = entered.send(());
                }
                let (lock, wake) = &*self.gate;
                let mut released = lock.lock().unwrap();
                while !*released {
                    released = wake.wait(released).unwrap();
                }
                Ok(WebDriverResponse::Generic(ValueResponse(json!({
                    "ready": true,
                    "message": "ready",
                }))))
            }
            WebDriverCommand::NewSession(_) if self.expire_first_session => {
                self.expire_first_session = false;
                if let Some(remaining) = self.remaining.take() {
                    let _ = remaining.send(lifetime.remaining().unwrap_or_default());
                }
                while !lifetime.cancel_if_expired() {
                    thread::sleep(Duration::from_millis(1));
                }
                Err(WebDriverError::new(
                    ErrorStatus::Timeout,
                    "test command retained the inherited absolute deadline",
                ))
            }
            WebDriverCommand::NewSession(_) => {
                if lifetime.cancel_if_expired() {
                    return Err(WebDriverError::new(
                        ErrorStatus::Timeout,
                        "fresh recovery command unexpectedly expired",
                    ));
                }
                self.mutations.fetch_add(1, Ordering::SeqCst);
                Ok(WebDriverResponse::NewSession(NewSessionResponse::new(
                    "near-expiry-recovery".to_owned(),
                    json!({}),
                )))
            }
            WebDriverCommand::DeleteSession => Ok(WebDriverResponse::DeleteSession),
            _ => Err(WebDriverError::new(
                ErrorStatus::UnsupportedOperation,
                "test handler does not support this command",
            )),
        }
    }

    fn teardown_session(&mut self, _: SessionTeardownKind) {}
}

struct DisconnectAwareHandler {
    entered: Option<mpsc::Sender<DispatchLifetime>>,
    wait_for_disconnect: bool,
    calls: Arc<AtomicUsize>,
}

impl WebDriverHandler<VoidWebDriverExtensionRoute> for DisconnectAwareHandler {
    fn handle_command(
        &mut self,
        _: &Option<Session>,
        _: WebDriverMessage<VoidWebDriverExtensionRoute>,
    ) -> WebDriverResult<WebDriverResponse> {
        Err(WebDriverError::new(
            ErrorStatus::UnknownError,
            "authenticated test command lacked its dispatch lifetime",
        ))
    }

    fn handle_command_with_lifetime(
        &mut self,
        _: &Option<Session>,
        _: WebDriverMessage<VoidWebDriverExtensionRoute>,
        lifetime: &DispatchLifetime,
    ) -> WebDriverResult<WebDriverResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.wait_for_disconnect {
            self.wait_for_disconnect = false;
            if let Some(entered) = self.entered.take() {
                let _ = entered.send(lifetime.clone());
            }
            while !lifetime.is_cancelled() {
                thread::sleep(Duration::from_millis(1));
            }
            return Err(WebDriverError::new(
                ErrorStatus::Timeout,
                "client disconnected",
            ));
        }
        Ok(WebDriverResponse::Generic(ValueResponse(json!({
            "ready": true,
            "message": "ready",
        }))))
    }

    fn teardown_session(&mut self, _: SessionTeardownKind) {}
}

struct PostCompletionDeliveryHandler {
    authority: Option<mpsc::Sender<DispatchLifetime>>,
    new_session_calls: Arc<AtomicUsize>,
    session_command_calls: Arc<AtomicUsize>,
    teardown_calls: Arc<AtomicUsize>,
}

impl WebDriverHandler<VoidWebDriverExtensionRoute> for PostCompletionDeliveryHandler {
    fn handle_command(
        &mut self,
        _: &Option<Session>,
        message: WebDriverMessage<VoidWebDriverExtensionRoute>,
    ) -> WebDriverResult<WebDriverResponse> {
        if matches!(message.command, WebDriverCommand::DeleteSession) {
            Ok(WebDriverResponse::DeleteSession)
        } else {
            Err(WebDriverError::new(
                ErrorStatus::UnsupportedOperation,
                "test handler supports synthetic teardown only",
            ))
        }
    }

    fn handle_command_with_lifetime(
        &mut self,
        _: &Option<Session>,
        message: WebDriverMessage<VoidWebDriverExtensionRoute>,
        lifetime: &DispatchLifetime,
    ) -> WebDriverResult<WebDriverResponse> {
        match message.command {
            WebDriverCommand::NewSession(_) => {
                let call = self.new_session_calls.fetch_add(1, Ordering::SeqCst);
                if call == 0 {
                    if let Some(authority) = self.authority.take() {
                        let _ = authority.send(lifetime.clone());
                    }
                    Ok(WebDriverResponse::NewSession(NewSessionResponse::new(
                        "abandoned-after-completion".to_owned(),
                        json!({"deliveryPadding": "x".repeat(16 * 1024 * 1024)}),
                    )))
                } else {
                    Ok(WebDriverResponse::NewSession(NewSessionResponse::new(
                        "delivery-recovery".to_owned(),
                        json!({}),
                    )))
                }
            }
            WebDriverCommand::GetCurrentUrl => {
                self.session_command_calls.fetch_add(1, Ordering::SeqCst);
                Ok(WebDriverResponse::Generic(ValueResponse(json!(
                    "http://must-not-run.invalid/"
                ))))
            }
            WebDriverCommand::DeleteSession => Ok(WebDriverResponse::DeleteSession),
            _ => Err(WebDriverError::new(
                ErrorStatus::UnsupportedOperation,
                "test handler does not support this command",
            )),
        }
    }

    fn teardown_session(&mut self, _: SessionTeardownKind) {
        self.teardown_calls.fetch_add(1, Ordering::SeqCst);
    }
}

struct PanicAfterSessionHandler {
    authority: Arc<Mutex<Option<DispatchLifetime>>>,
    stateful_calls: Arc<AtomicUsize>,
    teardown_calls: Arc<AtomicUsize>,
}

impl WebDriverHandler<VoidWebDriverExtensionRoute> for PanicAfterSessionHandler {
    fn handle_command(
        &mut self,
        _: &Option<Session>,
        message: WebDriverMessage<VoidWebDriverExtensionRoute>,
    ) -> WebDriverResult<WebDriverResponse> {
        if matches!(message.command, WebDriverCommand::DeleteSession) {
            Ok(WebDriverResponse::DeleteSession)
        } else {
            Err(WebDriverError::new(
                ErrorStatus::UnsupportedOperation,
                "test handler does not support this command",
            ))
        }
    }

    fn handle_command_with_lifetime(
        &mut self,
        session: &Option<Session>,
        message: WebDriverMessage<VoidWebDriverExtensionRoute>,
        lifetime: &DispatchLifetime,
    ) -> WebDriverResult<WebDriverResponse> {
        match message.command {
            WebDriverCommand::NewSession(_) => {
                *self.authority.lock().unwrap() = Some(lifetime.clone());
                Ok(WebDriverResponse::NewSession(NewSessionResponse::new(
                    "panic-session".to_owned(),
                    json!({}),
                )))
            }
            WebDriverCommand::Status if session.is_some() => {
                panic!("injected dispatcher handler panic");
            }
            WebDriverCommand::Get(_) => {
                self.stateful_calls.fetch_add(1, Ordering::SeqCst);
                Ok(WebDriverResponse::Void)
            }
            WebDriverCommand::DeleteSession => Ok(WebDriverResponse::DeleteSession),
            _ => Ok(WebDriverResponse::Generic(ValueResponse(json!({
                "ready": true,
                "message": "ready",
            })))),
        }
    }

    fn teardown_session(&mut self, _: SessionTeardownKind) {
        self.teardown_calls.fetch_add(1, Ordering::SeqCst);
        panic!("injected teardown panic");
    }
}

fn policy(body_limit: usize, queue_limit: usize, deadline: Duration) -> ServerSecurityPolicy {
    ServerSecurityPolicy::new(
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
        BearerToken::from_lower_hex(TOKEN.as_bytes()).unwrap(),
    )
    .unwrap()
    .with_allowed_origins(["http://allowed.invalid"])
    .unwrap()
    .with_limits(body_limit, queue_limit, deadline)
    .unwrap()
}

fn request(
    address: SocketAddr,
    method: &str,
    path: &str,
    host: &str,
    authorization: Option<&str>,
    origin: Option<&str>,
    body: &[u8],
) -> (u16, String) {
    let mut stream = TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    if let Some(authorization) = authorization {
        request.push_str("Authorization: ");
        request.push_str(authorization);
        request.push_str("\r\n");
    }
    if let Some(origin) = origin {
        request.push_str("Origin: ");
        request.push_str(origin);
        request.push_str("\r\n");
    }
    if method == "POST" {
        request.push_str("Content-Type: application/json\r\n");
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).unwrap();
    stream.write_all(body).unwrap();
    stream.flush().unwrap();
    read_response(stream)
}

fn request_without_content_length(
    address: SocketAddr,
    host: &str,
    authorization: &str,
) -> (u16, String) {
    let mut stream = TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let request = format!(
        "GET /status HTTP/1.1\r\nHost: {host}\r\nAuthorization: {authorization}\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).unwrap();
    stream.flush().unwrap();
    read_response(stream)
}

fn request_headers_only(
    address: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    content_length: usize,
) -> (u16, String, Duration) {
    let mut stream = TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut request = format!(
        "{method} {path} HTTP/1.1\r\nConnection: close\r\nContent-Length: {content_length}\r\n"
    );
    for (name, value) in headers {
        request.push_str(name);
        request.push_str(": ");
        request.push_str(value);
        request.push_str("\r\n");
    }
    request.push_str("\r\n");
    let started = Instant::now();
    stream.write_all(request.as_bytes()).unwrap();
    stream.flush().unwrap();
    let (status, response) = read_response(stream);
    (status, response, started.elapsed())
}

fn partial_request(address: SocketAddr, prefix: &[u8]) -> TcpStream {
    let mut stream = TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    stream.write_all(prefix).unwrap();
    stream.flush().unwrap();
    stream
}

fn read_response(mut stream: TcpStream) -> (u16, String) {
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let status = response
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse().ok())
        .expect("HTTP response has a numeric status");
    (status, response)
}

fn authorization() -> String {
    format!("Bearer {TOKEN}")
}

#[test]
fn policy_rejects_non_loopback_and_redacts_the_token() {
    let token = BearerToken::from_lower_hex(TOKEN.as_bytes()).unwrap();
    assert!(!format!("{token:?}").contains(TOKEN));
    let error = ServerSecurityPolicy::new(
        SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 4444),
        token,
    )
    .unwrap_err();
    assert!(!format!("{error:?}").contains(TOKEN));

    let policy = policy(128, 1, Duration::from_secs(1));
    assert!(!format!("{policy:?}").contains(TOKEN));
}

#[test]
#[allow(clippy::too_many_lines)]
fn authentication_host_origin_and_body_limits_precede_dispatch() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut listener = start_authenticated::<_, VoidWebDriverExtensionRoute>(
        policy(128, 2, Duration::from_secs(1)),
        CountingHandler {
            calls: Arc::clone(&calls),
            gate: None,
            delay: Duration::ZERO,
            entered: None,
        },
        Vec::<(Method, &'static str, VoidWebDriverExtensionRoute)>::new(),
    )
    .unwrap();
    let host = listener.socket.to_string();
    let valid_authorization = authorization();

    assert_eq!(
        request(listener.socket, "GET", "/status", &host, None, None, b"").0,
        401
    );
    assert_eq!(
        request(
            listener.socket,
            "GET",
            "/status",
            &host,
            Some("Bearer ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"),
            None,
            b"",
        )
        .0,
        401
    );
    assert_eq!(
        request(
            listener.socket,
            "GET",
            "/status",
            "localhost:1",
            Some(&valid_authorization),
            None,
            b"",
        )
        .0,
        500
    );
    assert_eq!(
        request(
            listener.socket,
            "GET",
            "/status",
            &host,
            Some(&valid_authorization),
            Some("http://rejected.invalid"),
            b"",
        )
        .0,
        500
    );
    assert_eq!(
        request(
            listener.socket,
            "POST",
            "/session",
            &host,
            Some(&valid_authorization),
            None,
            &[b'x'; 256],
        )
        .0,
        413
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    let (status, body) = request(
        listener.socket,
        "GET",
        "/status",
        &host,
        Some(&valid_authorization),
        Some("http://allowed.invalid"),
        b"",
    );
    assert_eq!(status, 200);
    assert!(body.contains("\"ready\":true"));
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let (status, _) = request_without_content_length(listener.socket, &host, &valid_authorization);
    assert_eq!(status, 200);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        request(
            listener.socket,
            "GET",
            "/status",
            &host,
            Some(&valid_authorization),
            None,
            b"x",
        )
        .0,
        413
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    listener.shutdown().unwrap();
    assert!(TcpStream::connect(listener.socket).is_err());
}

#[test]
fn rejected_headers_return_before_any_declared_body_is_read() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut listener = start_authenticated::<_, VoidWebDriverExtensionRoute>(
        policy(128, 1, Duration::from_millis(500)),
        CountingHandler {
            calls: Arc::clone(&calls),
            gate: None,
            delay: Duration::ZERO,
            entered: None,
        },
        Vec::new(),
    )
    .unwrap();
    let host = listener.socket.to_string();
    let auth = authorization();

    let cases = [
        (vec![("Host", host.as_str())], 401, "missing authorization"),
        (
            vec![("Host", "localhost:1"), ("Authorization", auth.as_str())],
            500,
            "invalid Host",
        ),
        (
            vec![
                ("Host", host.as_str()),
                ("Authorization", auth.as_str()),
                ("Origin", "http://rejected.invalid"),
            ],
            500,
            "invalid Origin",
        ),
    ];
    for (headers, expected_status, case) in cases {
        let (status, _, elapsed) =
            request_headers_only(listener.socket, "POST", "/session", &headers, 64);
        assert_eq!(status, expected_status, "{case}");
        assert!(
            elapsed < Duration::from_millis(250),
            "{case} waited for the rejected request body: {elapsed:?}"
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    listener.shutdown().unwrap();
}

#[test]
fn connection_workers_and_header_body_admission_are_bounded() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut listener = start_authenticated::<_, VoidWebDriverExtensionRoute>(
        policy(128, 1, Duration::from_millis(500)),
        CountingHandler {
            calls: Arc::clone(&calls),
            gate: None,
            delay: Duration::ZERO,
            entered: None,
        },
        Vec::new(),
    )
    .unwrap();
    let first = partial_request(listener.socket, b"GET /status HTTP/1.1\r\nHost:");
    thread::sleep(Duration::from_millis(30));
    let second = partial_request(listener.socket, b"GET /status HTTP/1.1\r\nHost:");
    thread::sleep(Duration::from_millis(30));
    let host = listener.socket.to_string();
    let auth = authorization();
    let started = Instant::now();
    let (status, _) = request(
        listener.socket,
        "GET",
        "/status",
        &host,
        Some(&auth),
        None,
        b"",
    );
    assert_eq!(status, 503);
    assert!(started.elapsed() < Duration::from_millis(250));
    assert_eq!(read_response(first).0, 408);
    assert_eq!(read_response(second).0, 408);

    thread::sleep(Duration::from_millis(30));
    let mut oversized_header = b"GET /status HTTP/1.1\r\nX-Fill: ".to_vec();
    oversized_header.resize(16_384, b'x');
    let oversized_header = partial_request(listener.socket, &oversized_header);
    assert_eq!(read_response(oversized_header).0, 431);

    let body_stall = partial_request(
        listener.socket,
        format!(
            "POST /session HTTP/1.1\r\nHost: {host}\r\nAuthorization: {auth}\r\nContent-Type: application/json\r\nContent-Length: 16\r\nConnection: close\r\n\r\nx"
        )
        .as_bytes(),
    );
    body_stall
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    assert_eq!(read_response(body_stall).0, 408);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    listener.shutdown().unwrap();
}

#[test]
fn bounded_dispatch_queue_rejects_excess_concurrency() {
    let calls = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let (entered_send, entered_recv) = mpsc::channel();
    let mut listener = start_authenticated::<_, VoidWebDriverExtensionRoute>(
        policy(128, 1, Duration::from_secs(2)),
        CountingHandler {
            calls: Arc::clone(&calls),
            gate: Some(Arc::clone(&gate)),
            delay: Duration::ZERO,
            entered: Some(entered_send),
        },
        Vec::new(),
    )
    .unwrap();
    let address = listener.socket;
    let host = address.to_string();
    let auth = authorization();
    let first = thread::spawn({
        let host = host.clone();
        let auth = auth.clone();
        move || request(address, "GET", "/status", &host, Some(&auth), None, b"").0
    });
    entered_recv.recv_timeout(Duration::from_secs(1)).unwrap();
    let second = thread::spawn({
        let host = host.clone();
        let auth = auth.clone();
        move || request(address, "GET", "/status", &host, Some(&auth), None, b"").0
    });
    thread::sleep(Duration::from_millis(30));
    let third =
        thread::spawn(move || request(address, "GET", "/status", &host, Some(&auth), None, b"").0);
    thread::sleep(Duration::from_millis(30));
    {
        let (lock, wake) = &*gate;
        *lock.lock().unwrap() = true;
        wake.notify_all();
    }
    let statuses = [
        first.join().unwrap(),
        second.join().unwrap(),
        third.join().unwrap(),
    ];
    assert_eq!(statuses.iter().filter(|status| **status == 200).count(), 2);
    assert_eq!(statuses.iter().filter(|status| **status == 503).count(), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    listener.shutdown().unwrap();
}

#[test]
fn request_deadline_and_explicit_shutdown_are_bounded() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut listener = start_authenticated::<_, VoidWebDriverExtensionRoute>(
        policy(128, 1, Duration::from_millis(30)),
        CountingHandler {
            calls: Arc::clone(&calls),
            gate: None,
            delay: Duration::from_millis(80),
            entered: None,
        },
        Vec::new(),
    )
    .unwrap();
    let host = listener.socket.to_string();
    let auth = authorization();
    let (status, body) = request(
        listener.socket,
        "GET",
        "/status",
        &host,
        Some(&auth),
        None,
        b"",
    );
    assert_eq!(status, 500);
    assert!(body.contains("\"error\":\"timeout\""));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    listener.shutdown().unwrap();
    listener.shutdown().unwrap();
}

#[test]
fn queued_stateful_request_expiring_behind_blocker_never_reaches_handler() {
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let (entered_send, entered_recv) = mpsc::channel();
    let new_session_calls = Arc::new(AtomicUsize::new(0));
    let mut listener = start_authenticated::<_, VoidWebDriverExtensionRoute>(
        policy(256, 2, Duration::from_millis(120)),
        BlockingStatefulHandler {
            gate: Some(Arc::clone(&gate)),
            entered: Some(entered_send),
            new_session_calls: Arc::clone(&new_session_calls),
            next_session: 0,
        },
        Vec::new(),
    )
    .unwrap();
    let address = listener.socket;
    let host = address.to_string();
    let auth = authorization();
    let blocker = thread::spawn({
        let host = host.clone();
        let auth = auth.clone();
        move || request(address, "GET", "/status", &host, Some(&auth), None, b"")
    });
    entered_recv.recv_timeout(Duration::from_secs(1)).unwrap();
    let expired = thread::spawn({
        let host = host.clone();
        let auth = auth.clone();
        move || {
            request(
                address,
                "POST",
                "/session",
                &host,
                Some(&auth),
                None,
                br#"{"capabilities":{}}"#,
            )
        }
    });

    let (expired_status, expired_body) = expired.join().unwrap();
    assert_eq!(expired_status, 500);
    assert!(expired_body.contains("\"error\":\"timeout\""));
    assert_eq!(new_session_calls.load(Ordering::SeqCst), 0);
    {
        let (lock, wake) = &*gate;
        *lock.lock().unwrap() = true;
        wake.notify_all();
    }
    assert_eq!(blocker.join().unwrap().0, 500);
    thread::sleep(Duration::from_millis(30));
    assert_eq!(new_session_calls.load(Ordering::SeqCst), 0);

    let (status, body) = request(
        address,
        "POST",
        "/session",
        &host,
        Some(&auth),
        None,
        br#"{"capabilities":{}}"#,
    );
    assert_eq!(status, 200);
    assert!(body.contains("recovery-session-1"));
    assert_eq!(new_session_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        request(
            address,
            "DELETE",
            "/session/recovery-session-1",
            &host,
            Some(&auth),
            None,
            b"",
        )
        .0,
        200
    );
    listener.shutdown().unwrap();
}

#[test]
fn near_expiry_handler_inherits_outer_deadline_and_fresh_session_recovers() {
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let (entered_send, entered_recv) = mpsc::channel();
    let (remaining_send, remaining_recv) = mpsc::channel();
    let mutations = Arc::new(AtomicUsize::new(0));
    let mut listener = start_authenticated::<_, VoidWebDriverExtensionRoute>(
        policy(256, 2, Duration::from_millis(180)),
        NearExpiryHandler {
            gate: Arc::clone(&gate),
            entered: Some(entered_send),
            remaining: Some(remaining_send),
            expire_first_session: true,
            mutations: Arc::clone(&mutations),
        },
        Vec::new(),
    )
    .unwrap();
    let address = listener.socket;
    let host = address.to_string();
    let auth = authorization();
    let blocker = thread::spawn({
        let host = host.clone();
        let auth = auth.clone();
        move || request(address, "GET", "/status", &host, Some(&auth), None, b"")
    });
    entered_recv.recv_timeout(Duration::from_secs(1)).unwrap();
    let started = Instant::now();
    let expiring = thread::spawn({
        let host = host.clone();
        let auth = auth.clone();
        move || {
            request(
                address,
                "POST",
                "/session",
                &host,
                Some(&auth),
                None,
                br#"{"capabilities":{}}"#,
            )
        }
    });
    thread::sleep(Duration::from_millis(100));
    {
        let (lock, wake) = &*gate;
        *lock.lock().unwrap() = true;
        wake.notify_all();
    }
    assert_eq!(blocker.join().unwrap().0, 200);
    let inherited_remaining = remaining_recv.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(
        inherited_remaining < Duration::from_millis(120),
        "handler received a refreshed budget: {inherited_remaining:?}"
    );
    let (status, body) = expiring.join().unwrap();
    assert_eq!(status, 500);
    assert!(body.contains("\"error\":\"timeout\""));
    assert!(started.elapsed() < Duration::from_millis(320));
    assert_eq!(mutations.load(Ordering::SeqCst), 0);

    let (status, body) = request(
        address,
        "POST",
        "/session",
        &host,
        Some(&auth),
        None,
        br#"{"capabilities":{}}"#,
    );
    assert_eq!(status, 200);
    assert!(body.contains("near-expiry-recovery"));
    assert_eq!(mutations.load(Ordering::SeqCst), 1);
    assert_eq!(
        request(
            address,
            "DELETE",
            "/session/near-expiry-recovery",
            &host,
            Some(&auth),
            None,
            b"",
        )
        .0,
        200
    );
    listener.shutdown().unwrap();
}

#[test]
fn client_disconnect_cancels_exact_dispatch_and_server_recovers() {
    let (entered_send, entered_recv) = mpsc::channel();
    let calls = Arc::new(AtomicUsize::new(0));
    let mut listener = start_authenticated::<_, VoidWebDriverExtensionRoute>(
        policy(128, 1, Duration::from_secs(1)),
        DisconnectAwareHandler {
            entered: Some(entered_send),
            wait_for_disconnect: true,
            calls: Arc::clone(&calls),
        },
        Vec::new(),
    )
    .unwrap();
    let host = listener.socket.to_string();
    let auth = authorization();
    let mut disconnected = TcpStream::connect(listener.socket).unwrap();
    write!(
        disconnected,
        "GET /status HTTP/1.1\r\nHost: {host}\r\nAuthorization: {auth}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
    )
    .unwrap();
    disconnected.flush().unwrap();
    let lifetime = entered_recv.recv_timeout(Duration::from_secs(1)).unwrap();
    drop(disconnected);
    let cancellation_deadline = Instant::now() + Duration::from_millis(300);
    while !lifetime.is_cancelled() && Instant::now() < cancellation_deadline {
        thread::sleep(Duration::from_millis(1));
    }
    assert!(lifetime.is_cancelled());
    thread::sleep(Duration::from_millis(30));
    assert_eq!(
        request(
            listener.socket,
            "GET",
            "/status",
            &host,
            Some(&auth),
            None,
            b"",
        )
        .0,
        200
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    listener.shutdown().unwrap();
}

#[test]
fn post_completion_client_eof_revokes_exact_session_and_fresh_session_recovers() {
    let (authority_send, authority_recv) = mpsc::channel();
    let new_session_calls = Arc::new(AtomicUsize::new(0));
    let session_command_calls = Arc::new(AtomicUsize::new(0));
    let teardown_calls = Arc::new(AtomicUsize::new(0));
    let mut listener = start_authenticated::<_, VoidWebDriverExtensionRoute>(
        policy(256, 2, Duration::from_secs(5)),
        PostCompletionDeliveryHandler {
            authority: Some(authority_send),
            new_session_calls: Arc::clone(&new_session_calls),
            session_command_calls: Arc::clone(&session_command_calls),
            teardown_calls: Arc::clone(&teardown_calls),
        },
        Vec::new(),
    )
    .unwrap();
    let host = listener.socket.to_string();
    let auth = authorization();
    let mut abandoned = TcpStream::connect(listener.socket).unwrap();
    write!(
        abandoned,
        "POST /session HTTP/1.1\r\nHost: {host}\r\nAuthorization: {auth}\r\nConnection: close\r\nContent-Length: 19\r\n\r\n{{\"capabilities\":{{}}}}"
    )
    .unwrap();
    abandoned.flush().unwrap();
    let authority = authority_recv.recv_timeout(Duration::from_secs(3)).unwrap();

    let completion_deadline = Instant::now() + Duration::from_secs(3);
    while authority.remaining().is_some() && Instant::now() < completion_deadline {
        thread::sleep(Duration::from_millis(1));
    }
    assert!(authority.remaining().is_none());
    assert!(
        !authority.is_cancelled(),
        "the client must abandon only after dispatcher completion"
    );

    let queued_session_command = thread::spawn({
        let host = host.clone();
        let auth = auth.clone();
        let address = listener.socket;
        move || {
            request(
                address,
                "GET",
                "/session/abandoned-after-completion/url",
                &host,
                Some(&auth),
                None,
                b"",
            )
        }
    });
    thread::sleep(Duration::from_millis(50));
    assert_eq!(
        session_command_calls.load(Ordering::SeqCst),
        0,
        "a pending terminal response must gate the next session command"
    );
    abandoned.shutdown(std::net::Shutdown::Both).unwrap();
    drop(abandoned);

    let cancellation_deadline = Instant::now() + Duration::from_millis(500);
    while !authority.is_cancelled() && Instant::now() < cancellation_deadline {
        thread::sleep(Duration::from_millis(1));
    }
    assert!(authority.is_cancelled());
    let teardown_deadline = Instant::now() + Duration::from_millis(500);
    while teardown_calls.load(Ordering::SeqCst) == 0 && Instant::now() < teardown_deadline {
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(teardown_calls.load(Ordering::SeqCst), 1);
    assert_eq!(new_session_calls.load(Ordering::SeqCst), 1);
    let (queued_status, queued_body) = queued_session_command.join().unwrap();
    assert_eq!(queued_status, 404);
    assert!(queued_body.contains("invalid session id"));
    assert_eq!(session_command_calls.load(Ordering::SeqCst), 0);

    let (status, body) = request(
        listener.socket,
        "POST",
        "/session",
        &host,
        Some(&auth),
        None,
        br#"{"capabilities":{}}"#,
    );
    assert_eq!(status, 200);
    assert!(body.contains("delivery-recovery"));
    assert_eq!(new_session_calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        request(
            listener.socket,
            "DELETE",
            "/session/delivery-recovery",
            &host,
            Some(&auth),
            None,
            b"",
        )
        .0,
        200
    );
    listener.shutdown().unwrap();
}

#[test]
fn post_completion_socket_timeout_revokes_exact_session_and_fresh_session_recovers() {
    let (authority_send, authority_recv) = mpsc::channel();
    let new_session_calls = Arc::new(AtomicUsize::new(0));
    let session_command_calls = Arc::new(AtomicUsize::new(0));
    let teardown_calls = Arc::new(AtomicUsize::new(0));
    let mut listener = start_authenticated::<_, VoidWebDriverExtensionRoute>(
        policy(256, 2, Duration::from_secs(3)),
        PostCompletionDeliveryHandler {
            authority: Some(authority_send),
            new_session_calls: Arc::clone(&new_session_calls),
            session_command_calls: Arc::clone(&session_command_calls),
            teardown_calls: Arc::clone(&teardown_calls),
        },
        Vec::new(),
    )
    .unwrap();
    let host = listener.socket.to_string();
    let auth = authorization();
    let mut abandoned = TcpStream::connect(listener.socket).unwrap();
    write!(
        abandoned,
        "POST /session HTTP/1.1\r\nHost: {host}\r\nAuthorization: {auth}\r\nConnection: close\r\nContent-Length: 19\r\n\r\n{{\"capabilities\":{{}}}}"
    )
    .unwrap();
    abandoned.flush().unwrap();
    let authority = authority_recv.recv_timeout(Duration::from_secs(2)).unwrap();

    let completion_deadline = Instant::now() + Duration::from_secs(2);
    while authority.remaining().is_some() && Instant::now() < completion_deadline {
        thread::sleep(Duration::from_millis(1));
    }
    assert!(authority.remaining().is_none());
    assert!(
        !authority.is_cancelled(),
        "the socket must remain open until after dispatcher completion"
    );

    let cancellation_deadline = Instant::now() + Duration::from_secs(4);
    while !authority.is_cancelled() && Instant::now() < cancellation_deadline {
        thread::sleep(Duration::from_millis(1));
    }
    assert!(authority.is_cancelled());
    let teardown_deadline = Instant::now() + Duration::from_secs(1);
    while teardown_calls.load(Ordering::SeqCst) == 0 && Instant::now() < teardown_deadline {
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(teardown_calls.load(Ordering::SeqCst), 1);
    assert_eq!(new_session_calls.load(Ordering::SeqCst), 1);
    assert_eq!(session_command_calls.load(Ordering::SeqCst), 0);
    drop(abandoned);

    let (status, body) = request(
        listener.socket,
        "POST",
        "/session",
        &host,
        Some(&auth),
        None,
        br#"{"capabilities":{}}"#,
    );
    assert_eq!(status, 200);
    assert!(body.contains("delivery-recovery"));
    assert_eq!(new_session_calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        request(
            listener.socket,
            "DELETE",
            "/session/delivery-recovery",
            &host,
            Some(&auth),
            None,
            b"",
        )
        .0,
        200
    );
    listener.shutdown().unwrap();
}

#[test]
fn dispatcher_and_teardown_panics_revoke_authority_and_report_failure() {
    let authority = Arc::new(Mutex::new(None));
    let stateful_calls = Arc::new(AtomicUsize::new(0));
    let teardown_calls = Arc::new(AtomicUsize::new(0));
    let mut listener = start_authenticated::<_, VoidWebDriverExtensionRoute>(
        policy(256, 2, Duration::from_secs(1)),
        PanicAfterSessionHandler {
            authority: Arc::clone(&authority),
            stateful_calls: Arc::clone(&stateful_calls),
            teardown_calls: Arc::clone(&teardown_calls),
        },
        Vec::new(),
    )
    .unwrap();
    let host = listener.socket.to_string();
    let auth = authorization();
    let (status, body) = request(
        listener.socket,
        "POST",
        "/session",
        &host,
        Some(&auth),
        None,
        br#"{"capabilities":{}}"#,
    );
    assert_eq!(status, 200);
    assert!(body.contains("panic-session"));
    let captured = authority.lock().unwrap().clone().unwrap();
    assert!(!captured.is_cancelled());

    let (panic_status, _) = request(
        listener.socket,
        "GET",
        "/status",
        &host,
        Some(&auth),
        None,
        b"",
    );
    assert_eq!(panic_status, 500);
    let cancellation_deadline = Instant::now() + Duration::from_millis(300);
    while !captured.is_cancelled() && Instant::now() < cancellation_deadline {
        thread::sleep(Duration::from_millis(1));
    }
    assert!(captured.is_cancelled());
    assert_eq!(teardown_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        request(
            listener.socket,
            "POST",
            "/session/panic-session/url",
            &host,
            Some(&auth),
            None,
            br#"{"url":"http://late.invalid/"}"#,
        )
        .0,
        500
    );
    assert_eq!(stateful_calls.load(Ordering::SeqCst), 0);
    let shutdown_error = listener.shutdown().unwrap_err();
    assert!(
        shutdown_error
            .to_string()
            .contains("dispatcher thread panicked")
    );

    let recovery_calls = Arc::new(AtomicUsize::new(0));
    let mut recovery = start_authenticated::<_, VoidWebDriverExtensionRoute>(
        policy(256, 1, Duration::from_secs(1)),
        BlockingStatefulHandler {
            gate: None,
            entered: None,
            new_session_calls: Arc::clone(&recovery_calls),
            next_session: 0,
        },
        Vec::new(),
    )
    .unwrap();
    let recovery_host = recovery.socket.to_string();
    let (status, body) = request(
        recovery.socket,
        "POST",
        "/session",
        &recovery_host,
        Some(&auth),
        None,
        br#"{"capabilities":{}}"#,
    );
    assert_eq!(status, 200);
    assert!(body.contains("recovery-session-1"));
    assert_eq!(recovery_calls.load(Ordering::SeqCst), 1);
    recovery.shutdown().unwrap();
}
