use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::Duration;

use wild_buzzard_dom::{Document, DocumentVersion};
use wild_buzzard_engine::{
    CancellationToken, DocumentLoadProof, DocumentOperationFailure, EngineFrame, EngineLimits,
    ExecutionFailure, ExecutorDocumentRerender, ExecutorOutput, FontSourcePolicy,
    NavigationCommitError, NavigationExecutor, NavigationGeneration, NavigationId,
    NavigationRequest, PixelSize, StaticPageConfig, TopLevelContextId,
};
use wild_buzzard_linux::BrowserNavigationIdentity;
use wild_buzzard_ui::{
    BrowserCommandOutcome, BrowserSession, BrowserTabId, BrowserWindowId, EngineDocumentVersion,
    EnginePort, EnginePortError, EnginePortEventKind, EnginePortStopReason, EnginePumpOutcome,
    NavigationEnginePort, SessionLimits, SessionPresentationError,
};

struct PixelExecutor {
    document: Option<Document>,
}

#[derive(Clone, Copy, Debug)]
enum RerenderResult {
    Rendered { width: u32 },
    Rejected(DocumentOperationFailure),
}

struct RerenderExecutor {
    document: Option<Document>,
    result: RerenderResult,
}

impl NavigationExecutor for RerenderExecutor {
    fn execute(
        &mut self,
        navigation: NavigationId,
        _request: &NavigationRequest,
        _cancellation: &CancellationToken,
    ) -> Result<ExecutorOutput, ExecutionFailure> {
        let document = Document::new();
        let proof = DocumentLoadProof::from_snapshot(&document.snapshot().unwrap());
        let frame = EngineFrame::from_rgba8_for_document(
            PixelSize::new(1, 1).unwrap(),
            vec![
                u8::try_from(navigation.generation().get()).unwrap_or(u8::MAX),
                17,
                34,
                255,
            ],
            document.version(),
        )
        .unwrap();
        self.document = Some(document);
        ExecutorOutput::new_document(200, frame, proof)
    }

    fn rerender_document(
        &mut self,
        _navigation: NavigationId,
        expected_live_version: DocumentVersion,
        _cancellation: &CancellationToken,
    ) -> ExecutorDocumentRerender {
        let version = self.document.as_ref().unwrap().version();
        assert_eq!(version, expected_live_version);
        match self.result {
            RerenderResult::Rendered { width } => ExecutorDocumentRerender::Rendered {
                live_version: version,
                previous_frame_version: version,
                frame: EngineFrame::from_rgba8_for_document(
                    PixelSize::new(width, 1).unwrap(),
                    vec![51; usize::try_from(width).unwrap() * 4],
                    version,
                )
                .unwrap(),
            },
            RerenderResult::Rejected(failure) => ExecutorDocumentRerender::Rejected {
                live_version: Some(version),
                frame_version: Some(version),
                failure,
            },
        }
    }

    fn shutdown(&mut self) -> Result<(), ExecutionFailure> {
        Ok(())
    }
}

impl NavigationExecutor for PixelExecutor {
    fn execute(
        &mut self,
        navigation: NavigationId,
        _request: &NavigationRequest,
        _cancellation: &CancellationToken,
    ) -> Result<ExecutorOutput, ExecutionFailure> {
        let pixel = [
            u8::try_from(navigation.generation().get()).unwrap_or(u8::MAX),
            17,
            34,
            255,
        ];
        let document = Document::new();
        let proof = DocumentLoadProof::from_snapshot(&document.snapshot().unwrap());
        let frame = EngineFrame::from_rgba8_for_document(
            PixelSize::new(1, 1).unwrap(),
            pixel.to_vec(),
            document.version(),
        )
        .expect("fixed one-pixel document frame is valid");
        self.document = Some(document);
        ExecutorOutput::new_document(200, frame, proof)
    }

    fn shutdown(&mut self) -> Result<(), ExecutionFailure> {
        Ok(())
    }
}

#[test]
fn concrete_navigation_port_drives_a_real_worker_frame_lease() {
    let engine_limits = EngineLimits::new(4, 16, 4, 4, 16).unwrap();
    let port = NavigationEnginePort::spawn_with_executor(engine_limits, || {
        Ok(PixelExecutor { document: None })
    })
    .expect("deterministic real navigation worker starts");
    let session_limits = SessionLimits::new(2, 8, 8, 8, 8, 4_096, 4_096, 4_096, 8).unwrap();
    let mut session = BrowserSession::new(port, session_limits).unwrap();
    let tab = BrowserTabId::new(1).unwrap();
    let navigation = match session
        .navigate_new(tab, "https://deterministic.invalid/")
        .unwrap()
    {
        BrowserCommandOutcome::NavigationQueued { navigation, .. } => navigation,
        other => panic!("unexpected navigation outcome: {other:?}"),
    };

    let mut received = false;
    for _ in 0..100_000 {
        match session.poll_engine_once().unwrap() {
            EnginePumpOutcome::Applied if session.frame(tab).unwrap().is_some() => {
                received = true;
                break;
            }
            EnginePumpOutcome::Empty => thread::yield_now(),
            EnginePumpOutcome::Applied
            | EnginePumpOutcome::StaleSuppressed
            | EnginePumpOutcome::RetiredContextSuppressed { .. }
            | EnginePumpOutcome::ContextCloseAcknowledged { .. }
            | EnginePumpOutcome::FrameSuppressedByResourceLimit { .. }
            | EnginePumpOutcome::MutationAppliedFrameSuppressed { .. } => {}
            EnginePumpOutcome::Batch { .. } => panic!("single poll returned a batch outcome"),
        }
    }
    assert!(
        received,
        "worker did not publish within the bounded poll budget"
    );
    let frame = session.frame(tab).unwrap().unwrap();
    assert_eq!(frame.navigation(), navigation);
    assert_eq!(frame.rgba8_pixels(), Some(&[1, 17, 34, 255][..]));

    let status = session.shutdown();
    assert_eq!(status.reason(), EnginePortStopReason::Requested);
}

#[test]
fn concrete_port_binds_one_exact_commitment_before_returning_the_commit_event() {
    let mut port = NavigationEnginePort::spawn_with_executor(
        EngineLimits::new(4, 16, 4, 4, 16).unwrap(),
        || Ok(PixelExecutor { document: None }),
    )
    .unwrap();
    let context = TopLevelContextId::new(41).unwrap();
    let requested = "https://deterministic.invalid/requested";
    let navigation = port
        .navigate(context, NavigationRequest::new(requested).unwrap())
        .unwrap();
    let mut saw_commit = false;
    for _ in 0..100_000 {
        let Some(event) = port.poll_event().unwrap() else {
            thread::yield_now();
            continue;
        };
        if matches!(
            event.kind(),
            EnginePortEventKind::NavigationCommitted { .. }
        ) {
            let foreign = NavigationId::new(
                TopLevelContextId::new(42).unwrap(),
                NavigationGeneration::INITIAL,
            );
            assert!(matches!(
                port.take_navigation_commit(foreign),
                Err(EnginePortError::NavigationCommit(
                    NavigationCommitError::Unknown
                ))
            ));
            let commitment = port
                .take_navigation_commit(navigation)
                .unwrap()
                .expect("the concrete port always transfers exact commitment metadata");
            assert_eq!(commitment.final_url(), requested);
            assert!(matches!(
                port.take_navigation_commit(navigation),
                Err(EnginePortError::NavigationCommit(
                    NavigationCommitError::Unknown
                ))
            ));
            saw_commit = true;
            break;
        }
    }
    assert!(saw_commit, "worker did not publish the commitment event");
    assert_eq!(port.shutdown().reason(), EnginePortStopReason::Requested);
}

#[test]
fn presentation_rerender_terminal_names_exact_success_rejection_and_budget_suppression() {
    let cases = [
        (RerenderResult::Rendered { width: 1 }, 4_096, None),
        (
            RerenderResult::Rejected(DocumentOperationFailure::Rendering),
            4_096,
            Some(DocumentOperationFailure::Rendering),
        ),
        (
            RerenderResult::Rendered { width: 2 },
            4,
            Some(DocumentOperationFailure::ResourceLimit),
        ),
    ];

    for (result, frame_limit, expected_failure) in cases {
        let port = NavigationEnginePort::spawn_with_executor(
            EngineLimits::new(4, 16, 4, 64, 64).unwrap(),
            move || {
                Ok(RerenderExecutor {
                    document: None,
                    result,
                })
            },
        )
        .unwrap();
        let limits = SessionLimits::new(2, 8, 8, 8, 8, 4_096, frame_limit, 4_096, 8).unwrap();
        let mut session = BrowserSession::new(port, limits).unwrap();
        let tab = BrowserTabId::new(1).unwrap();
        assert!(matches!(
            session
                .navigate_new(tab, "https://rerender-terminal.invalid/")
                .unwrap(),
            BrowserCommandOutcome::NavigationQueued { .. }
        ));
        wait_for_frame(&mut session, tab);

        let operation = session.request_presentation_rerender(tab).unwrap();
        assert_eq!(
            session
                .tab_snapshot(tab)
                .unwrap()
                .last_presentation_rerender,
            None
        );
        let terminal_outcome = loop {
            let outcome = session
                .poll_engine_once()
                .unwrap_or_else(|error| panic!("{result:?} rerender failed: {error:?}"));
            if session
                .tab_snapshot(tab)
                .unwrap()
                .last_presentation_rerender
                .is_some()
            {
                break outcome;
            }
            if matches!(outcome, EnginePumpOutcome::Empty) {
                thread::yield_now();
            }
        };
        let terminal = session
            .tab_snapshot(tab)
            .unwrap()
            .last_presentation_rerender
            .unwrap();
        assert_eq!(terminal.operation(), operation);
        assert_eq!(terminal.failure(), expected_failure);
        if expected_failure == Some(DocumentOperationFailure::ResourceLimit) {
            assert!(matches!(
                terminal_outcome,
                EnginePumpOutcome::FrameSuppressedByResourceLimit { .. }
            ));
            assert!(session.frame(tab).unwrap().is_none());
        } else {
            assert_eq!(terminal_outcome, EnginePumpOutcome::Applied);
        }
        let _ = session.shutdown();
    }
}

fn serve_presentation_page() -> (String, thread::JoinHandle<()>) {
    const BODY: &[u8] = br"<!doctype html><style>html,body{margin:0}main{display:block;width:160px;height:80px;background:#17406b;color:white}</style><main>Rust page scene</main>";
    let listener = TcpListener::bind("127.0.0.1:0").expect("numeric loopback binds");
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("presentation client connects");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        consume_request(&mut stream);
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            BODY.len()
        );
        stream.write_all(head.as_bytes()).unwrap();
        stream.write_all(BODY).unwrap();
    });
    (format!("http://{address}/scene.html"), server)
}

fn consume_request(stream: &mut TcpStream) {
    let mut request = Vec::new();
    let mut chunk = [0_u8; 256];
    while !request.ends_with(b"\r\n\r\n") {
        let count = stream.read(&mut chunk).expect("request head reads");
        assert!(count > 0, "request head terminates");
        request.extend_from_slice(&chunk[..count]);
        assert!(request.len() <= 8 * 1024, "request head stays bounded");
    }
    assert!(request.starts_with(b"GET /scene.html HTTP/1.1\r\n"));
}

fn wait_for_frame(session: &mut BrowserSession<NavigationEnginePort>, tab: BrowserTabId) {
    for _ in 0..100_000 {
        if session.frame(tab).unwrap().is_some() {
            return;
        }
        match session.poll_engine_once().unwrap() {
            EnginePumpOutcome::Empty => thread::yield_now(),
            EnginePumpOutcome::Batch { .. } => panic!("single poll returned a batch outcome"),
            _ => {}
        }
    }
    panic!("presentation worker did not publish within its bounded poll budget");
}

#[test]
fn presentation_transfer_rejects_foreign_labels_without_consuming_the_candidate() {
    let (url, server) = serve_presentation_page();
    let config = StaticPageConfig {
        viewport_width: 320,
        viewport_height: 180,
        font_source: FontSourcePolicy::EmbeddedOnly,
        ..StaticPageConfig::default()
    };
    let port = NavigationEnginePort::spawn_for_presentation(config, EngineLimits::default())
        .expect("presentation worker starts without a headless renderer");
    let mut session = BrowserSession::new(port, SessionLimits::default()).unwrap();
    let first = BrowserTabId::new(1).unwrap();
    let navigation = match session.navigate_new(first, &url).unwrap() {
        BrowserCommandOutcome::NavigationQueued { navigation, .. } => navigation,
        other => panic!("unexpected navigation outcome: {other:?}"),
    };
    wait_for_frame(&mut session, first);
    server.join().unwrap();

    let snapshot = session.tab_snapshot(first).unwrap();
    let document = snapshot.engine_frame_version.unwrap();
    let descriptor = session
        .frame(first)
        .unwrap()
        .unwrap()
        .descriptor()
        .presentation_scene()
        .expect("presentation worker did not rasterize RGBA8 pixels");
    let retained = session.retained_frame_bytes();
    assert_eq!(descriptor.document_version(), document);
    assert!(retained > 0);

    assert!(matches!(
        session.take_presentation_scene(
            first,
            navigation,
            document,
            descriptor.scene_revision() + 1,
            BrowserNavigationIdentity::new(1).unwrap(),
        ),
        Err(SessionPresentationError::CandidateIdentityMismatch)
    ));
    assert!(session.frame(first).unwrap().is_some());
    assert_eq!(session.retained_frame_bytes(), retained);

    let stale_document = EngineDocumentVersion::new(document.document(), document.revision() + 1);
    assert!(matches!(
        session.take_presentation_scene(
            first,
            navigation,
            stale_document,
            descriptor.scene_revision(),
            BrowserNavigationIdentity::new(1).unwrap(),
        ),
        Err(SessionPresentationError::CandidateIdentityMismatch)
    ));
    assert!(session.frame(first).unwrap().is_some());
    assert_eq!(session.retained_frame_bytes(), retained);

    let second = match session.open_tab(BrowserWindowId::new(1).unwrap()).unwrap() {
        BrowserCommandOutcome::TabOpened { tab, .. } => tab,
        other => panic!("unexpected tab outcome: {other:?}"),
    };
    let foreign_navigation = match session
        .navigate_new(second, "http://127.0.0.1:9/cross-tab")
        .unwrap()
    {
        BrowserCommandOutcome::NavigationQueued { navigation, .. } => navigation,
        other => panic!("unexpected navigation outcome: {other:?}"),
    };
    assert!(matches!(
        session.take_presentation_scene(
            first,
            foreign_navigation,
            document,
            descriptor.scene_revision(),
            BrowserNavigationIdentity::new(1).unwrap(),
        ),
        Err(SessionPresentationError::CandidateIdentityMismatch)
    ));
    assert!(session.frame(first).unwrap().is_some());
    assert_eq!(session.retained_frame_bytes(), retained);

    let page = session
        .take_presentation_scene(
            first,
            navigation,
            document,
            descriptor.scene_revision(),
            BrowserNavigationIdentity::new(7).unwrap(),
        )
        .unwrap()
        .expect("exact presentation candidate transfers once");
    assert_eq!(page.identity().navigation().get(), 7);
    assert_eq!(
        page.identity().revision().get(),
        descriptor.scene_revision()
    );
    assert!(session.frame(first).unwrap().is_none());
    assert_eq!(session.retained_frame_bytes(), 0);

    let status = session.shutdown();
    assert_eq!(status.reason(), EnginePortStopReason::Requested);
}
