use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

use wild_buzzard_engine::{
    EngineLimits, FontSourcePolicy, GeneralWebConfig, StaticPageConfig, TrustStore,
};
use wild_buzzard_linux::BrowserNavigationIdentity;
use wild_buzzard_ui::{
    BrowserCommandOutcome, BrowserNavigationMode, BrowserSession, BrowserTabId, BrowserWindowId,
    EnginePumpOutcome, NavigationEnginePort, NavigationPhase, SessionError, SessionLimits,
    SessionPresentationError,
};

struct RedirectServer {
    address: SocketAddr,
    thread: Option<thread::JoinHandle<()>>,
}

impl RedirectServer {
    fn start() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let thread = thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                serve(&mut stream, address);
            }
        });
        Self {
            address,
            thread: Some(thread),
        }
    }

    fn requested_url(&self) -> String {
        format!("http://{}/redirect", self.address)
    }

    fn final_url(&self) -> String {
        format!("http://{}/final", self.address)
    }
}

impl Drop for RedirectServer {
    fn drop(&mut self) {
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

fn serve(stream: &mut TcpStream, address: SocketAddr) {
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
        let body = b"<!doctype html><title>Committed</title><p>final</p>";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(response.as_bytes()).unwrap();
        stream.write_all(body).unwrap();
    }
    stream.flush().unwrap();
}

fn presentation_page() -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        serve(&mut stream, address);
    });
    (format!("http://{address}/scene"), server)
}

#[test]
fn committed_url_is_redirect_final_and_ignores_a_dirty_address_draft() {
    let server = RedirectServer::start();
    let port = NavigationEnginePort::spawn_general_web(
        StaticPageConfig::default(),
        GeneralWebConfig::default(),
        TrustStore::default(),
        EngineLimits::default(),
    )
    .unwrap();
    let limits = SessionLimits::new(1, 4, 4, 4, 16, 32_768, 32_768, 4_096, 32).unwrap();
    let mut session =
        BrowserSession::new_with_navigation_mode(port, limits, BrowserNavigationMode::GeneralWeb)
            .unwrap();
    let tab = BrowserTabId::new(1).unwrap();
    assert_eq!(session.committed_url(tab).unwrap(), None);
    let navigation = match session.navigate_new(tab, &server.requested_url()).unwrap() {
        BrowserCommandOutcome::NavigationQueued { navigation, .. } => navigation,
        other => panic!("unexpected navigation outcome: {other:?}"),
    };
    let mut ready = false;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        match session.pump_engine(32).unwrap() {
            EnginePumpOutcome::Empty => thread::sleep(Duration::from_millis(1)),
            EnginePumpOutcome::Batch { .. } => {}
            other => panic!("bounded engine pump returned an unexpected outcome: {other:?}"),
        }
        let snapshot = session.tab_snapshot(tab).unwrap();
        if snapshot.latest_navigation == Some(navigation)
            && snapshot.latest_navigation_phase == Some(NavigationPhase::Ready)
        {
            ready = true;
            break;
        }
    }
    assert!(ready, "redirect navigation did not reach Ready");
    assert_eq!(
        session.committed_url(tab).unwrap().as_deref(),
        Some(server.final_url().as_str())
    );

    let draft = "https://draft.invalid/not-document-state";
    let address = session.address_mut(tab).unwrap();
    address.select_all();
    address.insert(draft).unwrap();
    let snapshot = session.tab_snapshot(tab).unwrap();
    assert!(snapshot.address_dirty);
    assert_eq!(snapshot.address.as_ref(), draft);
    assert_eq!(
        session.committed_url(tab).unwrap().as_deref(),
        Some(server.final_url().as_str())
    );

    let window = BrowserWindowId::new(1).unwrap();
    let replacement = match session.open_tab(window).unwrap() {
        BrowserCommandOutcome::TabOpened { tab, .. } => tab,
        other => panic!("unexpected tab-open outcome: {other:?}"),
    };
    session.activate_tab(replacement).unwrap();
    session.close_tab(tab).unwrap();
    assert!(matches!(
        session.committed_url(tab),
        Err(SessionError::UnknownTab(candidate)) if candidate == tab
    ));
    let _ = session.shutdown();
}

#[test]
fn exact_presentation_candidate_labels_are_revalidated_before_transfer() {
    let (url, server) = presentation_page();
    let page = StaticPageConfig {
        viewport_width: 320,
        viewport_height: 180,
        font_source: FontSourcePolicy::EmbeddedOnly,
        ..StaticPageConfig::default()
    };
    let port = NavigationEnginePort::spawn_for_presentation(page, EngineLimits::default()).unwrap();
    let mut session = BrowserSession::new(port, SessionLimits::default()).unwrap();
    let tab = BrowserTabId::new(1).unwrap();
    let navigation = match session.navigate_new(tab, &url).unwrap() {
        BrowserCommandOutcome::NavigationQueued { navigation, .. } => navigation,
        other => panic!("unexpected navigation outcome: {other:?}"),
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        match session.pump_engine(32).unwrap() {
            EnginePumpOutcome::Empty => thread::sleep(Duration::from_millis(1)),
            EnginePumpOutcome::Batch { .. } => {}
            other => panic!("bounded engine pump returned an unexpected outcome: {other:?}"),
        }
        let snapshot = session.tab_snapshot(tab).unwrap();
        if snapshot.latest_navigation_phase == Some(NavigationPhase::Ready)
            && snapshot.frame.is_some()
        {
            break;
        }
    }
    server.join().unwrap();
    let labels = session
        .presentation_candidate_labels(tab)
        .unwrap()
        .expect("presentation navigation retained one exact candidate");
    assert_eq!(labels.0, navigation);
    assert_eq!(
        session.committed_url(tab).unwrap().as_deref(),
        Some(url.as_str())
    );
    let retained = session.retained_frame_bytes();
    let wrong = (labels.0, labels.1, labels.2, labels.3 + 1);
    assert!(matches!(
        session.take_exact_presentation_scene(
            tab,
            wrong,
            BrowserNavigationIdentity::new(1).unwrap(),
        ),
        Err(SessionPresentationError::CandidateIdentityMismatch)
    ));
    assert_eq!(
        session.presentation_candidate_labels(tab).unwrap(),
        Some(labels)
    );
    assert_eq!(session.retained_frame_bytes(), retained);

    let page = session
        .take_exact_presentation_scene(tab, labels, BrowserNavigationIdentity::new(7).unwrap())
        .unwrap()
        .expect("the exact inspected candidate transfers once");
    assert_eq!(page.identity().navigation().get(), 7);
    assert_eq!(page.identity().revision().get(), labels.3);
    assert!(session.frame(tab).unwrap().is_none());
    assert_eq!(session.retained_frame_bytes(), 0);
    let _ = session.shutdown();
}
