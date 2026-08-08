use wild_buzzard_dom::{AttributeName, Document, DomError, NodeKind};

fn basic_document() -> (Document, wild_buzzard_dom::NodeId, wild_buzzard_dom::NodeId) {
    let mut document = Document::new();
    let html = document.create_html_element("html").unwrap();
    let body = document.create_html_element("body").unwrap();
    document
        .append_child(document.document_node(), html)
        .unwrap();
    document.append_child(html, body).unwrap();
    (document, html, body)
}

#[test]
fn stable_ids_survive_detach_and_reparent() {
    let (mut document, html, body) = basic_document();
    let first = document.create_html_element("section").unwrap();
    let second = document.create_html_element("section").unwrap();
    let text = document.create_text("buzzard").unwrap();
    document.append_child(body, first).unwrap();
    document.append_child(body, second).unwrap();
    document.append_child(first, text).unwrap();

    document.append_child(second, text).unwrap();
    assert_eq!(document.parent(text).unwrap(), Some(second));
    assert!(document.children(first).unwrap().is_empty());
    assert_eq!(
        document.text_content(second).unwrap().as_deref(),
        Some("buzzard")
    );

    document.detach(text).unwrap();
    assert_eq!(document.parent(text).unwrap(), None);
    assert!(matches!(document.node_kind(text), Ok(NodeKind::Text(data)) if data == "buzzard"));
    assert_eq!(document.document_element(), Some(html));
    document.validate_invariants().unwrap();
}

#[test]
fn failed_cycle_mutation_is_atomic() {
    let (mut document, html, body) = basic_document();
    let section = document.create_html_element("section").unwrap();
    document.append_child(body, section).unwrap();
    let before = document.snapshot().unwrap();

    assert_eq!(document.append_child(section, html), Err(DomError::Cycle));
    assert_eq!(
        document.snapshot().unwrap().nodes_in_document_order(),
        before.nodes_in_document_order()
    );
}

#[test]
fn document_child_constraints_match_preinsertion_shape() {
    let mut document = Document::new();
    let comment = document.create_comment("before").unwrap();
    let doctype = document.create_doctype("html", "", "").unwrap();
    let html = document.create_html_element("html").unwrap();
    document
        .append_child(document.document_node(), comment)
        .unwrap();
    document
        .append_child(document.document_node(), doctype)
        .unwrap();
    document
        .append_child(document.document_node(), html)
        .unwrap();

    let second_root = document.create_html_element("svg").unwrap();
    assert_eq!(
        document.append_child(document.document_node(), second_root),
        Err(DomError::HierarchyRequest)
    );
    let late_doctype = document.create_doctype("html", "", "").unwrap();
    assert_eq!(
        document.append_child(document.document_node(), late_doctype),
        Err(DomError::HierarchyRequest)
    );
    let text = document.create_text("not allowed").unwrap();
    assert_eq!(
        document.append_child(document.document_node(), text),
        Err(DomError::HierarchyRequest)
    );
    document.validate_invariants().unwrap();
}

#[test]
fn insert_replace_and_sibling_queries_preserve_order() {
    let (mut document, _, body) = basic_document();
    let a = document.create_html_element("a").unwrap();
    let b = document.create_html_element("b").unwrap();
    let c = document.create_html_element("c").unwrap();
    document.append_child(body, a).unwrap();
    document.append_child(body, c).unwrap();
    document.insert_before(body, b, Some(c)).unwrap();
    assert_eq!(document.children(body).unwrap(), &[a, b, c]);
    assert_eq!(document.previous_sibling(b).unwrap(), Some(a));
    assert_eq!(document.next_sibling(b).unwrap(), Some(c));

    let replacement = document.create_html_element("strong").unwrap();
    document.replace_child(body, replacement, b).unwrap();
    assert_eq!(document.children(body).unwrap(), &[a, replacement, c]);
    assert_eq!(document.parent(b).unwrap(), None);
    document.validate_invariants().unwrap();
}

#[test]
fn replace_with_same_parent_sibling_uses_post_removal_position() {
    let (mut document, _, body) = basic_document();
    let early = document.create_html_element("early").unwrap();
    let middle = document.create_html_element("middle").unwrap();
    let old = document.create_html_element("old").unwrap();
    let tail = document.create_html_element("tail").unwrap();
    for child in [early, middle, old, tail] {
        document.append_child(body, child).unwrap();
    }

    document.replace_child(body, early, old).unwrap();
    assert_eq!(document.children(body).unwrap(), &[middle, early, tail]);
    assert_eq!(document.parent(old).unwrap(), None);
    document.validate_invariants().unwrap();

    let later = document.create_html_element("later").unwrap();
    document.append_child(body, later).unwrap();
    document.replace_child(body, later, middle).unwrap();
    assert_eq!(document.children(body).unwrap(), &[later, early, tail]);
    assert_eq!(document.parent(middle).unwrap(), None);
    document.validate_invariants().unwrap();
}

#[test]
fn attributes_keep_order_and_queries_follow_document_order() {
    let (mut document, _, body) = basic_document();
    let first = document.create_html_element("p").unwrap();
    let second = document.create_html_element("P").unwrap();
    document.set_html_attribute(first, "id", "target").unwrap();
    document
        .set_attribute(first, AttributeName::html("class"), "first")
        .unwrap();
    assert_eq!(
        document
            .set_html_attribute(first, "id", "target-2")
            .unwrap(),
        Some("target".into())
    );
    document
        .set_html_attribute(second, "id", "target-2")
        .unwrap();
    document.append_child(body, first).unwrap();
    document.append_child(body, second).unwrap();

    assert_eq!(document.element_by_id("target-2").unwrap(), Some(first));
    assert_eq!(
        document.elements_by_tag_name("p").unwrap(),
        vec![first, second]
    );
    let NodeKind::Element(data) = document.node_kind(first).unwrap() else {
        panic!("expected element");
    };
    assert_eq!(data.attributes[0].name.local_name, "id");
    assert_eq!(data.attributes[1].name.local_name, "class");
    assert_eq!(
        document
            .remove_attribute(first, None, "id")
            .unwrap()
            .unwrap()
            .value,
        "target-2"
    );
}

#[test]
fn foreign_document_handles_are_rejected() {
    let (mut first, _, body) = basic_document();
    let mut second = Document::new();
    let alien = second.create_html_element("aside").unwrap();
    assert!(matches!(
        first.append_child(body, alien),
        Err(DomError::WrongDocument { .. })
    ));
}

#[test]
fn snapshots_are_owned_and_revisioned() {
    let (mut document, _, body) = basic_document();
    let snapshot = document.snapshot().unwrap();
    let snapshot_revision = snapshot.revision();
    let text = document.append_text(body, "one").unwrap();
    document.append_text(body, " two").unwrap();

    assert!(snapshot.node(text).is_none());
    assert!(document.revision() > snapshot_revision);
    assert_eq!(
        document.text_content(body).unwrap().as_deref(),
        Some("one two")
    );
    assert_eq!(document.children(body).unwrap(), &[text]);
}

#[test]
fn deep_preorder_text_and_snapshot_walks_are_iterative() {
    const DEPTH: usize = 1024;

    let mut document = Document::new();
    let html = document.create_html_element("html").unwrap();
    document
        .append_child(document.document_node(), html)
        .unwrap();
    let mut parent = html;
    for _ in 0..DEPTH {
        let child = document.create_html_element("div").unwrap();
        document.append_child(parent, child).unwrap();
        parent = child;
    }
    document.append_text(parent, "deep").unwrap();

    assert_eq!(
        document.text_content(html).unwrap().as_deref(),
        Some("deep")
    );
    assert_eq!(document.document_order().unwrap().len(), DEPTH + 3);
    assert_eq!(
        document.snapshot().unwrap().nodes_in_document_order().len(),
        DEPTH + 3
    );
}
