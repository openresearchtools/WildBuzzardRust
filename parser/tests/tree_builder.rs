use wild_buzzard_dom::{Document, NodeId, NodeKind};
use wild_buzzard_html::{
    DocumentMode, HtmlParser, ParseErrorCode, TokenizerLimits, parse_document,
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
