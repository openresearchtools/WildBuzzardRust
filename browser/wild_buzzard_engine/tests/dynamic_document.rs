use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::Duration;

use wild_buzzard_dom::bindings::{
    CreatedNodeToken, ScriptMutationBatch, ScriptMutationCommand, ScriptMutationError, ScriptNode,
};
use wild_buzzard_dom::{Document, DocumentVersion, DomError, NodeId};
use wild_buzzard_engine::{
    CancellationSource, DocumentUpdateError, DocumentUpdateRejection, FontSourcePolicy,
    PipelineError, RenderedStaticPage, StaticPageConfig, StaticPageEngine,
};
use wild_buzzard_headless::{HeadlessError, ResourceKind};

const WIDTH: u32 = 128;
const HEIGHT: u32 = 72;
const INITIAL_PANEL: [u8; 4] = [140, 20, 20, 255];
const UPDATED_PANEL: [u8; 4] = [20, 120, 200, 255];

const DOCUMENT: &str = r#"<!doctype html>
<style>
  html, body { margin: 0; }
  #panel {
    display: block;
    width: 96px;
    height: 48px;
    background-color: rgb(140 20 20);
    color: white;
    font-size: 14px;
    line-height: 20px;
  }
</style>
<div id="panel">base</div>"#;

fn config() -> StaticPageConfig {
    StaticPageConfig {
        viewport_width: WIDTH,
        viewport_height: HEIGHT,
        operation_timeout: Duration::from_secs(15),
        font_source: FontSourcePolicy::EmbeddedOnly,
        network: wild_buzzard_net::ClientConfig::default()
            .with_max_body_bytes(64 * 1024)
            .with_connect_timeout(Duration::from_secs(1))
            .with_read_timeout(Duration::from_secs(2))
            .with_write_timeout(Duration::from_secs(2)),
        headless: wild_buzzard_headless::HeadlessLimits::default()
            .with_max_width(WIDTH)
            .with_max_height(HEIGHT)
            .with_max_pixel_bytes(WIDTH as usize * HEIGHT as usize * 4),
        ..StaticPageConfig::default()
    }
}

fn serve_response(status: &'static str, body: &'static str) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("numeric loopback must bind");
    let address = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("page load must connect once");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        consume_request_head(&mut stream);
        let head = format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes()).unwrap();
        stream.write_all(body.as_bytes()).unwrap();
    });
    (format!("http://{address}/dynamic.html"), handle)
}

fn consume_request_head(stream: &mut TcpStream) {
    let mut received = Vec::new();
    let mut chunk = [0_u8; 256];
    while !received.ends_with(b"\r\n\r\n") {
        let count = stream.read(&mut chunk).expect("request head must read");
        assert!(count > 0, "request must contain a complete HTTP head");
        received.extend_from_slice(&chunk[..count]);
        assert!(received.len() <= 8 * 1024, "request head must be bounded");
    }
    assert!(received.starts_with(b"GET /dynamic.html HTTP/1.1\r\n"));
}

fn request_page(
    engine: &mut StaticPageEngine,
    status: &'static str,
    body: &'static str,
) -> Result<RenderedStaticPage, PipelineError> {
    let (url, server) = serve_response(status, body);
    let result = engine.load(&url, &CancellationSource::new().token());
    server.join().unwrap();
    result
}

fn load_page(engine: &mut StaticPageEngine) -> RenderedStaticPage {
    request_page(engine, "200 OK", DOCUMENT).expect("initial live page must render")
}

fn panel(engine: &StaticPageEngine) -> NodeId {
    engine
        .live_document()
        .unwrap()
        .element_by_id("panel")
        .unwrap()
        .unwrap()
}

fn live_versions(engine: &StaticPageEngine) -> (DocumentVersion, DocumentVersion) {
    let live = engine.live_document().unwrap();
    (live.live_version(), live.last_returned_frame_version())
}

fn color_update(version: DocumentVersion, panel: NodeId) -> ScriptMutationBatch {
    let element = CreatedNodeToken::from_index(0);
    let text = CreatedNodeToken::from_index(1);
    ScriptMutationBatch::new(
        version,
        vec![
            ScriptMutationCommand::CreateHtmlElement {
                token: element,
                local_name: "span".into(),
            },
            ScriptMutationCommand::CreateText {
                token: text,
                data: " dynamic".into(),
            },
            ScriptMutationCommand::AppendChild {
                parent: ScriptNode::Created(element),
                child: ScriptNode::Created(text),
            },
            ScriptMutationCommand::AppendChild {
                parent: ScriptNode::Existing(panel),
                child: ScriptNode::Created(element),
            },
            ScriptMutationCommand::SetHtmlAttribute {
                element: ScriptNode::Existing(panel),
                local_name: "style".into(),
                value: "background-color: rgb(20 120 200)".into(),
            },
        ],
    )
}

fn no_op_batch(version: DocumentVersion, panel: NodeId) -> ScriptMutationBatch {
    ScriptMutationBatch::new(
        version,
        vec![ScriptMutationCommand::RemoveHtmlAttribute {
            element: ScriptNode::Existing(panel),
            local_name: "data-never-present".into(),
        }],
    )
}

#[test]
fn no_live_document_precedes_cancellation_for_both_dynamic_apis() {
    let mut engine = StaticPageEngine::new(config()).expect("Linux EGL pbuffer must initialize");
    let version = Document::new().version();
    let cancellation = CancellationSource::new();
    assert!(cancellation.cancel());

    let mutation = ScriptMutationBatch::new(
        version,
        vec![ScriptMutationCommand::CreateText {
            token: CreatedNodeToken::from_index(0),
            data: "never applied".into(),
        }],
    );
    assert!(matches!(
        engine.apply_and_render(mutation, &cancellation.token()),
        Err(DocumentUpdateError::Rejected {
            live_version: None,
            last_returned_frame_version: None,
            reason: DocumentUpdateRejection::NoLiveDocument,
        })
    ));
    assert!(matches!(
        engine.rerender_live(version, &cancellation.token()),
        Err(DocumentUpdateError::Rejected {
            live_version: None,
            last_returned_frame_version: None,
            reason: DocumentUpdateRejection::NoLiveDocument,
        })
    ));

    engine.shutdown().expect("engine must shut down cleanly");
}

#[test]
fn successful_batch_returns_exact_frame_and_dense_created_map() {
    let mut engine = StaticPageEngine::new(config()).expect("Linux EGL pbuffer must initialize");
    let initial = load_page(&mut engine);
    let initial_version = initial.evidence.document_version;
    let element = CreatedNodeToken::from_index(0);
    let text = CreatedNodeToken::from_index(1);
    let panel = panel(&engine);

    let update = engine
        .apply_and_render(
            color_update(initial_version, panel),
            &CancellationSource::new().token(),
        )
        .expect("a bounded exact-version batch must fully rerender");

    assert_eq!(initial.frame.pixel(0, 0), Some(INITIAL_PANEL));
    assert_eq!(update.frame.pixel(0, 0), Some(UPDATED_PANEL));
    assert_ne!(update.frame.pixels(), initial.frame.pixels());
    assert_eq!(update.previous_live_version, initial_version);
    assert_eq!(update.previous_last_returned_frame_version, initial_version);
    assert_eq!(
        update.evidence.document_version.revision(),
        initial_version.revision() + 1
    );
    assert_eq!(
        update.evidence.document_version,
        update.frame.document_version()
    );
    assert_eq!(update.frame.epoch(), initial.frame.epoch() + 1);
    assert_eq!(update.created_nodes().len(), 2);
    assert_ne!(update.created_node(element), update.created_node(text));
    assert_eq!(
        update.created_node(element).unwrap().document_id(),
        initial_version.document_id()
    );
    assert_eq!(
        live_versions(&engine),
        (
            update.evidence.document_version,
            update.evidence.document_version
        )
    );

    engine.shutdown().expect("engine must shut down cleanly");
}

#[test]
fn stale_mutation_is_atomic_and_semantic_no_op_advances_once() {
    let mut engine = StaticPageEngine::new(config()).expect("Linux EGL pbuffer must initialize");
    let initial = load_page(&mut engine);
    let initial_version = initial.evidence.document_version;
    let panel = panel(&engine);
    let update = engine
        .apply_and_render(
            no_op_batch(initial_version, panel),
            &CancellationSource::new().token(),
        )
        .expect("a semantic no-op still commits one exact revision");
    let current = update.evidence.document_version;

    assert_eq!(current.revision(), initial_version.revision() + 1);
    assert_eq!(update.frame.epoch(), initial.frame.epoch() + 1);
    assert_eq!(update.frame.pixels(), initial.frame.pixels());
    let rejected = engine
        .apply_and_render(
            no_op_batch(initial_version, panel),
            &CancellationSource::new().token(),
        )
        .unwrap_err();
    assert!(matches!(
        rejected,
        DocumentUpdateError::Rejected {
            live_version: Some(live),
            last_returned_frame_version: Some(last_returned),
            reason: DocumentUpdateRejection::Mutation(
                ScriptMutationError::VersionMismatch { expected, actual }
            ),
        } if live == current
            && last_returned == current
            && expected == initial_version
            && actual == current
    ));
    assert_eq!(live_versions(&engine), (current, current));

    engine.shutdown().expect("engine must shut down cleanly");
}

#[test]
fn later_command_failure_discards_the_private_working_copy() {
    let mut engine = StaticPageEngine::new(config()).expect("Linux EGL pbuffer must initialize");
    let initial = load_page(&mut engine);
    let version = initial.evidence.document_version;
    let panel = panel(&engine);
    let rejected = engine
        .apply_and_render(
            ScriptMutationBatch::new(
                version,
                vec![
                    ScriptMutationCommand::SetHtmlAttribute {
                        element: panel.into(),
                        local_name: "id".into(),
                        value: "temporary".into(),
                    },
                    ScriptMutationCommand::AppendChild {
                        parent: panel.into(),
                        child: panel.into(),
                    },
                ],
            ),
            &CancellationSource::new().token(),
        )
        .unwrap_err();

    assert!(matches!(
        rejected,
        DocumentUpdateError::Rejected {
            live_version: Some(live),
            last_returned_frame_version: Some(last_returned),
            reason: DocumentUpdateRejection::Mutation(ScriptMutationError::Command {
                command_index: 1,
                error: DomError::Cycle,
            }),
        } if live == version && last_returned == version
    ));
    let live = engine.live_document().unwrap();
    assert_eq!(live.element_by_id("panel").unwrap(), Some(panel));
    assert_eq!(live.element_by_id("temporary").unwrap(), None);
    assert_eq!(live_versions(&engine), (version, version));

    engine.shutdown().expect("engine must shut down cleanly");
}

#[test]
fn exact_rerender_keeps_revision_and_stale_rerender_is_rejected() {
    let mut engine = StaticPageEngine::new(config()).expect("Linux EGL pbuffer must initialize");
    let initial = load_page(&mut engine);
    let initial_version = initial.evidence.document_version;
    let rerendered = engine
        .rerender_live(initial_version, &CancellationSource::new().token())
        .expect("the exact live revision must rerender");

    assert_eq!(
        rerendered.previous_last_returned_frame_version,
        initial_version
    );
    assert_eq!(rerendered.evidence.document_version, initial_version);
    assert_eq!(rerendered.frame.document_version(), initial_version);
    assert_eq!(rerendered.frame.epoch(), initial.frame.epoch() + 1);
    assert_eq!(live_versions(&engine), (initial_version, initial_version));

    let panel = panel(&engine);
    let updated = engine
        .apply_and_render(
            no_op_batch(initial_version, panel),
            &CancellationSource::new().token(),
        )
        .unwrap();
    let current = updated.evidence.document_version;
    let stale = engine
        .rerender_live(initial_version, &CancellationSource::new().token())
        .unwrap_err();
    assert!(matches!(
        stale,
        DocumentUpdateError::Rejected {
            live_version: Some(live),
            last_returned_frame_version: Some(last_returned),
            reason: DocumentUpdateRejection::LiveVersionMismatch { expected, actual },
        } if live == current
            && last_returned == current
            && expected == initial_version
            && actual == current
    ));
    let exact = engine
        .rerender_live(current, &CancellationSource::new().token())
        .expect("the unchanged current revision must rerender");
    assert_eq!(exact.evidence.document_version, current);
    assert_eq!(exact.frame.epoch(), updated.frame.epoch() + 1);
    assert_eq!(live_versions(&engine), (current, current));

    engine.shutdown().expect("engine must shut down cleanly");
}

#[test]
fn committed_style_failure_advances_live_only_and_repair_targets_it() {
    let mut limited = config();
    limited.style.max_inline_style_bytes = 32;
    let mut engine = StaticPageEngine::new(limited).expect("Linux EGL pbuffer must initialize");
    let initial = load_page(&mut engine);
    let initial_version = initial.evidence.document_version;
    let initial_pixels = initial.frame.pixels().to_vec();
    let panel = panel(&engine);
    let failure = engine
        .apply_and_render(
            color_update(initial_version, panel),
            &CancellationSource::new().token(),
        )
        .unwrap_err();
    let created_element = failure
        .created_node(CreatedNodeToken::from_index(0))
        .expect("a committed error must retain its dense token map");

    let DocumentUpdateError::Committed {
        previous_live_version,
        last_returned_frame_version,
        commit,
        source,
    } = failure
    else {
        panic!("the bounded style rejection must follow the DOM commit");
    };
    assert!(matches!(
        *source,
        PipelineError::Style(
            wild_buzzard_stylo_adapter::StyleAdapterError::InlineStyleByteLimitExceeded {
                limit: 32
            }
        )
    ));
    let live_version = commit.version();
    assert_eq!(previous_live_version, initial_version);
    assert_eq!(last_returned_frame_version, initial_version);
    assert_eq!(live_version.revision(), initial_version.revision() + 1);
    assert_eq!(commit.created_nodes().len(), 2);
    assert_eq!(live_versions(&engine), (live_version, initial_version));
    assert!(engine.renderer_is_usable());

    let repaired = engine
        .apply_and_render(
            ScriptMutationBatch::new(
                live_version,
                vec![
                    ScriptMutationCommand::RemoveChild {
                        parent: panel.into(),
                        child: created_element.into(),
                    },
                    ScriptMutationCommand::RemoveHtmlAttribute {
                        element: panel.into(),
                        local_name: "style".into(),
                    },
                ],
            ),
            &CancellationSource::new().token(),
        )
        .expect("repair must target the advanced live revision");
    assert_eq!(repaired.previous_live_version, live_version);
    assert_eq!(
        repaired.previous_last_returned_frame_version,
        initial_version
    );
    assert_eq!(repaired.frame.epoch(), initial.frame.epoch() + 1);
    assert_eq!(repaired.frame.pixels(), initial_pixels);
    assert_eq!(
        live_versions(&engine),
        (
            repaired.evidence.document_version,
            repaired.evidence.document_version
        )
    );

    engine.shutdown().expect("engine must shut down cleanly");
}

#[test]
fn pre_send_text_limit_failure_consumes_epoch_and_usable_repair_skips_it() {
    let mut limited = config();
    limited.headless = limited.headless.with_max_pending_text_runs(1);
    let mut engine = StaticPageEngine::new(limited).expect("Linux EGL pbuffer must initialize");
    let initial = load_page(&mut engine);
    let initial_version = initial.evidence.document_version;
    let panel = panel(&engine);
    let first = CreatedNodeToken::from_index(0);
    let second = CreatedNodeToken::from_index(1);
    let failure = engine
        .apply_and_render(
            ScriptMutationBatch::new(
                initial_version,
                vec![
                    ScriptMutationCommand::CreateText {
                        token: first,
                        data: " one".into(),
                    },
                    ScriptMutationCommand::AppendChild {
                        parent: panel.into(),
                        child: first.into(),
                    },
                    ScriptMutationCommand::CreateText {
                        token: second,
                        data: " two".into(),
                    },
                    ScriptMutationCommand::AppendChild {
                        parent: panel.into(),
                        child: second.into(),
                    },
                ],
            ),
            &CancellationSource::new().token(),
        )
        .unwrap_err();

    let DocumentUpdateError::Committed {
        last_returned_frame_version,
        commit,
        source,
        ..
    } = failure
    else {
        panic!("the headless limit must follow the DOM commit");
    };
    assert!(matches!(
        *source,
        PipelineError::Headless(HeadlessError::ResourceLimitExceeded {
            resource: ResourceKind::PendingTextRuns,
            observed,
            limit: 1,
        }) if observed > 1
    ));
    let live_version = commit.version();
    assert_eq!(last_returned_frame_version, initial_version);
    assert_eq!(live_versions(&engine), (live_version, initial_version));
    assert!(engine.renderer_is_usable());

    let repaired = engine
        .apply_and_render(
            ScriptMutationBatch::new(
                live_version,
                vec![
                    ScriptMutationCommand::RemoveChild {
                        parent: panel.into(),
                        child: commit.created_nodes()[0].into(),
                    },
                    ScriptMutationCommand::RemoveChild {
                        parent: panel.into(),
                        child: commit.created_nodes()[1].into(),
                    },
                ],
            ),
            &CancellationSource::new().token(),
        )
        .expect("a pre-send resource failure must leave the renderer repairable");
    assert_eq!(repaired.frame.epoch(), initial.frame.epoch() + 2);
    assert_eq!(
        live_versions(&engine),
        (
            repaired.evidence.document_version,
            repaired.evidence.document_version
        )
    );

    engine.shutdown().expect("engine must shut down cleanly");
}

#[test]
fn failed_replacement_load_retains_live_state_and_exact_rerender_recovers_frame() {
    let mut engine = StaticPageEngine::new(config()).expect("Linux EGL pbuffer must initialize");
    let initial = load_page(&mut engine);
    let version = initial.evidence.document_version;
    let panel = panel(&engine);

    assert!(matches!(
        request_page(&mut engine, "404 Not Found", "missing"),
        Err(PipelineError::HttpStatus(404))
    ));
    assert!(engine.renderer_is_usable());
    assert_eq!(live_versions(&engine), (version, version));
    assert_eq!(
        engine
            .live_document()
            .unwrap()
            .element_by_id("panel")
            .unwrap(),
        Some(panel)
    );

    let rerendered = engine
        .rerender_live(version, &CancellationSource::new().token())
        .expect("the retained exact live revision must rerender");
    assert_eq!(rerendered.previous_last_returned_frame_version, version);
    assert_eq!(rerendered.evidence.document_version, version);
    assert_eq!(rerendered.frame.epoch(), initial.frame.epoch() + 1);
    assert_eq!(live_versions(&engine), (version, version));

    engine.shutdown().expect("engine must shut down cleanly");
}
