#![cfg(feature = "webdriver")]

use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use webdriver::server::{BearerToken, ServerSecurityPolicy};
use wild_buzzard_shell::BrowserWebDriverConfig;

const TOKEN: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";

#[test]
fn webdriver_configuration_is_explicit_loopback_only_and_secret_redacted() {
    let token = BearerToken::from_lower_hex(TOKEN.as_bytes()).unwrap();
    let policy = ServerSecurityPolicy::new("127.0.0.1:4444".parse().unwrap(), token).unwrap();
    let config = BrowserWebDriverConfig::new(policy).unwrap();
    assert!(!format!("{config:?}").contains(TOKEN));

    let ephemeral = ServerSecurityPolicy::new(
        "127.0.0.1:0".parse().unwrap(),
        BearerToken::from_lower_hex(TOKEN.as_bytes()).unwrap(),
    )
    .unwrap();
    assert!(BrowserWebDriverConfig::new(ephemeral).is_err());
}

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct TokenFileGuard(PathBuf);

impl TokenFileGuard {
    fn create(output: &Path) -> Self {
        let path = output.join("webdriver-token");
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        file.write_all(TOKEN.as_bytes()).unwrap();
        file.flush().unwrap();
        drop(file);
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TokenFileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

struct RedirectFixture {
    address: SocketAddr,
    running: Arc<AtomicBool>,
    served: Arc<AtomicUsize>,
    thread: Option<thread::JoinHandle<()>>,
}

impl RedirectFixture {
    fn start() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let running = Arc::new(AtomicBool::new(true));
        let served = Arc::new(AtomicUsize::new(0));
        let thread_running = Arc::clone(&running);
        let thread_served = Arc::clone(&served);
        let thread = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(40);
            while thread_served.load(Ordering::Acquire) < 2
                && thread_running.load(Ordering::Acquire)
                && Instant::now() < deadline
            {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        serve_fixture(&mut stream, address);
                        thread_served.fetch_add(1, Ordering::AcqRel);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("redirect fixture failed: {error}"),
                }
            }
        });
        Self {
            address,
            running,
            served,
            thread: Some(thread),
        }
    }

    fn requested_url(&self) -> String {
        format!("http://{}/redirect", self.address)
    }

    fn final_url(&self) -> String {
        format!("http://{}/final", self.address)
    }

    fn assert_completed(&self) {
        assert_eq!(
            self.served.load(Ordering::Acquire),
            2,
            "browser did not complete the redirect fixture"
        );
    }
}

impl Drop for RedirectFixture {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve_fixture(stream: &mut TcpStream, address: SocketAddr) {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut request = [0_u8; 4_096];
    let size = stream.read(&mut request).unwrap();
    let request = String::from_utf8_lossy(&request[..size]);
    if request.starts_with("GET /redirect ") {
        let response = format!(
            "HTTP/1.1 302 Found\r\nLocation: http://{address}/final\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        stream.write_all(response.as_bytes()).unwrap();
    } else {
        let body = b"<!doctype html><title>Automation final</title><main>receipt bound</main>";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(response.as_bytes()).unwrap();
        stream.write_all(body).unwrap();
    }
    stream.flush().unwrap();
}

struct HttpResponse {
    status: u16,
    body: Value,
}

fn webdriver_request(
    address: SocketAddr,
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> HttpResponse {
    let body = body.map_or_else(String::new, Value::to_string);
    let mut stream = TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(40)))
        .unwrap();
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer {TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).unwrap();
    stream.flush().unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let (head, body) = response.split_once("\r\n\r\n").unwrap();
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse().ok())
        .unwrap();
    HttpResponse {
        status,
        body: serde_json::from_str(body).unwrap(),
    }
}

fn wait_for_webdriver(address: SocketAddr, browser: &mut Child, stderr_path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if probe_webdriver_status(address) {
            return;
        }
        if let Some(status) = browser.try_wait().unwrap() {
            panic!(
                "Wild Buzzard exited before WebDriver startup with {status}; stderr: {}",
                stderr_path.display()
            );
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("embedded WebDriver listener did not become reachable");
}

fn probe_webdriver_status(address: SocketAddr) -> bool {
    let Ok(mut stream) = TcpStream::connect(address) else {
        return false;
    };
    if stream
        .set_read_timeout(Some(Duration::from_secs(1)))
        .is_err()
    {
        return false;
    }
    let request = format!(
        "GET /status HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer {TOKEN}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    if stream.write_all(request.as_bytes()).is_err() || stream.flush().is_err() {
        return false;
    }
    let mut response = [0_u8; 64];
    stream
        .read(&mut response)
        .is_ok_and(|size| response[..size].starts_with(b"HTTP/1.1 200"))
}

fn start_live_browser(output: &Path) -> (SocketAddr, ChildGuard, TokenFileGuard) {
    let token_file = TokenFileGuard::create(output);
    let reserved = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = reserved.local_addr().unwrap();
    drop(reserved);

    let stdout_path = output.join("wild-buzzard.stdout.log");
    let stderr_path = output.join("wild-buzzard.stderr.log");
    let stdout = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(stdout_path)
        .unwrap();
    let stderr = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&stderr_path)
        .unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_wild-buzzard"))
        .args([
            "--webdriver-loopback-address",
            &address.to_string(),
            "--webdriver-token-file",
            token_file.path().to_str().unwrap(),
        ])
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .unwrap();
    let mut child = ChildGuard(child);
    wait_for_webdriver(address, &mut child.0, &stderr_path);
    (address, child, token_file)
}

/// Live-display acceptance for the real executable and native compositor.
///
/// This is ignored in ordinary headless test runs. The recorded W9-A6M gate
/// runs it explicitly with a live Wayland or X11 session and a Data-drive
/// output directory. Process termination is test cleanup after Delete Session;
/// separate owner/server tests prove graceful pending-call shutdown.
#[test]
#[ignore = "requires WILDBUZZARD_REAL_DISPLAY_TEST=1 and a live Linux display"]
fn real_browser_classic_flow_waits_for_exact_native_composition() {
    assert_eq!(
        std::env::var("WILDBUZZARD_REAL_DISPLAY_TEST").as_deref(),
        Ok("1")
    );
    let output = PathBuf::from(
        std::env::var("WILDBUZZARD_TEST_OUTPUT_DIR")
            .expect("live WebDriver test requires an external output directory"),
    );
    std::fs::create_dir_all(&output).unwrap();
    let fixture = RedirectFixture::start();
    let (webdriver_address, _child, _token_file) = start_live_browser(&output);

    let status = webdriver_request(webdriver_address, "GET", "/status", None);
    assert_eq!(status.status, 200);
    assert_eq!(status.body["value"]["ready"], true);

    let bidi = webdriver_request(
        webdriver_address,
        "POST",
        "/session",
        Some(&json!({"capabilities": {"alwaysMatch": {"webSocketUrl": true}}})),
    );
    assert_eq!(bidi.status, 500);
    assert_eq!(bidi.body["value"]["error"], "session not created");

    let created = webdriver_request(
        webdriver_address,
        "POST",
        "/session",
        Some(&json!({"capabilities": {}})),
    );
    assert_eq!(created.status, 200);
    assert!(
        created.body["value"]["capabilities"]
            .get("webSocketUrl")
            .is_none()
    );
    assert_eq!(
        created.body["value"]["capabilities"]["timeouts"]["pageLoad"],
        30_000
    );
    let session_id = created.body["value"]["sessionId"].as_str().unwrap();

    let navigation = webdriver_request(
        webdriver_address,
        "POST",
        &format!("/session/{session_id}/url"),
        Some(&json!({"url": fixture.requested_url()})),
    );
    assert_eq!(navigation.status, 200);
    assert!(navigation.body["value"].is_null());

    let current = webdriver_request(
        webdriver_address,
        "GET",
        &format!("/session/{session_id}/url"),
        None,
    );
    assert_eq!(current.status, 200);
    assert_eq!(current.body["value"], fixture.final_url());

    for endpoint in ["title", "screenshot", "source"] {
        let unsupported = webdriver_request(
            webdriver_address,
            "GET",
            &format!("/session/{session_id}/{endpoint}"),
            None,
        );
        assert_eq!(unsupported.status, 500);
        assert_eq!(unsupported.body["value"]["error"], "unsupported operation");
    }

    let deleted = webdriver_request(
        webdriver_address,
        "DELETE",
        &format!("/session/{session_id}"),
        None,
    );
    assert_eq!(deleted.status, 200);
    assert!(deleted.body["value"].is_null());
    let status = webdriver_request(webdriver_address, "GET", "/status", None);
    assert_eq!(status.body["value"]["ready"], true);
    fixture.assert_completed();
}
