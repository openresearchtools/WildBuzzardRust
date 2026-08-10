use std::rc::Rc;
#[cfg(feature = "alloc_error")]
use std::time::Duration;

#[cfg(feature = "alloc_error")]
use brimstone_core::runtime::{BrowserHostTask, ResourceLimitKind};
use brimstone_core::{
    common::options::OptionsBuilder,
    runtime::{
        BrowserHostCommitOutcome, BrowserHostError, BrowserHostPhaseOutcome, ClassicScriptLimits,
        ClassicScriptOutcome, ClassicScriptRequest, ContextBuilder, MicrotaskCheckpointOutcome,
        OwnedContext, ScriptInterruptHandle, ScriptValueSummary,
    },
};
use wild_buzzard_dom::bindings::{
    CreatedNodeToken, ScriptMutationBatch, ScriptMutationCommand, ScriptMutationLimits,
};
use wild_buzzard_dom::{Document, NodeId, NodeKind};
use wild_buzzard_dom_script_bridge::ScriptDocument;

fn initial_document() -> (ScriptDocument, NodeId) {
    let mut document = Document::new();
    let html = document.create_html_element("html").unwrap();
    let body = document.create_html_element("body").unwrap();
    document
        .append_child(document.document_node(), html)
        .unwrap();
    document.append_child(html, body).unwrap();
    (ScriptDocument::new(document), body)
}

fn context() -> OwnedContext {
    let options = OptionsBuilder::new().serialized_heap(None).build().unwrap();
    ContextBuilder::new()
        .set_options(Rc::new(options))
        .build()
        .unwrap()
}

#[cfg(feature = "alloc_error")]
fn fixed_heap_context(bytes: usize) -> OwnedContext {
    let options = OptionsBuilder::new()
        .serialized_heap(None)
        .min_heap_size(bytes)
        .max_heap_size(bytes)
        .build()
        .unwrap();
    ContextBuilder::new()
        .set_options(Rc::new(options))
        .build()
        .unwrap()
}

fn request(source: &str) -> ClassicScriptRequest<'_> {
    ClassicScriptRequest::new(source, "https://example.test/dom-task.js")
        .with_base_url("https://example.test/")
}

fn section_and_text(document: &ScriptDocument) -> (String, String) {
    let snapshot = document.snapshot().unwrap();
    let section = snapshot
        .nodes_in_document_order()
        .iter()
        .find(|node| {
            matches!(
                &node.kind,
                NodeKind::Element(element) if element.name.local_name == "section"
            )
        })
        .expect("script-created section is connected");
    let attribute = match &section.kind {
        NodeKind::Element(element) => element
            .html_attribute("data-phase")
            .expect("data-phase is present")
            .to_owned(),
        _ => unreachable!(),
    };
    let text = section
        .children
        .iter()
        .find_map(|child| match &snapshot.node(*child)?.kind {
            NodeKind::Text(data) => Some(data.clone()),
            _ => None,
        })
        .expect("script-created text is connected");
    (attribute, text)
}

#[test]
fn real_dom_prefix_is_visible_before_error_report_then_checkpoint_mutates_same_roots() {
    let (document, body) = initial_document();
    let before = document.current_version().unwrap();
    let mut task = document.begin_task(ScriptMutationLimits::DEFAULT).unwrap();
    let mut cx = context();

    cx.with_browser_script_realm(|realm| {
        let script = realm.execute_classic_with_host(
            &mut task,
            request(&format!(
                "const dom = __wildBuzzardDom;\n\
                 globalThis.bodyRoot = dom.lookup({});\n\
                 globalThis.sectionRoot = dom.createElement('section');\n\
                 globalThis.textRoot = dom.createText('before-checkpoint');\n\
                 dom.setAttribute(sectionRoot, 'DATA-PHASE', 'script');\n\
                 dom.append(sectionRoot, textRoot);\n\
                 dom.append(bodyRoot, sectionRoot);\n\
                 Promise.resolve().then(() => {{\n\
                   dom.setAttribute(sectionRoot, 'data-phase', 'microtask');\n\
                   dom.setText(textRoot, 'after-checkpoint');\n\
                 }});\n\
                 throw 23;",
                body.slot()
            )),
            ClassicScriptLimits::default(),
            &ScriptInterruptHandle::new(),
        );
        assert_eq!(
            script.script.outcome,
            ClassicScriptOutcome::Thrown(ScriptValueSummary::Number(23.0))
        );
        let BrowserHostPhaseOutcome::Completed(BrowserHostCommitOutcome::Committed(commit)) =
            script.host
        else {
            panic!("script phase did not publish DOM work: {:?}", script.host);
        };
        assert_eq!(commit.before().document_id(), before.document_id().get());
        assert_eq!(commit.before().revision(), before.revision());
        assert_eq!(commit.commands(), 5);
        assert_eq!(commit.created_nodes(), 2);
        assert!(script.script.report.pending_jobs_at_exit() >= 1);
        assert_eq!(script.script.report.jit_native_entries, 0);

        // This is the point where the embedding reports the primary script error. Successful DOM
        // calls are already visible, while promise work has not run.
        assert_eq!(
            section_and_text(&document),
            ("script".into(), "before-checkpoint".into())
        );

        let checkpoint = realm.perform_microtask_checkpoint_with_host(
            &mut task,
            ClassicScriptLimits::default(),
            &ScriptInterruptHandle::new(),
        );
        assert_eq!(
            checkpoint.checkpoint.outcome,
            MicrotaskCheckpointOutcome::Complete
        );
        let BrowserHostPhaseOutcome::Completed(BrowserHostCommitOutcome::Committed(commit)) =
            checkpoint.host
        else {
            panic!(
                "checkpoint phase did not publish DOM work: {:?}",
                checkpoint.host
            );
        };
        assert_eq!(commit.commands(), 2);
        assert_eq!(commit.created_nodes(), 0);
        assert_eq!(checkpoint.checkpoint.report.pending_jobs_at_exit(), 0);
        assert_eq!(checkpoint.checkpoint.report.jit_native_entries, 0);
        assert_eq!(
            section_and_text(&document),
            ("microtask".into(), "after-checkpoint".into())
        );
    });

    assert!(task.rooted_node_count() >= 3);
    assert_eq!(task.expected_version(), document.current_version().unwrap());
}

#[test]
fn invalid_later_dom_call_throws_without_rolling_back_successful_prefix() {
    let (document, body) = initial_document();
    let mut task = document.begin_task(ScriptMutationLimits::DEFAULT).unwrap();
    let mut cx = context();

    cx.with_browser_script_realm(|realm| {
        let script = realm.execute_classic_with_host(
            &mut task,
            request(&format!(
                "const dom = __wildBuzzardDom;\n\
                 const body = dom.lookup({});\n\
                 const child = dom.createElement('section');\n\
                 dom.append(body, child);\n\
                 dom.append(child, body);",
                body.slot()
            )),
            ClassicScriptLimits::default(),
            &ScriptInterruptHandle::new(),
        );
        assert!(matches!(
            script.script.outcome,
            ClassicScriptOutcome::Thrown(_)
        ));
        assert!(matches!(
            script.host,
            BrowserHostPhaseOutcome::Completed(BrowserHostCommitOutcome::Committed(_))
        ));
    });

    let snapshot = document.snapshot().unwrap();
    assert!(snapshot.nodes_in_document_order().iter().any(|node| {
        matches!(
            &node.kind,
            NodeKind::Element(element) if element.name.local_name == "section"
        )
    }));
}

#[test]
fn token_from_another_browser_task_fails_closed_without_dom_mutation() {
    let (document, body) = initial_document();
    let mut first_task = document.begin_task(ScriptMutationLimits::DEFAULT).unwrap();
    let mut cx = context();

    cx.with_browser_script_realm(|realm| {
        let first = realm.execute_classic_with_host(
            &mut first_task,
            request(&format!(
                "globalThis.staleBodyRoot = __wildBuzzardDom.lookup({});",
                body.slot()
            )),
            ClassicScriptLimits::default(),
            &ScriptInterruptHandle::new(),
        );
        assert!(matches!(
            first.script.outcome,
            ClassicScriptOutcome::Success(_)
        ));

        let version = document.current_version().unwrap();
        let mut second_task = document.begin_task(ScriptMutationLimits::DEFAULT).unwrap();
        let stale = realm.execute_classic_with_host(
            &mut second_task,
            request("__wildBuzzardDom.setAttribute(staleBodyRoot, 'data-stale', 'bad');"),
            ClassicScriptLimits::default(),
            &ScriptInterruptHandle::new(),
        );
        assert_eq!(
            stale.script.outcome,
            ClassicScriptOutcome::HostFailure(BrowserHostError::StaleTask)
        );
        assert_eq!(stale.host, BrowserHostPhaseOutcome::Discarded);
        assert_eq!(document.current_version().unwrap(), version);
    });
}

#[test]
fn exact_external_version_drift_and_retired_document_cancel_stale_tasks() {
    let (document, _body) = initial_document();
    let mut stale_version_task = document.begin_task(ScriptMutationLimits::DEFAULT).unwrap();
    let version = document.current_version().unwrap();
    document
        .apply_external_mutations(
            ScriptMutationBatch::new(
                version,
                vec![ScriptMutationCommand::CreateHtmlElement {
                    token: CreatedNodeToken::from_index(0),
                    local_name: "aside".into(),
                }],
            ),
            ScriptMutationLimits::DEFAULT,
        )
        .unwrap();

    let mut cx = context();
    cx.with_browser_script_realm(|realm| {
        let stale = realm.execute_classic_with_host(
            &mut stale_version_task,
            request("globalThis.mustNotRunAfterVersionDrift = true;"),
            ClassicScriptLimits::default(),
            &ScriptInterruptHandle::new(),
        );
        assert_eq!(
            stale.script.outcome,
            ClassicScriptOutcome::HostFailure(BrowserHostError::VersionMismatch)
        );
        assert_eq!(
            stale.host,
            BrowserHostPhaseOutcome::Failed(BrowserHostError::VersionMismatch)
        );
        let check = realm.execute_classic(
            request("if ('mustNotRunAfterVersionDrift' in globalThis) throw 'stale script ran';"),
            ClassicScriptLimits::default(),
            &ScriptInterruptHandle::new(),
        );
        assert!(matches!(check.outcome, ClassicScriptOutcome::Success(_)));
    });

    let (retired_document, _) = initial_document();
    let mut retired_task = retired_document
        .begin_task(ScriptMutationLimits::DEFAULT)
        .unwrap();
    retired_document.retire().unwrap();
    let mut retired_cx = context();
    retired_cx.with_browser_script_realm(|realm| {
        let stale = realm.execute_classic_with_host(
            &mut retired_task,
            request("globalThis.mustNotRunAfterRetire = true;"),
            ClassicScriptLimits::default(),
            &ScriptInterruptHandle::new(),
        );
        assert_eq!(
            stale.script.outcome,
            ClassicScriptOutcome::HostFailure(BrowserHostError::StaleDocument)
        );
        assert_eq!(
            stale.host,
            BrowserHostPhaseOutcome::Failed(BrowserHostError::StaleDocument)
        );
        let check = realm.execute_classic(
            request("if ('mustNotRunAfterRetire' in globalThis) throw 'retired script ran';"),
            ClassicScriptLimits::default(),
            &ScriptInterruptHandle::new(),
        );
        assert!(matches!(check.outcome, ClassicScriptOutcome::Success(_)));
    });
}

#[test]
fn retired_document_is_validated_before_checkpoint_jobs_run() {
    let (document, _) = initial_document();
    let mut task = document.begin_task(ScriptMutationLimits::DEFAULT).unwrap();
    let mut cx = context();

    cx.with_browser_script_realm(|realm| {
        let setup = realm.execute_classic(
            request("Promise.resolve().then(() => { globalThis.mustNotRunAfterRetire = true; });"),
            ClassicScriptLimits::default(),
            &ScriptInterruptHandle::new(),
        );
        assert!(setup.report.pending_jobs_at_exit() >= 1);
        document.retire().unwrap();

        let checkpoint = realm.perform_microtask_checkpoint_with_host(
            &mut task,
            ClassicScriptLimits::default(),
            &ScriptInterruptHandle::new(),
        );
        assert_eq!(
            checkpoint.checkpoint.outcome,
            MicrotaskCheckpointOutcome::HostFailure(BrowserHostError::StaleDocument)
        );
        assert_eq!(
            checkpoint.host,
            BrowserHostPhaseOutcome::Failed(BrowserHostError::StaleDocument)
        );
        assert_eq!(checkpoint.checkpoint.report.pending_jobs_at_exit(), 0);

        let check = realm.execute_classic(
            request("if ('mustNotRunAfterRetire' in globalThis) throw 'retired job ran';"),
            ClassicScriptLimits::default(),
            &ScriptInterruptHandle::new(),
        );
        assert!(matches!(check.outcome, ClassicScriptOutcome::Success(_)));
    });
}

#[cfg(feature = "gc_stress_test")]
#[test]
fn rooted_tokens_and_dom_results_survive_forced_moving_gc() {
    let (document, body) = initial_document();
    let mut task = document.begin_task(ScriptMutationLimits::DEFAULT).unwrap();
    let mut cx = context();
    cx.enable_gc_stress_test();

    cx.with_browser_script_realm(|realm| {
        let script = realm.execute_classic_with_host(
            &mut task,
            request(&format!(
                "const dom = __wildBuzzardDom;\n\
                 const body = dom.lookup({});\n\
                 const section = dom.createElement('section');\n\
                 const text = dom.createText('moving');\n\
                 for (let i = 0; i < 40; i++) {{\n\
                   const garbage = {{ i, value: 'moving-' + i, nested: {{ i }} }};\n\
                   if (garbage.nested.i !== i) throw 'GC corruption';\n\
                 }}\n\
                 dom.setAttribute(section, 'data-phase', 'gc');\n\
                 dom.append(section, text);\n\
                 dom.append(body, section);\n\
                 Promise.resolve().then(() => dom.setText(text, 'moved'));",
                body.slot()
            )),
            ClassicScriptLimits::default(),
            &ScriptInterruptHandle::new(),
        );
        assert!(matches!(
            script.script.outcome,
            ClassicScriptOutcome::Success(_)
        ));
        let checkpoint = realm.perform_microtask_checkpoint_with_host(
            &mut task,
            ClassicScriptLimits::default(),
            &ScriptInterruptHandle::new(),
        );
        assert_eq!(
            checkpoint.checkpoint.outcome,
            MicrotaskCheckpointOutcome::Complete
        );
    });
    assert_eq!(section_and_text(&document).1, "moved");
}

#[cfg(feature = "alloc_error")]
#[test]
fn engine_oom_keeps_synchronous_dom_prefix_but_discards_jobs_and_retires_task() {
    const HEAP_BYTES: usize = 64 * 1024 * 1024;
    let (document, body) = initial_document();
    let mut task = document.begin_task(ScriptMutationLimits::DEFAULT).unwrap();
    let mut cx = fixed_heap_context(HEAP_BYTES);

    cx.with_browser_script_realm(|realm| {
        let failed = realm.execute_classic_with_host(
            &mut task,
            request(&format!(
                "const dom = __wildBuzzardDom;\n\
                 const body = dom.lookup({});\n\
                 const section = dom.createElement('section');\n\
                 dom.setAttribute(section, 'data-phase', 'before-oom');\n\
                 dom.append(body, section);\n\
                 Promise.resolve().then(() => dom.setAttribute(section, 'data-phase', 'bad'));\n\
                 globalThis.a = new Array(2000000).fill(1);\n\
                 globalThis.b = new Array(2000000).fill(2);\n\
                 globalThis.c = new Array(2000000).fill(3);",
                body.slot()
            )),
            ClassicScriptLimits::new(
                1_000_000,
                256 * 1024 * 1024,
                64,
                64,
                Duration::from_secs(30),
            )
            .unwrap(),
            &ScriptInterruptHandle::new(),
        );
        assert_eq!(
            failed.script.outcome,
            ClassicScriptOutcome::ResourceLimit(ResourceLimitKind::EngineAllocation)
        );
        assert_eq!(failed.host, BrowserHostPhaseOutcome::Discarded);
        assert_eq!(failed.script.report.pending_jobs_at_exit(), 0);
    });

    assert_eq!(section_and_text_without_text(&document), "before-oom");
    assert_eq!(
        task.finish_phase(),
        Err(BrowserHostError::StaleTask),
        "resource failure permanently retires the browser task"
    );
}

#[cfg(feature = "alloc_error")]
fn section_and_text_without_text(document: &ScriptDocument) -> String {
    let snapshot = document.snapshot().unwrap();
    snapshot
        .nodes_in_document_order()
        .iter()
        .find_map(|node| match &node.kind {
            NodeKind::Element(element) if element.name.local_name == "section" => {
                element.html_attribute("data-phase").map(str::to_owned)
            }
            _ => None,
        })
        .expect("synchronous section prefix remains connected")
}
