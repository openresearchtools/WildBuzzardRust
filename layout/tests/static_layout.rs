use wild_buzzard_dom::{Document, NodeId};
use wild_buzzard_html::parse_document;
use wild_buzzard_layout::{
    Au, BoxKind, BoxSizing, ComputedStyle, ComputedStyleSnapshot, ComputedStyleSnapshotError,
    ComputedStyleSnapshotLimits, Display, InitialStyleResolver, LayoutError, LayoutLimits,
    LayoutPhase, MaxSizeValue, MonospaceTextMeasurer, PercentageEdges, SizeValue, StyleInput,
    StyleResolver, Viewport, WritingMode, layout_document, layout_document_with_limits,
};

fn parsed(source: &str) -> Document {
    parse_document(source).unwrap().document
}

fn node(document: &Document, tag: &str) -> NodeId {
    document.elements_by_tag_name(tag).unwrap()[0]
}

fn nested_block_document(element_depth: usize) -> Document {
    assert!(element_depth >= 2);
    let mut document = Document::new();
    let html = document.create_html_element("html").unwrap();
    let body = document.create_html_element("body").unwrap();
    document
        .append_child(document.document_node(), html)
        .unwrap();
    document.append_child(html, body).unwrap();
    let mut parent = body;
    for _ in 2..element_depth {
        let child = document.create_html_element("div").unwrap();
        document.append_child(parent, child).unwrap();
        parent = child;
    }
    document
}

#[test]
fn parsed_dom_flows_through_snapshot_to_block_inline_boxes() {
    let document = parsed("<main><p>Hello <span>Wild</span> Buzzard</p><p>Second line</p></main>");
    let snapshot = document.snapshot().unwrap();
    let output = layout_document(
        &snapshot,
        Viewport::from_css_pixels(200, 100),
        &InitialStyleResolver,
        &MonospaceTextMeasurer,
    )
    .unwrap();

    let root = output.box_by_id(output.root.unwrap()).unwrap();
    assert_eq!(root.kind, BoxKind::Block);
    assert_eq!(root.node_id, document.document_element());
    let body = node(&document, "body");
    let body_box = output.boxes_for_node(body).next().unwrap();
    assert_eq!(body_box.fragments[0].rect.origin.x, Au::from_px(8));
    assert_eq!(body_box.fragments[0].rect.size.width, Au::from_px(184));

    let span = node(&document, "span");
    let span_box = output.boxes_for_node(span).next().unwrap();
    assert_eq!(span_box.kind, BoxKind::Inline);
    assert_eq!(span_box.fragments.len(), 1);
    assert!(output.warnings.is_empty());
}

#[test]
fn narrow_inline_context_wraps_at_words_deterministically() {
    let document = parsed("<body>one two six</body>");
    let output = layout_document(
        &document.snapshot().unwrap(),
        Viewport::from_css_pixels(40, 20),
        &InitialStyleResolver,
        &MonospaceTextMeasurer,
    )
    .unwrap();
    let text_node = document.children(node(&document, "body")).unwrap()[0];
    let text_box = output.boxes_for_node(text_node).next().unwrap();
    assert_eq!(text_box.fragments.len(), 3);
    assert_eq!(
        text_box
            .fragments
            .iter()
            .map(|fragment| fragment.text.as_deref().unwrap())
            .collect::<Vec<_>>(),
        vec!["one", "two", "six"]
    );
    assert!(text_box.fragments[0].rect.origin.y < text_box.fragments[1].rect.origin.y);
    assert!(text_box.fragments[1].rect.origin.y < text_box.fragments[2].rect.origin.y);
}

#[test]
fn a_word_wider_than_the_line_breaks_at_character_boundaries() {
    let document = parsed("<body>abcd</body>");
    let output = layout_document(
        &document.snapshot().unwrap(),
        Viewport::from_css_pixels(32, 20),
        &InitialStyleResolver,
        &MonospaceTextMeasurer,
    )
    .unwrap();
    let text_node = document.children(node(&document, "body")).unwrap()[0];
    let text_box = output.boxes_for_node(text_node).next().unwrap();
    assert_eq!(
        text_box
            .fragments
            .iter()
            .map(|fragment| fragment.text.as_deref().unwrap())
            .collect::<Vec<_>>(),
        vec!["ab", "cd"]
    );
}

#[test]
fn block_boxes_wrap_each_contiguous_inline_run() {
    let document = parsed("<body>A<span>B</span><div>C</div>D</body>");
    let output = layout_document(
        &document.snapshot().unwrap(),
        Viewport::from_css_pixels(200, 100),
        &InitialStyleResolver,
        &MonospaceTextMeasurer,
    )
    .unwrap();
    let body_box = output
        .boxes_for_node(node(&document, "body"))
        .next()
        .unwrap();
    assert_eq!(body_box.children.len(), 3);
    assert_eq!(
        body_box
            .children
            .iter()
            .map(|child| output.box_by_id(*child).unwrap().kind)
            .collect::<Vec<_>>(),
        vec![
            BoxKind::AnonymousBlock,
            BoxKind::Block,
            BoxKind::AnonymousBlock
        ]
    );
}

struct AttributeStyles;

impl StyleResolver for AttributeStyles {
    fn resolve(&self, input: StyleInput<'_>) -> ComputedStyle {
        let requested = input
            .element
            .html_attribute("data-display")
            .map(str::to_owned);
        let padding = input.element.html_attribute("data-padding").is_some();
        let mut style = InitialStyleResolver.resolve(input);
        if requested.as_deref() == Some("none") {
            style.display = Display::None;
        } else if requested.as_deref() == Some("block") {
            style.display = Display::Block;
        }
        if padding {
            style.padding = wild_buzzard_layout::Edges::all(Au::from_px(4));
        }
        style
    }
}

#[test]
fn style_adapter_controls_box_generation_and_geometry() {
    let document = parsed(
        "<body><span data-display=none>hidden</span><span data-display=block data-padding>shown</span></body>",
    );
    let spans = document.elements_by_tag_name("span").unwrap();
    let output = layout_document(
        &document.snapshot().unwrap(),
        Viewport::from_css_pixels(100, 50),
        &AttributeStyles,
        &MonospaceTextMeasurer,
    )
    .unwrap();
    assert_eq!(output.boxes_for_node(spans[0]).count(), 0);
    let shown = output.boxes_for_node(spans[1]).next().unwrap();
    assert_eq!(shown.kind, BoxKind::Block);
    assert_eq!(shown.fragments[0].rect.size.width, Au::from_px(84));
    assert!(shown.fragments[0].rect.size.height > Au::from_px(8));
}

#[test]
fn normal_whitespace_collapses_across_inline_nodes_and_br_forces_a_line() {
    let document = parsed("<body>one <span> two </span> three<br>four</body>");
    let output = layout_document(
        &document.snapshot().unwrap(),
        Viewport::from_css_pixels(200, 40),
        &InitialStyleResolver,
        &MonospaceTextMeasurer,
    )
    .unwrap();
    let texts = output
        .boxes
        .iter()
        .flat_map(|layout_box| layout_box.fragments.iter())
        .filter_map(|fragment| fragment.text.as_deref())
        .collect::<Vec<_>>();
    assert_eq!(texts, vec!["one", " two", " three", "four"]);
    let four_y = output
        .boxes
        .iter()
        .flat_map(|layout_box| layout_box.fragments.iter())
        .find(|fragment| fragment.text.as_deref() == Some("four"))
        .unwrap()
        .rect
        .origin
        .y;
    let one_y = output
        .boxes
        .iter()
        .flat_map(|layout_box| layout_box.fragments.iter())
        .find(|fragment| fragment.text.as_deref() == Some("one"))
        .unwrap()
        .rect
        .origin
        .y;
    assert!(four_y > one_y);
}

#[test]
fn css_collapse_preserves_nbsp_and_other_unicode_spaces() {
    let document = parsed("<body>a&nbsp;b | c \t\n d | e\u{2003}f</body>");
    let output = layout_document(
        &document.snapshot().unwrap(),
        Viewport::from_css_pixels(400, 40),
        &InitialStyleResolver,
        &MonospaceTextMeasurer,
    )
    .unwrap();
    let texts = output
        .boxes
        .iter()
        .flat_map(|layout_box| layout_box.fragments.iter())
        .filter_map(|fragment| fragment.text.as_deref())
        .collect::<Vec<_>>();
    assert_eq!(
        texts,
        vec!["a\u{00a0}b", " |", " c", " d", " |", " e\u{2003}f"]
    );
}

#[test]
fn layout_consumes_an_owned_revisioned_snapshot() {
    let mut document = parsed("<body>before</body>");
    let snapshot = document.snapshot().unwrap();
    let first = layout_document(
        &snapshot,
        Viewport::from_css_pixels(80, 40),
        &InitialStyleResolver,
        &MonospaceTextMeasurer,
    )
    .unwrap();
    let body = node(&document, "body");
    document.append_text(body, " after").unwrap();
    let second = layout_document(
        &snapshot,
        Viewport::from_css_pixels(80, 40),
        &InitialStyleResolver,
        &MonospaceTextMeasurer,
    )
    .unwrap();
    assert_eq!(first, second);
    assert_eq!(first.document_revision, snapshot.revision());
    assert!(document.revision() > first.document_revision);
}

#[test]
fn rejects_non_positive_viewports() {
    let document = parsed("<p>x");
    assert_eq!(
        layout_document(
            &document.snapshot().unwrap(),
            Viewport::from_css_pixels(0, 100),
            &InitialStyleResolver,
            &MonospaceTextMeasurer,
        ),
        Err(LayoutError::InvalidViewport)
    );
}

#[test]
fn default_layout_depth_limit_accepts_boundary_and_rejects_next_level() {
    let limit = LayoutLimits::default().max_tree_depth;
    let at_limit = nested_block_document(limit);
    layout_document(
        &at_limit.snapshot().unwrap(),
        Viewport::from_css_pixels(100, 100),
        &InitialStyleResolver,
        &MonospaceTextMeasurer,
    )
    .unwrap();

    let over_limit = nested_block_document(limit + 1);
    assert!(matches!(
        layout_document(
            &over_limit.snapshot().unwrap(),
            Viewport::from_css_pixels(100, 100),
            &InitialStyleResolver,
            &MonospaceTextMeasurer,
        ),
        Err(LayoutError::TreeDepthLimitExceeded {
            limit: reported,
            node_id: Some(_),
            phase: LayoutPhase::BoxConstruction,
        }) if reported == limit
    ));
}

#[test]
fn anonymous_inline_depth_growth_is_checked_during_inline_layout() {
    let document = parsed("<body><span><span>x</span></span></body>");
    assert!(matches!(
        layout_document_with_limits(
            &document.snapshot().unwrap(),
            Viewport::from_css_pixels(100, 100),
            &InitialStyleResolver,
            &MonospaceTextMeasurer,
            LayoutLimits { max_tree_depth: 5 },
        ),
        Err(LayoutError::TreeDepthLimitExceeded {
            limit: 5,
            node_id: Some(_),
            phase: LayoutPhase::InlineLayout,
        })
    ));
}

#[test]
fn computed_style_publication_requires_exactly_one_style_per_element() {
    let document = parsed("<body><p>x</p></body>");
    let snapshot = document.snapshot().unwrap();
    let elements = snapshot
        .nodes_in_document_order()
        .iter()
        .filter(|node| node.kind.is_element())
        .map(|node| node.id)
        .collect::<Vec<_>>();
    let missing = elements[0];
    assert!(matches!(
        ComputedStyleSnapshot::try_new(
            &snapshot,
            elements[1..]
                .iter()
                .copied()
                .map(|node| (node, ComputedStyle::default())),
            ComputedStyleSnapshotLimits::default(),
        ),
        Err(ComputedStyleSnapshotError::MissingStyle(node)) if node == missing
    ));
    assert!(matches!(
        ComputedStyleSnapshot::try_new(
            &snapshot,
            elements
                .iter()
                .copied()
                .map(|node| (node, ComputedStyle::default())),
            ComputedStyleSnapshotLimits {
                max_entries: elements.len() - 1,
            },
        ),
        Err(ComputedStyleSnapshotError::EntryCapacityExceeded { .. })
    ));
}

struct InlinePercentageStyles;

impl StyleResolver for InlinePercentageStyles {
    fn resolve(&self, input: StyleInput<'_>) -> ComputedStyle {
        let is_span = input.element.name.local_name == "span";
        let mut style = InitialStyleResolver.resolve(input);
        if is_span {
            style.padding_percentage = PercentageEdges::all(100_000);
        }
        style
    }
}

#[test]
fn ignored_inline_percentage_edges_are_reported() {
    let document = parsed("<body><span>x</span></body>");
    let span = node(&document, "span");
    let output = layout_document(
        &document.snapshot().unwrap(),
        Viewport::from_css_pixels(100, 100),
        &InlinePercentageStyles,
        &MonospaceTextMeasurer,
    )
    .unwrap();
    assert!(output.warnings.iter().any(|warning| {
        warning.node_id == Some(span)
            && warning.code == wild_buzzard_layout::LayoutWarningCode::InlineEdgesNotApplied
    }));
}

struct SizingStyles;

impl StyleResolver for SizingStyles {
    fn resolve(&self, input: StyleInput<'_>) -> ComputedStyle {
        let id = input.element.html_attribute("id");
        let mut style = InitialStyleResolver.resolve(input);
        if matches!(id, Some("content-box") | Some("border-box")) {
            style.display = Display::Block;
            style.width = SizeValue::length(Au::from_px(50));
            style.height = SizeValue::length(Au::from_px(10));
            style.padding = wild_buzzard_layout::Edges::all(Au::from_px(5));
            style.border = wild_buzzard_layout::Edges::all(Au::from_px(2));
        }
        if id == Some("content-box") {
            style.min_width = SizeValue::length(Au::from_px(70));
            style.max_width = MaxSizeValue::length(Au::from_px(60));
            style.min_height = SizeValue::length(Au::from_px(20));
            style.max_height = MaxSizeValue::length(Au::from_px(15));
        } else if id == Some("border-box") {
            style.box_sizing = BoxSizing::BorderBox;
            style.height = SizeValue::length(Au::from_px(30));
        }
        style
    }
}

#[test]
fn block_layout_honors_sizes_constraints_and_box_sizing() {
    let document = parsed("<body><div id=content-box></div><div id=border-box></div></body>");
    let content_box = document.elements_by_tag_name("div").unwrap()[0];
    let border_box = document.elements_by_tag_name("div").unwrap()[1];
    let output = layout_document(
        &document.snapshot().unwrap(),
        Viewport::from_css_pixels(200, 200),
        &SizingStyles,
        &MonospaceTextMeasurer,
    )
    .unwrap();
    let content_fragment = &output.boxes_for_node(content_box).next().unwrap().fragments[0];
    let border_fragment = &output.boxes_for_node(border_box).next().unwrap().fragments[0];
    assert_eq!(content_fragment.rect.size.width, Au::from_px(84));
    assert_eq!(content_fragment.rect.size.height, Au::from_px(34));
    assert_eq!(border_fragment.rect.size.width, Au::from_px(50));
    assert_eq!(border_fragment.rect.size.height, Au::from_px(30));
}

struct VerticalStyles;

impl StyleResolver for VerticalStyles {
    fn resolve(&self, input: StyleInput<'_>) -> ComputedStyle {
        let node = input.node_id;
        let mut style = InitialStyleResolver.resolve(input);
        if node.slot() > 0 && style.display != Display::None {
            style.writing_mode = WritingMode::VerticalLr;
        }
        style
    }
}

#[test]
fn unsupported_writing_mode_is_not_silently_laid_out_horizontally() {
    let document = parsed("<div>x</div>");
    let root = document.document_element().unwrap();
    assert!(matches!(
        layout_document(
            &document.snapshot().unwrap(),
            Viewport::from_css_pixels(200, 200),
            &VerticalStyles,
            &MonospaceTextMeasurer,
        ),
        Err(LayoutError::UnsupportedWritingMode {
            node,
            writing_mode: WritingMode::VerticalLr,
        }) if node == root
    ));
}
