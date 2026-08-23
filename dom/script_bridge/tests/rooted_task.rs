use std::{rc::Rc, time::Duration};

#[cfg(feature = "alloc_error")]
use brimstone_core::runtime::ResourceLimitKind;
use brimstone_core::{
    common::options::OptionsBuilder,
    runtime::{
        BrowserHostCommitOutcome, BrowserHostError, BrowserHostPhaseOutcome, BrowserHostTask,
        ClassicScriptLimits, ClassicScriptOutcome, ClassicScriptRequest, ContextBuilder,
        MicrotaskCheckpointOutcome, OwnedContext, ScriptInterruptHandle, ScriptValueSummary,
    },
};
use wild_buzzard_dom::bindings::{
    CreatedNodeToken, ScriptMutationBatch, ScriptMutationCommand, ScriptMutationLimits,
};
use wild_buzzard_dom::{Document, DocumentSnapshot, NodeId, NodeKind};
use wild_buzzard_dom_script_bridge::{ParserPhaseError, ScriptDocument};
use wild_buzzard_html::{HtmlParser, ParserInsertedScript, TokenizerLimits};

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

fn snapshot_text(snapshot: &DocumentSnapshot, node: NodeId) -> String {
    snapshot
        .node(node)
        .unwrap()
        .children
        .iter()
        .filter_map(|child| match &snapshot.node(*child)?.kind {
            NodeKind::Text(data) => Some(data.as_str()),
            _ => None,
        })
        .collect()
}

#[test]
fn one_hosted_realm_survives_parser_leases_and_publishes_the_final_document_identity() {
    let document = ScriptDocument::new(Document::new());
    let initial_version = document.current_version().unwrap();
    let mut task = document.begin_task(ScriptMutationLimits::DEFAULT).unwrap();
    let (parser_document, initial_lease) = task.lend_document_to_parser().unwrap().into_parts();
    assert_eq!(parser_document.version(), initial_version);
    assert_eq!(document.current_version(), Err(BrowserHostError::StaleTask));

    let mut parser =
        HtmlParser::from_pristine_document(TokenizerLimits::default(), parser_document).unwrap();
    let mut cx = context();
    let interrupt = ScriptInterruptHandle::new();
    let document_limits =
        ClassicScriptLimits::parser_blocking_document(Duration::from_secs(30)).unwrap();
    let mut lease = Some(initial_lease);
    let mut boundaries = 0usize;

    cx.with_browser_script_realm(|realm| {
        realm
            .with_hosted_document_script_budget(
                &mut task,
                document_limits,
                &interrupt,
                |document_session| {
                    let mut handler = |parser_document: &mut Document,
                                       script: ParserInsertedScript|
                     -> Result<(), ParserPhaseError> {
                        let script_node = script.node();
                        let current_lease = lease
                            .take()
                            .ok_or(ParserPhaseError::Host(BrowserHostError::StaleTask))?;
                        let restored = document
                            .restore_parser_boundary(parser_document, current_lease, script)
                            .map_err(ParserPhaseError::Host)?;
                        assert!(matches!(
                            document.snapshot(),
                            Err(BrowserHostError::StaleTask)
                        ));

                        let prepared = restored.perform_pre_checkpoint(document_session)?;
                        let pre = prepared.pre_checkpoint();
                        assert_eq!(pre.checkpoint.outcome, MicrotaskCheckpointOutcome::Complete);
                        assert!(matches!(pre.host, BrowserHostPhaseOutcome::Completed(_)));

                        assert!(matches!(
                            document.snapshot(),
                            Err(BrowserHostError::StaleTask)
                        ));
                        let snapshot = prepared.snapshot().map_err(ParserPhaseError::Host)?;
                        let source = snapshot_text(&snapshot, script_node);
                        if boundaries == 0 {
                            assert!(snapshot.nodes_in_document_order().iter().all(|node| {
                                !matches!(
                                    &node.kind,
                                    NodeKind::Element(element)
                                        if element.name.local_name == "p"
                                )
                            }));
                        } else {
                            assert!(snapshot.nodes_in_document_order().iter().any(|node| {
                                matches!(
                                    &node.kind,
                                    NodeKind::Element(element)
                                        if element.name.local_name == "p"
                                )
                            }));
                        }

                        let executed =
                            prepared.execute_classic(document_session, request(&source))?;
                        let execution = executed.execution();
                        assert!(
                            matches!(execution.script.outcome, ClassicScriptOutcome::Success(_)),
                            "script outcome: {:?}; host outcome: {:?}",
                            execution.script.outcome,
                            execution.host
                        );
                        assert!(matches!(
                            execution.host,
                            BrowserHostPhaseOutcome::Completed(_)
                        ));
                        let completed = executed.perform_post_checkpoint(document_session)?;
                        let post = completed
                            .post_checkpoint()
                            .expect("admitted classic scripts require a post checkpoint");
                        assert_eq!(
                            post.checkpoint.outcome,
                            MicrotaskCheckpointOutcome::Complete
                        );
                        assert!(matches!(post.host, BrowserHostPhaseOutcome::Completed(_)));

                        lease = Some(
                            completed
                                .lend_back_to_parser()
                                .map_err(ParserPhaseError::Host)?,
                        );
                        boundaries += 1;
                        Ok(())
                    };

                    parser
                        .feed_with_script_handler(
                            "<body><script>\
                             globalThis.dom = __wildBuzzardDom;\
                             globalThis.bodyRoot = dom.lookup(3);\
                             globalThis.sectionRoot = dom.createElement('section');\
                             globalThis.textRoot = dom.createText('first');\
                             dom.setAttribute(sectionRoot, 'data-phase', 'first');\
                             dom.append(sectionRoot, textRoot);\
                             dom.append(bodyRoot, sectionRoot);\
                             </script><p>between</p><script>\
                             dom.setAttribute(sectionRoot, 'data-phase', 'second');\
                             dom.setText(textRoot, 'second');\
                             globalThis.paragraphRoot = dom.lookup(8);\
                             dom.setAttribute(paragraphRoot, 'data-seen', 'second');\
                             </script>",
                            &mut handler,
                        )
                        .unwrap();
                    let parsed = parser.finish_with_script_handler(&mut handler).unwrap();
                    let final_lease = lease
                        .take()
                        .ok_or(ParserPhaseError::Host(BrowserHostError::StaleTask))?;
                    let completion = document
                        .restore_parser_completion(final_lease, parsed)
                        .map_err(ParserPhaseError::Host)?;
                    assert!(matches!(
                        document.snapshot(),
                        Err(BrowserHostError::StaleTask)
                    ));
                    let published = completion.perform_final_checkpoint(document_session)?;
                    let final_checkpoint = published.final_checkpoint();
                    assert_eq!(
                        final_checkpoint.checkpoint.outcome,
                        MicrotaskCheckpointOutcome::Complete
                    );
                    assert!(matches!(
                        final_checkpoint.host,
                        BrowserHostPhaseOutcome::Completed(_)
                    ));
                    assert_eq!(
                        published
                            .snapshot()
                            .map_err(ParserPhaseError::Host)?
                            .version(),
                        published.published_version()
                    );
                    Ok::<(), ParserPhaseError>(())
                },
            )
            .unwrap()
            .unwrap();
    });

    assert_eq!(boundaries, 2);
    assert_eq!(task.expected_version(), document.current_version().unwrap());
    assert_eq!(
        task.expected_version().document_id(),
        initial_version.document_id()
    );
    assert_eq!(
        section_and_text(&document),
        ("second".into(), "second".into())
    );
    let snapshot = document.snapshot().unwrap();
    let paragraph = snapshot
        .nodes_in_document_order()
        .iter()
        .find(|node| {
            matches!(
                &node.kind,
                NodeKind::Element(element) if element.name.local_name == "p"
            )
        })
        .unwrap();
    let NodeKind::Element(paragraph) = &paragraph.kind else {
        unreachable!();
    };
    assert_eq!(paragraph.html_attribute("data-seen"), Some("second"));
}

#[test]
fn parser_lease_rejects_cross_document_and_wrong_version_restores_without_swapping() {
    let first = ScriptDocument::new(Document::new());
    let mut first_task = first.begin_task(ScriptMutationLimits::DEFAULT).unwrap();
    let (parser_document, lease) = first_task.lend_document_to_parser().unwrap().into_parts();
    let parser_id = parser_document.id();
    let foreign = ScriptDocument::new(Document::new());
    let mut parser =
        HtmlParser::from_pristine_document(TokenizerLimits::default(), parser_document).unwrap();
    let mut lease = Some(lease);
    let mut foreign_error = None;
    {
        let mut handler = |parser_document: &mut Document, script: ParserInsertedScript| {
            foreign_error = Some(
                match foreign.restore_parser_boundary(
                    parser_document,
                    lease.take().unwrap(),
                    script,
                ) {
                    Ok(_) => {
                        panic!("a foreign document unexpectedly restored the parser boundary")
                    }
                    Err(error) => error,
                },
            );
            assert_eq!(parser_document.id(), parser_id);
            Ok::<(), &'static str>(())
        };
        parser
            .feed_with_script_handler("<script></script>", &mut handler)
            .unwrap();
    }
    assert_eq!(foreign_error, Some(BrowserHostError::StaleDocument));
    assert_eq!(
        first_task.validate_phase(),
        Err(BrowserHostError::StaleTask)
    );

    let second = ScriptDocument::new(Document::new());
    let mut second_task = second.begin_task(ScriptMutationLimits::DEFAULT).unwrap();
    let (parser_document, lease) = second_task.lend_document_to_parser().unwrap().into_parts();
    let mut parser =
        HtmlParser::from_pristine_document(TokenizerLimits::default(), parser_document).unwrap();
    let mut lease = Some(lease);
    let mut drifted_version = None;
    let mut wrong_version_error = None;
    {
        let mut handler = |parser_document: &mut Document, script: ParserInsertedScript| {
            let _ = parser_document.create_html_element("detached").unwrap();
            drifted_version = Some(parser_document.version());
            wrong_version_error = Some(
                match second.restore_parser_boundary(parser_document, lease.take().unwrap(), script)
                {
                    Ok(_) => panic!("a drifted parser version unexpectedly restored"),
                    Err(error) => error,
                },
            );
            Ok::<(), &'static str>(())
        };
        parser
            .feed_with_script_handler("<script></script>", &mut handler)
            .unwrap();
    }
    assert_eq!(wrong_version_error, Some(BrowserHostError::VersionMismatch));
    assert_eq!(parser.document().version(), drifted_version.unwrap());
    assert_eq!(
        second_task.validate_phase(),
        Err(BrowserHostError::StaleTask)
    );
}

#[test]
fn dropping_a_restored_parser_guard_recovers_the_document_after_host_retirement() {
    let document = ScriptDocument::new(Document::new());
    let mut task = document.begin_task(ScriptMutationLimits::DEFAULT).unwrap();
    let (parser_document, lease) = task.lend_document_to_parser().unwrap().into_parts();
    let identity = parser_document.id();
    let mut parser =
        HtmlParser::from_pristine_document(TokenizerLimits::default(), parser_document).unwrap();
    let mut lease = Some(lease);
    let mut recovered_version = None;
    {
        let mut handler = |parser_document: &mut Document, script: ParserInsertedScript| {
            let version = parser_document.version();
            let restored = document
                .restore_parser_boundary(parser_document, lease.take().unwrap(), script)
                .unwrap();
            task.abort_phase();
            drop(restored);
            assert_eq!(parser_document.id(), identity);
            assert_eq!(parser_document.version(), version);
            recovered_version = Some(version);
            Ok::<(), &'static str>(())
        };
        parser
            .feed_with_script_handler("<script></script>", &mut handler)
            .unwrap();
    }

    assert_eq!(parser.document().id(), identity);
    assert_eq!(parser.document().version(), recovered_version.unwrap());
    assert_eq!(
        document.current_version(),
        Err(BrowserHostError::StaleDocument)
    );
    assert_eq!(task.validate_phase(), Err(BrowserHostError::StaleTask));
}

#[test]
fn omitting_the_final_checkpoint_never_publishes_the_parser_document() {
    let document = ScriptDocument::new(Document::new());
    let mut task = document.begin_task(ScriptMutationLimits::DEFAULT).unwrap();
    let (parser_document, lease) = task.lend_document_to_parser().unwrap().into_parts();
    let mut parser =
        HtmlParser::from_pristine_document(TokenizerLimits::default(), parser_document).unwrap();
    parser.feed("<main></main>").unwrap();
    let parsed = parser.finish().unwrap();

    let completion = document.restore_parser_completion(lease, parsed).unwrap();
    assert!(matches!(
        document.snapshot(),
        Err(BrowserHostError::StaleTask)
    ));
    assert!(matches!(
        document.current_version(),
        Err(BrowserHostError::StaleTask)
    ));

    drop(completion);
    assert!(matches!(
        document.snapshot(),
        Err(BrowserHostError::StaleDocument)
    ));
    assert_eq!(task.validate_phase(), Err(BrowserHostError::StaleTask));
}

#[test]
fn ignored_closed_script_boundary_cannot_be_published_at_parser_completion() {
    let document = ScriptDocument::new(Document::new());
    let mut task = document.begin_task(ScriptMutationLimits::DEFAULT).unwrap();
    let (parser_document, lease) = task.lend_document_to_parser().unwrap().into_parts();
    let mut parser =
        HtmlParser::from_pristine_document(TokenizerLimits::default(), parser_document).unwrap();
    let mut ignored = 0_u64;
    let mut handler = |_: &mut Document, script: ParserInsertedScript| {
        ignored += 1;
        assert_eq!(script.ordinal(), ignored);
        Ok::<(), &'static str>(())
    };
    parser
        .feed_with_script_handler(
            "<body><script>globalThis.mustNotRun = true;</script><p>after</p>",
            &mut handler,
        )
        .unwrap();
    let parsed = parser.finish_with_script_handler(&mut handler).unwrap();
    assert_eq!(parsed.completed_script_boundaries(), 1);
    assert!(matches!(
        document.restore_parser_completion(lease, parsed),
        Err(BrowserHostError::VersionMismatch)
    ));
    assert!(matches!(
        document.snapshot(),
        Err(BrowserHostError::StaleTask)
    ));
    assert_eq!(task.validate_phase(), Err(BrowserHostError::StaleTask));
}

#[test]
fn parser_lending_rejects_a_nonpristine_document_without_detaching_it() {
    let mut arena = Document::new();
    let _ = arena.create_html_element("detached").unwrap();
    let version = arena.version();
    let document = ScriptDocument::new(arena);
    let mut task = document.begin_task(ScriptMutationLimits::DEFAULT).unwrap();
    assert!(matches!(
        task.lend_document_to_parser(),
        Err(BrowserHostError::InvalidOperation)
    ));
    assert_eq!(document.current_version().unwrap(), version);
    assert_eq!(task.validate_phase(), Ok(()));
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
