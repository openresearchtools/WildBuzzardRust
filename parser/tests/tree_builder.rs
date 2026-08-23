use wild_buzzard_dom::{Document, NodeId, NodeKind};
use wild_buzzard_html::{
    DocumentMode, HtmlParser, ParseErrorCode, ParserStateError, ScriptHandlerError,
    TokenizerLimits, parse_document,
};

fn element_name(document: &Document, node: NodeId) -> &str {
    let NodeKind::Element(element) = document.node_kind(node).unwrap() else {
        panic!("expected element");
    };
    &element.name.local_name
}

fn element_children(document: &Document, parent: NodeId) -> Vec<NodeId> {
    document
        .children(parent)
        .unwrap()
        .iter()
        .copied()
        .filter(|child| matches!(document.node_kind(*child), Ok(NodeKind::Element(_))))
        .collect()
}

fn body(document: &Document) -> NodeId {
    document.elements_by_tag_name("body").unwrap()[0]
}

fn tree_shape(document: &Document) -> Vec<(u32, Option<u32>, Vec<u32>, NodeKind)> {
    document
        .snapshot()
        .unwrap()
        .nodes_in_document_order()
        .iter()
        .map(|node| {
            (
                node.id.slot(),
                node.parent.map(NodeId::slot),
                node.children.iter().copied().map(NodeId::slot).collect(),
                node.kind.clone(),
            )
        })
        .collect()
}

#[test]
fn builds_doctype_and_implicit_html_head_body() {
    let output =
        parse_document("<!DOCTYPE html><title>T &amp; C</title><main id=x>Hello</main>").unwrap();
    let document = &output.document;
    let root = document.document_element().unwrap();
    assert_eq!(element_name(document, root), "html");
    let root_children = element_children(document, root);
    assert_eq!(
        root_children
            .iter()
            .map(|node| element_name(document, *node))
            .collect::<Vec<_>>(),
        vec!["head", "body"]
    );
    let title = document.elements_by_tag_name("title").unwrap()[0];
    assert_eq!(
        document.text_content(title).unwrap().as_deref(),
        Some("T & C")
    );
    let main = document.elements_by_tag_name("main").unwrap()[0];
    assert_eq!(document.attribute(main, None, "id").unwrap(), Some("x"));
    assert_eq!(
        document.text_content(main).unwrap().as_deref(),
        Some("Hello")
    );
    assert!(document.doctype().is_some());
    assert_eq!(output.document_mode, DocumentMode::NoQuirks);
}

#[test]
fn missing_or_non_html_doctype_enters_quirks_mode() {
    assert_eq!(
        parse_document("<p>x").unwrap().document_mode,
        DocumentMode::Quirks
    );
    assert_eq!(
        parse_document("<!doctype potato><p>x")
            .unwrap()
            .document_mode,
        DocumentMode::Quirks
    );
}

#[test]
fn only_ascii_html_spaces_are_ignored_before_structure() {
    let output = parse_document(" \t\r\n\u{00a0}<p>x</p>").unwrap();
    let body = body(&output.document);
    assert_eq!(
        output.document.text_content(body).unwrap().as_deref(),
        Some("\u{00a0}x")
    );
    assert!(
        output
            .errors
            .iter()
            .any(|error| error.code == ParseErrorCode::UnexpectedCharactersBeforeDocumentElement)
    );
}

#[test]
fn incremental_feeds_produce_the_same_tree_and_errors() {
    let source = "<!doctype html><title>x&amp;y</title><p a=1>one<div>two</div><!--z-->";
    let expected = parse_document(source).unwrap();
    for boundary in 0..=source.len() {
        let mut parser = HtmlParser::default();
        parser.feed(&source[..boundary]).unwrap();
        parser.feed(&source[boundary..]).unwrap();
        let actual = parser.finish().unwrap();
        assert_eq!(
            tree_shape(&actual.document),
            tree_shape(&expected.document),
            "boundary {boundary}"
        );
        assert_eq!(actual.errors, expected.errors, "boundary {boundary}");
    }
}

#[test]
fn caller_owned_pristine_document_keeps_its_identity_and_rejects_prior_mutation() {
    let document = Document::new();
    let document_id = document.id();
    let mut parser = HtmlParser::from_pristine_document(TokenizerLimits::default(), document)
        .expect("a fresh arena is an admissible parser owner");
    parser.feed("<main>owned</main>").unwrap();
    let output = parser.finish().unwrap();
    assert_eq!(output.document.id(), document_id);
    assert_eq!(
        output
            .document
            .text_content(output.document.elements_by_tag_name("main").unwrap()[0])
            .unwrap()
            .as_deref(),
        Some("owned")
    );

    let mut modified = Document::new();
    let _ = modified.create_html_element("detached").unwrap();
    assert!(matches!(
        HtmlParser::from_pristine_document(TokenizerLimits::default(), modified),
        Err(ParserStateError::Dom(_))
    ));
}

#[test]
fn parser_inserted_script_hands_off_the_live_node_before_following_markup() {
    let mut parser = HtmlParser::default();
    let mut observed = Vec::new();
    let mut handler = |document: &mut Document, script: wild_buzzard_html::ParserInsertedScript| {
        assert!(document.elements_by_tag_name("p").unwrap().is_empty());
        assert_eq!(
            document.text_content(script.node()).unwrap().as_deref(),
            Some("globalThis.phase = 'script';")
        );
        assert_eq!(
            document.attribute(script.node(), None, "src").unwrap(),
            Some("original.js")
        );
        assert_eq!(script.document_version(), document.version());
        assert_eq!(script.node().document_id(), document.id());
        assert_eq!(script.ordinal(), 1);
        assert_eq!(script.start_tag().src(), Some("original.js"));

        let text = document.children(script.node()).unwrap()[0];
        document
            .set_character_data(text, "globalThis.phase = 'checkpoint';")
            .unwrap();
        document
            .set_html_attribute(script.node(), "src", "changed.js")
            .unwrap();
        let prepared_source = document.text_content(script.node()).unwrap().unwrap();
        let prepared_src = document
            .attribute(script.node(), None, "src")
            .unwrap()
            .unwrap()
            .to_owned();

        let marker = document.create_html_element("section").unwrap();
        document
            .set_html_attribute(marker, "data-phase", "script")
            .unwrap();
        let body = body(document);
        document.append_child(body, marker).unwrap();
        observed.push((script.node(), marker, prepared_source, prepared_src));
        assert_eq!(script.start_tag().src(), Some("original.js"));
        Ok::<(), &'static str>(())
    };

    parser
        .feed_with_script_handler(
            "<body><script src=original.js>globalThis.phase = 'script';</script><p>after</p>",
            &mut handler,
        )
        .unwrap();
    let output = parser.finish_with_script_handler(&mut handler).unwrap();
    assert_eq!(observed.len(), 1);
    assert_eq!(output.completed_script_boundaries(), 1);
    assert_eq!(
        output.completion_document_version(),
        output.document.version()
    );
    let body = body(&output.document);
    let names = element_children(&output.document, body)
        .into_iter()
        .map(|node| element_name(&output.document, node).to_owned())
        .collect::<Vec<_>>();
    assert_eq!(names, ["script", "section", "p"]);
    assert_eq!(
        output
            .document
            .attribute(observed[0].0, None, "src")
            .unwrap(),
        Some("changed.js")
    );
    assert_eq!(observed[0].2, "globalThis.phase = 'checkpoint';");
    assert_eq!(observed[0].3, "changed.js");
}

#[test]
fn parser_script_execution_attributes_and_base_are_frozen_at_the_start_tag() {
    let mut parser = HtmlParser::default();
    let mut observed = false;
    let mut handler = |document: &mut Document, script: wild_buzzard_html::ParserInsertedScript| {
        let start_tag = script.start_tag();
        assert_eq!(start_tag.base_href(), Some("https://first.invalid/root/"));
        assert_eq!(start_tag.src(), Some("original.js"));
        assert_eq!(start_tag.script_type(), Some("module"));
        assert_eq!(start_tag.language(), Some("javascript"));
        assert_eq!(start_tag.charset(), Some("utf-8"));
        assert_eq!(start_tag.cross_origin(), Some("anonymous"));
        assert_eq!(start_tag.integrity(), Some("sha256-original"));
        assert_eq!(start_tag.nonce(), Some("secret-nonce"));
        assert_eq!(start_tag.referrer_policy(), Some("no-referrer"));
        assert_eq!(start_tag.fetch_priority(), Some("high"));
        assert_eq!(start_tag.blocking(), Some("render"));
        assert!(start_tag.async_present());
        assert!(start_tag.defer_present());
        assert!(start_tag.no_module_present());
        assert!(start_tag.opening_span().start.byte < start_tag.opening_span().end.byte);

        let redacted_debug = format!("{start_tag:?}");
        assert!(!redacted_debug.contains("original.js"));
        assert!(!redacted_debug.contains("secret-nonce"));
        assert!(!redacted_debug.contains("sha256-original"));

        for (name, value) in [
            ("src", "changed.js"),
            ("type", "text/plain"),
            ("language", "not-javascript"),
            ("charset", "changed-charset"),
            ("crossorigin", "use-credentials"),
            ("integrity", "changed-integrity"),
            ("nonce", "changed-nonce"),
            ("referrerpolicy", "origin"),
            ("fetchpriority", "low"),
            ("blocking", "changed-blocking"),
        ] {
            document
                .set_html_attribute(script.node(), name, value)
                .unwrap();
        }
        let first_base = document.elements_by_tag_name("base").unwrap()[0];
        document
            .set_html_attribute(first_base, "href", "https://changed.invalid/")
            .unwrap();

        assert_eq!(start_tag.base_href(), Some("https://first.invalid/root/"));
        assert_eq!(start_tag.src(), Some("original.js"));
        assert_eq!(start_tag.script_type(), Some("module"));
        assert_eq!(start_tag.nonce(), Some("secret-nonce"));
        observed = true;
        Ok::<(), &'static str>(())
    };

    parser
        .feed_with_script_handler(
            "<head><base href='https://first.invalid/root/'>\
             <base href='https://second.invalid/'>\
             <script src=original.js type=module language=javascript charset=utf-8 \
             crossorigin=anonymous integrity=sha256-original nonce=secret-nonce \
             referrerpolicy=no-referrer fetchpriority=high blocking=render async defer \
             nomodule>globalThis.phase = 'script';</script></head>",
            &mut handler,
        )
        .unwrap();
    let output = parser.finish_with_script_handler(&mut handler).unwrap();
    assert!(observed);
    let script = output.document.elements_by_tag_name("script").unwrap()[0];
    assert_eq!(
        output.document.attribute(script, None, "src").unwrap(),
        Some("changed.js")
    );
}

#[test]
fn every_empty_or_split_parser_script_boundary_is_delivered_once_in_order() {
    let mut parser = HtmlParser::default();
    let sources = std::cell::RefCell::new(Vec::new());
    let mut handler = |document: &mut Document, script: wild_buzzard_html::ParserInsertedScript| {
        assert_eq!(script.document_version(), document.version());
        assert_eq!(script.ordinal(), sources.borrow().len() as u64 + 1);
        sources.borrow_mut().push((
            document.text_content(script.node()).unwrap().unwrap(),
            document
                .attribute(script.node(), None, "type")
                .unwrap()
                .map(str::to_owned),
        ));
        Ok::<(), &'static str>(())
    };

    parser
        .feed_with_script_handler("<script></scr", &mut handler)
        .unwrap();
    assert!(sources.borrow().is_empty());
    parser
        .feed_with_script_handler(
            "ipt><div>between</div><script type=module>second</script>",
            &mut handler,
        )
        .unwrap();
    let output = parser.finish_with_script_handler(&mut handler).unwrap();
    assert_eq!(output.completed_script_boundaries(), 2);
    assert_eq!(
        *sources.borrow(),
        [
            (String::new(), None),
            ("second".to_owned(), Some("module".to_owned()))
        ]
    );
}

#[test]
fn later_script_freezes_the_current_first_base_after_prior_script_mutation() {
    let mut parser = HtmlParser::default();
    let mut boundary = 0_u64;
    let mut second_base = None;
    let mut handler = |document: &mut Document, script: wild_buzzard_html::ParserInsertedScript| {
        boundary += 1;
        assert_eq!(script.ordinal(), boundary);
        match boundary {
            1 => {
                let first_base = document.elements_by_tag_name("base").unwrap()[0];
                document
                    .set_html_attribute(first_base, "href", "https://two.invalid/")
                    .unwrap();
            }
            2 => second_base = script.start_tag().base_href().map(str::to_owned),
            _ => unreachable!(),
        }
        Ok::<(), &'static str>(())
    };

    parser
        .feed_with_script_handler(
            "<head><base href='https://one.invalid/'>\
             <script></script><script></script></head>",
            &mut handler,
        )
        .unwrap();
    let output = parser.finish_with_script_handler(&mut handler).unwrap();
    assert_eq!(output.completed_script_boundaries(), 2);
    assert_eq!(second_base.as_deref(), Some("https://two.invalid/"));
}

#[test]
fn head_and_body_script_handoffs_are_chunk_boundary_invariant() {
    let source = "<!doctype html><head><script>head&<b></scriptX>tail</script></head>\
                  <body><script>body</script><p>after</p>";
    let expected = parse_document(source).unwrap();

    for boundary in 0..=source.len() {
        let mut parser = HtmlParser::default();
        let observed = std::cell::RefCell::new(Vec::new());
        let mut handler =
            |document: &mut Document, script: wild_buzzard_html::ParserInsertedScript| {
                assert_eq!(script.document_version(), document.version());
                observed
                    .borrow_mut()
                    .push(document.text_content(script.node()).unwrap().unwrap());
                Ok::<(), &'static str>(())
            };
        parser
            .feed_with_script_handler(&source[..boundary], &mut handler)
            .unwrap();
        parser
            .feed_with_script_handler(&source[boundary..], &mut handler)
            .unwrap();
        let output = parser.finish_with_script_handler(&mut handler).unwrap();
        assert_eq!(
            *observed.borrow(),
            ["head&<b></scriptX>tail".to_owned(), "body".to_owned()],
            "boundary {boundary}"
        );
        assert_eq!(
            tree_shape(&output.document),
            tree_shape(&expected.document),
            "boundary {boundary}"
        );
    }

    let mut bytewise = HtmlParser::default();
    let observed = std::cell::RefCell::new(Vec::new());
    let mut handler = |document: &mut Document, script: wild_buzzard_html::ParserInsertedScript| {
        observed
            .borrow_mut()
            .push(document.text_content(script.node()).unwrap().unwrap());
        Ok::<(), &'static str>(())
    };
    for byte in source.as_bytes() {
        bytewise
            .feed_with_script_handler(
                std::str::from_utf8(std::slice::from_ref(byte)).unwrap(),
                &mut handler,
            )
            .unwrap();
    }
    let output = bytewise.finish_with_script_handler(&mut handler).unwrap();
    assert_eq!(
        *observed.borrow(),
        ["head&<b></scriptX>tail".to_owned(), "body".to_owned()]
    );
    assert_eq!(tree_shape(&output.document), tree_shape(&expected.document));
}

#[test]
fn eof_marks_an_unclosed_parser_script_malformed_without_executing_it() {
    let mut parser = HtmlParser::default();
    let called = std::cell::Cell::new(false);
    let mut handler = |_: &mut Document, _: wild_buzzard_html::ParserInsertedScript| {
        called.set(true);
        Ok::<(), &'static str>(())
    };
    parser
        .feed_with_script_handler("<script>unterminated", &mut handler)
        .unwrap();
    assert!(!called.get());
    let output = parser.finish_with_script_handler(&mut handler).unwrap();
    assert!(!called.get());
    let script = output.document.elements_by_tag_name("script").unwrap()[0];
    assert_eq!(
        output.document.text_content(script).unwrap().as_deref(),
        Some("unterminated")
    );
    assert!(
        output
            .errors
            .iter()
            .any(|error| error.code == ParseErrorCode::EofInScript)
    );
}

#[test]
fn script_handler_error_or_unwind_permanently_closes_incremental_parser() {
    let mut failed = HtmlParser::default();
    let error = failed
        .feed_with_script_handler("<script>stop</script><p>must-not-parse</p>", &mut |_, _| {
            Err::<(), _>("stop")
        })
        .unwrap_err();
    assert!(matches!(error, ScriptHandlerError::Handler("stop")));
    assert!(
        failed
            .document()
            .elements_by_tag_name("p")
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        failed.feed("<p>still-must-not-parse</p>"),
        Err(ParserStateError::ScriptHandlerAborted)
    ));

    let mut unwound = HtmlParser::default();
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = unwound.feed_with_script_handler(
            "<script>panic</script><p>must-not-parse</p>",
            &mut |_, _| -> Result<(), &'static str> { panic!("injected script-handler panic") },
        );
    }));
    assert!(panic.is_err());
    assert!(
        unwound
            .document()
            .elements_by_tag_name("p")
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        unwound.feed("<p>still-must-not-parse</p>"),
        Err(ParserStateError::ScriptHandlerAborted)
    ));
}

#[test]
fn implied_end_tags_close_paragraphs_and_list_items() {
    let output = parse_document("<ul><li>one<li>two</ul><p>alpha<div>beta</div>").unwrap();
    let document = &output.document;
    let items = document.elements_by_tag_name("li").unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(
        document.text_content(items[0]).unwrap().as_deref(),
        Some("one")
    );
    assert_eq!(
        document.text_content(items[1]).unwrap().as_deref(),
        Some("two")
    );

    let body_children = element_children(document, body(document));
    assert_eq!(
        body_children
            .iter()
            .map(|node| element_name(document, *node))
            .collect::<Vec<_>>(),
        vec!["ul", "p", "div"]
    );
    assert_eq!(
        document.text_content(body_children[1]).unwrap().as_deref(),
        Some("alpha")
    );
}

#[test]
fn mismatched_end_tag_pops_nested_elements_with_error() {
    let output = parse_document("<div><span>x</div><p>y").unwrap();
    assert!(
        output
            .errors
            .iter()
            .any(|error| error.code == ParseErrorCode::MismatchedEndTag)
    );
    let div = output.document.elements_by_tag_name("div").unwrap()[0];
    assert_eq!(
        output.document.text_content(div).unwrap().as_deref(),
        Some("x")
    );
    let paragraph = output.document.elements_by_tag_name("p").unwrap()[0];
    assert_eq!(
        output.document.parent(paragraph).unwrap(),
        Some(body(&output.document))
    );
}

#[test]
fn repeated_html_and_body_tags_only_add_missing_attributes() {
    let output =
        parse_document("<html lang=en><head></head><body id=first><body id=second class=added>x")
            .unwrap();
    let document = &output.document;
    let html = document.document_element().unwrap();
    let body = body(document);
    assert_eq!(document.attribute(html, None, "lang").unwrap(), Some("en"));
    assert_eq!(document.attribute(body, None, "id").unwrap(), Some("first"));
    assert_eq!(
        document.attribute(body, None, "class").unwrap(),
        Some("added")
    );
}

#[test]
fn raw_text_and_rcdata_have_distinct_entity_rules() {
    let output = parse_document(
        "<title>a&amp;b</title><body><script>if (a < b) &amp;</script><textarea>\nX&amp;Y</textarea>",
    )
    .unwrap();
    let document = &output.document;
    let title = document.elements_by_tag_name("title").unwrap()[0];
    let script = document.elements_by_tag_name("script").unwrap()[0];
    let textarea = document.elements_by_tag_name("textarea").unwrap()[0];
    assert_eq!(
        document.text_content(title).unwrap().as_deref(),
        Some("a&b")
    );
    assert_eq!(
        document.text_content(script).unwrap().as_deref(),
        Some("if (a < b) &amp;")
    );
    assert_eq!(
        document.text_content(textarea).unwrap().as_deref(),
        Some("X&Y")
    );
}

#[test]
fn comments_and_void_elements_do_not_corrupt_open_element_stack() {
    let output = parse_document("<div>a<br><!-- marker --><img src=x>b</div><p>c").unwrap();
    let document = &output.document;
    let div = document.elements_by_tag_name("div").unwrap()[0];
    assert_eq!(document.text_content(div).unwrap().as_deref(), Some("ab"));
    assert_eq!(document.elements_by_tag_name("br").unwrap().len(), 1);
    assert_eq!(document.elements_by_tag_name("img").unwrap().len(), 1);
    let paragraph = document.elements_by_tag_name("p").unwrap()[0];
    assert_eq!(document.parent(paragraph).unwrap(), Some(body(document)));
}

#[test]
fn configured_tree_depth_limit_recovers_without_breaking_dom_invariants() {
    let mut parser = HtmlParser::new(TokenizerLimits {
        max_tree_depth: 3,
        ..TokenizerLimits::default()
    });
    parser.feed("<div><span><b>x</b></span></div>").unwrap();
    let output = parser.finish().unwrap();
    assert!(
        output
            .errors
            .iter()
            .any(|error| error.code == ParseErrorCode::TreeDepthLimitExceeded)
    );
    output.document.validate_invariants().unwrap();
}
