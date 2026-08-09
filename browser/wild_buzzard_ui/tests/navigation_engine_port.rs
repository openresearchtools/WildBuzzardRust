use std::thread;

use wild_buzzard_dom::Document;
use wild_buzzard_engine::{
    CancellationToken, DocumentLoadProof, EngineFrame, EngineLimits, ExecutionFailure,
    ExecutorOutput, NavigationExecutor, NavigationId, NavigationRequest, PixelSize,
};
use wild_buzzard_ui::{
    BrowserCommandOutcome, BrowserSession, BrowserTabId, EnginePortStopReason, EnginePumpOutcome,
    NavigationEnginePort, SessionLimits,
};

struct PixelExecutor {
    document: Option<Document>,
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
    assert_eq!(frame.pixels(), &[1, 17, 34, 255]);

    let status = session.shutdown();
    assert_eq!(status.reason(), EnginePortStopReason::Requested);
}
