use wild_buzzard_dom::{Document, NodeId, NodeKind};
use wild_buzzard_html::parse_document;
use wild_buzzard_layout::{
    Au, AutomaticMarginContext, BackgroundImageLayers, BoxKind, BoxSizing, CanvasBackgroundSource,
    Color, ComputedStyle, ComputedStyleSnapshot, ComputedStyleSnapshotError,
    ComputedStyleSnapshotLimits, Display, Edges, EffectiveContainment, FlexBasis, FlexDirection,
    FlexFactor, FlexWrap, InitialStyleResolver, InlineDirection, LayoutError, LayoutLimits,
    LayoutPhase, LengthPercentage, MaxSizeValue, MonospaceTextMeasurer, PercentageEdges, Point,
    SizeValue, StyleInput, StyleResolver, TextMeasurer, TextMetrics, Viewport, WhiteSpace,
    WritingMode, layout_document, layout_document_with_limits, layout_document_with_style_snapshot,
};

fn parsed(source: &str) -> Document {
    parse_document(source).unwrap().document
}

fn node(document: &Document, tag: &str) -> NodeId {
    document.elements_by_tag_name(tag).unwrap()[0]
}

fn node_with_id(document: &Document, id: &str) -> NodeId {
    document
        .snapshot()
        .unwrap()
        .nodes_in_document_order()
        .iter()
        .find_map(|node| match &node.kind {
            NodeKind::Element(element) if element.html_attribute("id") == Some(id) => Some(node.id),
            _ => None,
        })
        .unwrap_or_else(|| panic!("missing element #{id}"))
}

const fn point(x: Au, y: Au) -> Point {
    Point { x, y }
}

const fn rgba(red: u8, green: u8, blue: u8, alpha: u8) -> Color {
    Color {
        red,
        green,
        blue,
        alpha,
    }
}

struct CanvasStyles;

impl StyleResolver for CanvasStyles {
    fn resolve(&self, input: StyleInput<'_>) -> ComputedStyle {
        let background = input.element.html_attribute("data-canvas-bg");
        let display = input.element.html_attribute("data-canvas-display");
        let image_layers = input.element.html_attribute("data-canvas-images");
        let containment = input.element.html_attribute("data-canvas-contain");
        let mut style = InitialStyleResolver.resolve(input);
        style.background_color = match background {
            Some("gray") => rgba(238, 238, 238, 255),
            Some("red") => rgba(220, 20, 30, 255),
            Some("green") => rgba(30, 180, 70, 255),
            _ => style.background_color,
        };
        style.display = match display {
            Some("inline") => Display::Inline,
            Some("none") => Display::None,
            _ => style.display,
        };
        style.background_image_layers = match image_layers {
            Some("meaningful") => BackgroundImageLayers::Meaningful,
            Some("unknown") => BackgroundImageLayers::Unknown,
            _ => style.background_image_layers,
        };
        style.effective_containment = match containment {
            Some("any") => EffectiveContainment::Any,
            Some("unknown") => EffectiveContainment::Unknown,
            _ => style.effective_containment,
        };
        style
    }
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
fn computed_style_snapshot_selects_the_canonical_html_body_for_the_canvas() {
    let document = parsed("<body data-canvas-bg=gray>Example</body>");
    let snapshot = document.snapshot().unwrap();
    let entries = snapshot
        .nodes_in_document_order()
        .iter()
        .filter_map(|snapshot_node| {
            let NodeKind::Element(element) = &snapshot_node.kind else {
                return None;
            };
            Some((
                snapshot_node.id,
                CanvasStyles.resolve(StyleInput {
                    node_id: snapshot_node.id,
                    node: snapshot_node,
                    element,
                    parent_style: None,
                }),
            ))
        });
    let styles =
        ComputedStyleSnapshot::try_new(&snapshot, entries, ComputedStyleSnapshotLimits::default())
            .unwrap();
    let output = layout_document_with_style_snapshot(
        &snapshot,
        Viewport::from_css_pixels(1366, 768),
        &styles,
        &MonospaceTextMeasurer,
    )
    .unwrap();
    let body_box = output
        .boxes_for_node(node(&document, "body"))
        .next()
        .unwrap();
    let canvas = output.canvas_background().expect("body color propagates");

    assert_eq!(canvas.color(), rgba(238, 238, 238, 255));
    assert_eq!(canvas.source(), CanvasBackgroundSource::HtmlBody);
    assert_eq!(canvas.source_box(), body_box.id);
    assert_eq!(
        output
            .box_by_id(output.root.unwrap())
            .unwrap()
            .canvas_background(),
        Some(canvas)
    );
    assert!(
        output
            .boxes
            .iter()
            .filter(|layout_box| layout_box.id != output.root.unwrap())
            .all(|layout_box| layout_box.canvas_background_decision().is_none())
    );
}

#[test]
fn meaningful_root_background_wins_and_inline_body_remains_eligible_for_fallback() {
    let root_document =
        parsed("<html data-canvas-bg=red><body data-canvas-bg=green>root precedence</body></html>");
    let root_output = layout_document(
        &root_document.snapshot().unwrap(),
        Viewport::from_css_pixels(320, 180),
        &CanvasStyles,
        &MonospaceTextMeasurer,
    )
    .unwrap();
    let root_id = root_output.root.unwrap();
    let root_canvas = root_output.canvas_background().unwrap();
    assert_eq!(root_canvas.color(), rgba(220, 20, 30, 255));
    assert_eq!(root_canvas.source(), CanvasBackgroundSource::RootElement);
    assert_eq!(root_canvas.source_box(), root_id);

    let inline_document =
        parsed("<body data-canvas-display=inline data-canvas-bg=green>inline fallback</body>");
    let inline_output = layout_document(
        &inline_document.snapshot().unwrap(),
        Viewport::from_css_pixels(320, 180),
        &CanvasStyles,
        &MonospaceTextMeasurer,
    )
    .unwrap();
    let inline_body = inline_output
        .boxes_for_node(node(&inline_document, "body"))
        .next()
        .unwrap();
    assert_eq!(inline_body.kind, BoxKind::Inline);
    assert_eq!(
        inline_output.canvas_background().unwrap().source(),
        CanvasBackgroundSource::HtmlBody
    );
    assert_eq!(
        inline_output.canvas_background().unwrap().source_box(),
        inline_body.id
    );
}

#[test]
fn root_image_or_unknown_state_blocks_body_fallback_without_fabricating_a_color() {
    for root_fact in ["meaningful", "unknown"] {
        let document = parsed(&format!(
            "<html data-canvas-images={root_fact}><body data-canvas-bg=green>local body</body></html>"
        ));
        let output = layout_document(
            &document.snapshot().unwrap(),
            Viewport::from_css_pixels(320, 180),
            &CanvasStyles,
            &MonospaceTextMeasurer,
        )
        .unwrap();
        assert_eq!(output.canvas_background(), None);
        assert!(output.canvas_background_decision().is_some());
    }
}

#[test]
fn root_and_body_containment_block_fallback_but_meaningful_root_precedes_containment() {
    for containment in ["any", "unknown"] {
        let root_contained = parsed(&format!(
            "<html data-canvas-contain={containment}><body data-canvas-bg=green>root contain</body></html>"
        ));
        let root_contained_output = layout_document(
            &root_contained.snapshot().unwrap(),
            Viewport::from_css_pixels(320, 180),
            &CanvasStyles,
            &MonospaceTextMeasurer,
        )
        .unwrap();
        assert_eq!(root_contained_output.canvas_background(), None);

        let body_contained = parsed(&format!(
            "<html><body data-canvas-contain={containment} data-canvas-bg=green>body contain</body></html>"
        ));
        let body_contained_output = layout_document(
            &body_contained.snapshot().unwrap(),
            Viewport::from_css_pixels(320, 180),
            &CanvasStyles,
            &MonospaceTextMeasurer,
        )
        .unwrap();
        assert_eq!(body_contained_output.canvas_background(), None);
    }

    let meaningful_root = parsed(
        "<html data-canvas-contain=any data-canvas-bg=red><body data-canvas-bg=green>root wins</body></html>",
    );
    let meaningful_root_output = layout_document(
        &meaningful_root.snapshot().unwrap(),
        Viewport::from_css_pixels(320, 180),
        &CanvasStyles,
        &MonospaceTextMeasurer,
    )
    .unwrap();
    assert_eq!(
        meaningful_root_output.canvas_background().unwrap().source(),
        CanvasBackgroundSource::RootElement
    );
}

#[test]
fn transparent_or_nonrendered_body_does_not_fabricate_a_canvas_background() {
    let transparent = parsed("<body>transparent</body>");
    let transparent_output = layout_document(
        &transparent.snapshot().unwrap(),
        Viewport::from_css_pixels(320, 180),
        &CanvasStyles,
        &MonospaceTextMeasurer,
    )
    .unwrap();
    assert_eq!(transparent_output.canvas_background(), None);

    let hidden = parsed("<body data-canvas-display=none data-canvas-bg=green>hidden</body>");
    let hidden_output = layout_document(
        &hidden.snapshot().unwrap(),
        Viewport::from_css_pixels(320, 180),
        &CanvasStyles,
        &MonospaceTextMeasurer,
    )
    .unwrap();
    assert_eq!(hidden_output.canvas_background(), None);
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

struct InlineBlockStyles;

impl StyleResolver for InlineBlockStyles {
    fn resolve(&self, input: StyleInput<'_>) -> ComputedStyle {
        let id = input.element.html_attribute("id");
        let is_body = input.element.name.local_name.as_str() == "body";
        let width = input.element.html_attribute("data-width");
        let nowrap = input.element.html_attribute("data-nowrap").is_some();
        let normal = input.element.html_attribute("data-normal").is_some();
        let pre = input.element.html_attribute("data-pre").is_some();
        let edges = input.element.html_attribute("data-edges").is_some();
        let auto_margin = input.element.html_attribute("data-auto-margin").is_some();
        let overflow = input.element.html_attribute("data-overflow").is_some();
        let vertical_overflow = input
            .element
            .html_attribute("data-vertical-overflow")
            .is_some();
        let wrap_y_overflow = input
            .element
            .html_attribute("data-wrap-y-overflow")
            .is_some();
        let mut style = InitialStyleResolver.resolve(input);
        if is_body {
            style.margin = Edges::default();
        }
        if nowrap {
            style.white_space = WhiteSpace::Nowrap;
        } else if normal {
            style.white_space = WhiteSpace::Normal;
        } else if pre {
            style.white_space = WhiteSpace::Pre;
        }
        match id {
            Some("line") => {
                style.display = Display::Block;
                style.width = SizeValue::length(match width {
                    Some("40") => Au::from_px(40),
                    Some("48") => Au::from_px(48),
                    _ => Au::from_px(80),
                });
                style.line_height = Au::from_px(10);
                if wrap_y_overflow {
                    style.margin.top = Au::from_raw(1);
                    style.line_height = Au::from_raw(i32::MAX);
                }
            }
            Some("atom" | "atom-a" | "atom-b") => {
                style.display = Display::InlineBlock;
                style.width = match width {
                    Some("auto") => SizeValue::Auto,
                    Some("overflow") => SizeValue::length(Au::from_raw(i32::MAX)),
                    Some("40") => SizeValue::length(Au::from_px(40)),
                    _ => SizeValue::length(Au::from_px(30)),
                };
                style.height = SizeValue::length(Au::from_px(20));
                if edges {
                    style.margin = Edges {
                        top: Au::from_px(4),
                        right: Au::from_px(3),
                        bottom: Au::from_px(5),
                        left: Au::from_px(2),
                    };
                    style.border = Edges::all(Au::from_px(1));
                    style.padding = Edges::all(Au::from_px(2));
                    style.background_color = rgba(20, 120, 60, 255);
                }
                if overflow {
                    style.border.right = Au::from_px(1);
                }
                if vertical_overflow {
                    style.height = SizeValue::length(Au::from_raw(i32::MAX));
                    style.margin.top = Au::from_raw(1);
                }
                if auto_margin {
                    style.margin.left = Au::from_px(99);
                    style.margin.right = Au::from_px(99);
                    style.automatic_margin.left = true;
                    style.automatic_margin.right = true;
                }
            }
            Some("child") => {
                style.display = Display::Block;
                style.width = SizeValue::length(Au::from_px(10));
                style.height = SizeValue::length(Au::from_px(30));
            }
            _ => {}
        }
        style
    }
}

struct ShrinkToFitStyles;

impl StyleResolver for ShrinkToFitStyles {
    fn resolve(&self, input: StyleInput<'_>) -> ComputedStyle {
        let mut style = InitialStyleResolver.resolve(StyleInput {
            node_id: input.node_id,
            node: input.node,
            element: input.element,
            parent_style: input.parent_style,
        });
        let attribute = |name| input.element.html_attribute(name);
        let pixels = |name| {
            attribute(name).map(|value| {
                Au::from_px(
                    value
                        .parse::<i32>()
                        .unwrap_or_else(|_| panic!("invalid {name} test value {value:?}")),
                )
            })
        };
        let percentage = |name| {
            attribute(name).map(|value| {
                value
                    .parse::<i32>()
                    .unwrap_or_else(|_| panic!("invalid {name} test value {value:?}"))
                    * 10_000
            })
        };
        let size = |name| {
            attribute(name).map(|value| match value {
                "auto" => SizeValue::Auto,
                "half-plus-10" => SizeValue::LengthPercentage(LengthPercentage {
                    length: Au::from_px(10),
                    percentage: 500_000,
                }),
                "half-plus-20" => SizeValue::LengthPercentage(LengthPercentage {
                    length: Au::from_px(20),
                    percentage: 500_000,
                }),
                _ => SizeValue::length(Au::from_px(
                    value
                        .parse::<i32>()
                        .unwrap_or_else(|_| panic!("invalid {name} test value {value:?}")),
                )),
            })
        };
        let maximum = |name| {
            attribute(name).map(|value| match value {
                "half" => MaxSizeValue::percentage(500_000),
                "half-plus-10" => MaxSizeValue::LengthPercentage(LengthPercentage {
                    length: Au::from_px(10),
                    percentage: 500_000,
                }),
                _ => MaxSizeValue::length(Au::from_px(
                    value
                        .parse::<i32>()
                        .unwrap_or_else(|_| panic!("invalid {name} test value {value:?}")),
                )),
            })
        };

        if input.element.name.local_name.as_str() == "body" {
            style.margin = Edges::default();
        }
        style.display = match attribute("data-display") {
            Some("block") => Display::Block,
            Some("inline") => Display::Inline,
            Some("inline-block") => Display::InlineBlock,
            Some("flex") => Display::Flex,
            Some(value) => panic!("invalid data-display test value {value:?}"),
            None => style.display,
        };
        if attribute("data-stf").is_some() {
            style.display = Display::InlineBlock;
            style.width = SizeValue::Auto;
        }
        if attribute("data-intrinsic-overflow").is_some() {
            style.display = Display::InlineBlock;
            style.width = SizeValue::length(Au::from_raw(i32::MAX));
            style.margin.right = Au::from_raw(1);
        }
        if attribute("data-available-overflow").is_some() {
            style.display = Display::InlineBlock;
            style.width = SizeValue::Auto;
            style.margin.left = Au::from_raw(i32::MIN);
        }
        if let Some(width) = pixels("data-line-width") {
            style.display = Display::Block;
            style.width = SizeValue::length(width);
        }
        if let Some(width) = size("data-width") {
            style.width = width;
        }
        if let Some(minimum) = size("data-min-width") {
            style.min_width = minimum;
        }
        if let Some(maximum) = maximum("data-max-width") {
            style.max_width = maximum;
        }
        if let Some(minimum) = percentage("data-min-percent") {
            style.min_width = SizeValue::percentage(minimum);
        }
        if attribute("data-border-box").is_some() {
            style.box_sizing = BoxSizing::BorderBox;
        }
        if attribute("data-nowrap").is_some() {
            style.white_space = WhiteSpace::Nowrap;
        }
        if attribute("data-pre").is_some() {
            style.white_space = WhiteSpace::Pre;
        }
        style.margin.left = pixels("data-margin-left").unwrap_or(style.margin.left);
        style.margin.right = pixels("data-margin-right").unwrap_or(style.margin.right);
        style.margin_percentage.left =
            percentage("data-margin-left-percent").unwrap_or(style.margin_percentage.left);
        style.margin_percentage.right =
            percentage("data-margin-right-percent").unwrap_or(style.margin_percentage.right);
        style.padding.left = pixels("data-padding-left").unwrap_or(style.padding.left);
        style.padding.right = pixels("data-padding-right").unwrap_or(style.padding.right);
        style.padding_percentage.left =
            percentage("data-padding-left-percent").unwrap_or(style.padding_percentage.left);
        style.padding_percentage.right =
            percentage("data-padding-right-percent").unwrap_or(style.padding_percentage.right);
        style.border.left = pixels("data-border-left").unwrap_or(style.border.left);
        style.border.right = pixels("data-border-right").unwrap_or(style.border.right);
        if attribute("data-auto-margin").is_some() {
            style.automatic_margin.left = true;
            style.automatic_margin.right = true;
        }
        style.flex.direction = match attribute("data-flex-direction") {
            Some("row") | None => FlexDirection::Row,
            Some("column") => FlexDirection::Column,
            Some(value) => panic!("invalid data-flex-direction test value {value:?}"),
        };
        style.flex.wrap = match attribute("data-flex-wrap") {
            Some("wrap") => FlexWrap::Wrap,
            Some("nowrap") | None => FlexWrap::NoWrap,
            Some(value) => panic!("invalid data-flex-wrap test value {value:?}"),
        };
        if let Some(gap) = pixels("data-column-gap") {
            style.flex.column_gap = LengthPercentage::length(gap);
        }
        if let Some(gap) = pixels("data-row-gap") {
            style.flex.row_gap = LengthPercentage::length(gap);
        }
        style
    }
}

fn shrink_to_fit_layout(
    document: &Document,
    viewport: Viewport,
) -> wild_buzzard_layout::LayoutOutput {
    layout_document(
        &document.snapshot().unwrap(),
        viewport,
        &ShrinkToFitStyles,
        &MonospaceTextMeasurer,
    )
    .unwrap()
}

fn node_fragment_width(
    output: &wild_buzzard_layout::LayoutOutput,
    document: &Document,
    id: &str,
) -> Au {
    output
        .boxes_for_node(node_with_id(document, id))
        .next()
        .unwrap_or_else(|| panic!("missing layout box for #{id}"))
        .fragments[0]
        .rect
        .size
        .width
}

#[test]
fn auto_inline_block_applies_the_css2_shrink_to_fit_formula() {
    for (available, expected) in [(24, 32), (48, 48), (80, 72), (100, 72)] {
        let document = parsed(&format!(
            "<div data-line-width={available}><span id=atom data-stf>aaaa bbbb</span></div>"
        ));
        let output = shrink_to_fit_layout(&document, Viewport::from_css_pixels(1366, 768));
        assert_eq!(
            node_fragment_width(&output, &document, "atom"),
            Au::from_px(expected),
            "available content width {available}px"
        );
    }
}

#[test]
fn direct_inline_root_is_measured_once_for_text_atomic_and_forced_break_content() {
    let document = parsed(
        "<div data-line-width=200><span id=outer data-stf>aa<span id=inner data-stf data-width=16></span><br>bbb</span></div>",
    );
    let output = shrink_to_fit_layout(&document, Viewport::from_css_pixels(1366, 768));

    // The first forced line is 16px of text plus the 16px atom; the second is
    // 24px. Replaying the pair-producing walker would incorrectly join the
    // trailing 24px line to the repeated 32px first line before the next br.
    assert_eq!(
        node_fragment_width(&output, &document, "outer"),
        Au::from_px(32)
    );
    assert_eq!(
        node_fragment_width(&output, &document, "inner"),
        Au::from_px(16)
    );
}

#[test]
fn intrinsic_text_contributions_distinguish_normal_nowrap_pre_and_forced_lines() {
    let normal = parsed(
        "<div data-line-width=48><span id=atom data-stf>  aa   <span>bbbb</span> c  </span></div>",
    );
    let nowrap =
        parsed("<div data-line-width=32><span id=atom data-stf data-nowrap>aa bbbb</span></div>");
    let pre =
        parsed("<div data-line-width=200><span id=atom data-stf data-pre>aa  \n b</span></div>");
    let forced =
        parsed("<div data-line-width=200><span id=atom data-stf>aa<br>bbbbbb</span></div>");

    let normal_output = shrink_to_fit_layout(&normal, Viewport::from_css_pixels(1366, 768));
    let nowrap_output = shrink_to_fit_layout(&nowrap, Viewport::from_css_pixels(1366, 768));
    let pre_output = shrink_to_fit_layout(&pre, Viewport::from_css_pixels(1366, 768));
    let forced_output = shrink_to_fit_layout(&forced, Viewport::from_css_pixels(1366, 768));
    assert_eq!(
        node_fragment_width(&normal_output, &normal, "atom"),
        Au::from_px(48)
    );
    assert_eq!(
        node_fragment_width(&nowrap_output, &nowrap, "atom"),
        Au::from_px(56)
    );
    assert_eq!(
        node_fragment_width(&pre_output, &pre, "atom"),
        Au::from_px(32)
    );
    assert_eq!(
        node_fragment_width(&forced_output, &forced, "atom"),
        Au::from_px(48)
    );
}

#[test]
fn atomic_descendants_are_soft_boundaries_only_in_normal_inline_contexts() {
    let normal = parsed(
        "<div data-line-width=40><span id=outer data-stf><span data-width=24 data-display=inline-block></span><span data-width=32 data-display=inline-block></span></span></div>",
    );
    let nowrap = parsed(
        "<div data-line-width=40><span id=outer data-stf data-nowrap><span data-width=24 data-display=inline-block></span><span data-width=32 data-display=inline-block></span></span></div>",
    );
    let normal_output = shrink_to_fit_layout(&normal, Viewport::from_css_pixels(1366, 768));
    let nowrap_output = shrink_to_fit_layout(&nowrap, Viewport::from_css_pixels(1366, 768));

    assert_eq!(
        node_fragment_width(&normal_output, &normal, "outer"),
        Au::from_px(40)
    );
    assert_eq!(
        node_fragment_width(&nowrap_output, &nowrap, "outer"),
        Au::from_px(56)
    );
}

#[test]
fn nested_auto_inline_blocks_reuse_the_pair_but_resolve_each_available_width() {
    for (available, expected) in [(24, 32), (48, 48)] {
        let document = parsed(&format!(
            "<div data-line-width={available}><span id=outer data-stf><span id=inner data-stf>aaaa bbbb</span></span></div>"
        ));
        let output = shrink_to_fit_layout(&document, Viewport::from_css_pixels(1920, 1080));
        assert_eq!(
            node_fragment_width(&output, &document, "outer"),
            Au::from_px(expected)
        );
        assert_eq!(
            node_fragment_width(&output, &document, "inner"),
            Au::from_px(expected)
        );
    }
}

#[test]
fn block_and_supported_flex_descendants_have_bounded_intrinsic_contributions() {
    let blocks = parsed(
        "<div data-line-width=100><span id=outer data-stf><div data-width=20></div><div data-width=40></div></span></div>",
    );
    let row = parsed(
        "<div data-line-width=100><span id=outer data-stf><div data-display=flex data-column-gap=5><div data-width=20></div><div data-width=30></div></div></span></div>",
    );
    let wrapped_row = parsed(
        "<div data-line-width=40><span id=outer data-stf><div data-display=flex data-flex-wrap=wrap data-column-gap=5><div data-width=20></div><div data-width=30></div></div></span></div>",
    );
    let column = parsed(
        "<div data-line-width=100><span id=outer data-stf><div data-display=flex data-flex-direction=column data-row-gap=9><div data-width=20></div><div data-width=30></div></div></span></div>",
    );

    for (document, expected) in [(blocks, 40), (row, 55), (wrapped_row, 40), (column, 30)] {
        let output = shrink_to_fit_layout(&document, Viewport::from_css_pixels(1366, 768));
        assert_eq!(
            node_fragment_width(&output, &document, "outer"),
            Au::from_px(expected)
        );
    }
}

#[test]
fn subject_edges_use_the_real_containing_width_and_auto_margins_use_zero() {
    let percentages = parsed(
        "<div data-line-width=100><span id=atom data-stf data-margin-left-percent=10 data-margin-right-percent=10 data-padding-left-percent=10 data-padding-right-percent=10 data-border-left=2 data-border-right=2>aaaa bbbb</span></div>",
    );
    let automatic = parsed(
        "<div data-line-width=60><span id=atom data-stf data-margin-left=99 data-margin-right=99 data-auto-margin>aaaa bbbb</span></div>",
    );
    let percentage_output =
        shrink_to_fit_layout(&percentages, Viewport::from_css_pixels(1366, 768));
    let automatic_output = shrink_to_fit_layout(&automatic, Viewport::from_css_pixels(1366, 768));

    // 100 - 10% margins - 10% padding - 4px border leaves 56px of
    // available content. The published fragment is the 80px border box.
    assert_eq!(
        node_fragment_width(&percentage_output, &percentages, "atom"),
        Au::from_px(80)
    );
    assert_eq!(
        node_fragment_width(&automatic_output, &automatic, "atom"),
        Au::from_px(60)
    );
}

#[test]
fn negative_margins_remain_signed_in_subject_space_and_descendant_contributions() {
    let subject = parsed(
        "<div data-line-width=40><span id=atom data-stf data-margin-left=-10 data-margin-right=-10>aaaa bbbb</span></div>",
    );
    let descendant = parsed(
        "<div data-line-width=100><span id=outer data-stf><div data-margin-left=-10 data-margin-right=-10>aaaa bbbb</div></span></div>",
    );
    let subject_output = shrink_to_fit_layout(&subject, Viewport::from_css_pixels(1366, 768));
    let descendant_output = shrink_to_fit_layout(&descendant, Viewport::from_css_pixels(1366, 768));

    // Negative subject margins enlarge the available content width from 40px
    // to 60px; they are not clamped before the shrink-to-fit formula.
    assert_eq!(
        node_fragment_width(&subject_output, &subject, "atom"),
        Au::from_px(60)
    );
    // The block descendant contributes (32px, 72px) plus -20px of margins,
    // preserving the ordered (12px, 52px) pair.
    assert_eq!(
        node_fragment_width(&descendant_output, &descendant, "outer"),
        Au::from_px(52)
    );
}

#[test]
fn negative_atomic_contributions_survive_inline_and_row_flex_accumulation() {
    let inline = parsed(
        "<div data-line-width=100><span id=outer data-stf><span data-display=inline-block data-width=20 data-margin-right=-40></span>aaaaaa</span></div>",
    );
    let row_flex = parsed(
        "<div data-line-width=100><span id=outer data-stf><div data-display=flex><div data-width=20 data-margin-right=-40></div><div data-width=48></div></div></span></div>",
    );
    let negative_only = parsed(
        "<div data-line-width=100><span id=outer data-stf><span data-display=inline-block data-width=20 data-margin-right=-40></span></span></div>",
    );

    let inline_output = shrink_to_fit_layout(&inline, Viewport::from_css_pixels(1366, 768));
    let flex_output = shrink_to_fit_layout(&row_flex, Viewport::from_css_pixels(1366, 768));
    let negative_output =
        shrink_to_fit_layout(&negative_only, Viewport::from_css_pixels(1366, 768));

    // The first atom contributes -20px. Its optional trailing break is
    // ignored while the current minimum line is negative, so the following
    // 48px word brings both contributions to 28px.
    assert_eq!(
        node_fragment_width(&inline_output, &inline, "outer"),
        Au::from_px(28)
    );
    assert_eq!(
        node_fragment_width(&flex_output, &row_flex, "outer"),
        Au::from_px(28)
    );
    // Signed contribution state is internal; the measured content size
    // published to the subject remains nonnegative.
    assert_eq!(
        node_fragment_width(&negative_output, &negative_only, "outer"),
        Au::ZERO
    );
}

#[test]
fn inline_block_constraints_apply_max_then_min_for_content_and_border_boxes() {
    let content_box = parsed(
        "<div data-line-width=100><span id=atom data-stf data-max-width=50 data-min-width=60>aaaa bbbb</span></div>",
    );
    let border_box = parsed(
        "<div data-line-width=100><span id=atom data-stf data-border-box data-padding-left=8 data-padding-right=8 data-border-left=2 data-border-right=2 data-max-width=50 data-min-width=60>aaaa bbbb</span></div>",
    );
    let percentages = parsed(
        "<div data-line-width=100><span id=atom data-stf data-border-box data-padding-left=8 data-padding-right=8 data-border-left=2 data-border-right=2 data-max-width=half data-min-percent=60>aaaa bbbb</span></div>",
    );
    let content_output = shrink_to_fit_layout(&content_box, Viewport::from_css_pixels(1366, 768));
    let border_output = shrink_to_fit_layout(&border_box, Viewport::from_css_pixels(1366, 768));
    let percentage_output =
        shrink_to_fit_layout(&percentages, Viewport::from_css_pixels(1366, 768));

    assert_eq!(
        node_fragment_width(&content_output, &content_box, "atom"),
        Au::from_px(60)
    );
    assert_eq!(
        node_fragment_width(&border_output, &border_box, "atom"),
        Au::from_px(60)
    );
    assert_eq!(
        node_fragment_width(&percentage_output, &percentages, "atom"),
        Au::from_px(60)
    );
}

#[test]
fn descendant_cyclic_percentages_follow_the_frozen_intrinsic_rules_then_reresolve() {
    for (available, expected_outer, expected_child) in [(100, 28, 52), (16, 16, 40)] {
        let document = parsed(&format!(
            "<div data-line-width={available}><span id=outer data-stf><div id=child data-width=half-plus-10 data-min-width=half-plus-20 data-max-width=half-plus-10 data-padding-left=4 data-padding-right-percent=50>a b</div></span></div>"
        ));
        let output = shrink_to_fit_layout(&document, Viewport::from_css_pixels(1366, 768));

        // Cyclic preferred width/max/min and percentage padding are ignored
        // for intrinsic contribution; the absolute 4px padding remains, so
        // the pair is (12px, 28px). The 16px control distinguishes ignoring
        // the complete min-width calc from retaining its 20px length. Actual
        // layout then re-resolves every percentage against the selected outer
        // width, with the descendant allowed to overflow.
        assert_eq!(
            node_fragment_width(&output, &document, "outer"),
            Au::from_px(expected_outer)
        );
        assert_eq!(
            node_fragment_width(&output, &document, "child"),
            Au::from_px(expected_child)
        );
    }
}

#[test]
fn unsupported_replaced_and_table_contributions_stop_with_the_exact_node() {
    for (source, unsupported_id) in [
        (
            "<div data-line-width=100><span data-stf><img id=unsupported></span></div>",
            "unsupported",
        ),
        (
            "<div data-line-width=100><span data-stf><table id=unsupported></table></span></div>",
            "unsupported",
        ),
        (
            "<div data-line-width=100><span data-stf><wbr id=unsupported></span></div>",
            "unsupported",
        ),
    ] {
        let document = parsed(source);
        let unsupported = node_with_id(&document, unsupported_id);
        assert!(matches!(
            layout_document(
                &document.snapshot().unwrap(),
                Viewport::from_css_pixels(1366, 768),
                &ShrinkToFitStyles,
                &MonospaceTextMeasurer,
            ),
            Err(LayoutError::UnsupportedInlineBlockIntrinsicContribution {
                node_id: Some(node),
            }) if node == unsupported
        ));
    }
}

#[test]
fn intrinsic_inline_edges_match_the_existing_warning_and_are_not_silently_applied() {
    let document = parsed(
        "<div data-line-width=200><span id=outer data-stf><span id=edged data-padding-left=20 data-padding-right-percent=50 data-border-left=3 data-margin-right=7>aaaa</span></span></div>",
    );
    let edged = node_with_id(&document, "edged");
    let output = shrink_to_fit_layout(&document, Viewport::from_css_pixels(1366, 768));

    // The bounded inline formatter deliberately does not apply non-atomic
    // inline edges. Intrinsic sizing mirrors that behavior and the ordinary
    // layout pass keeps publishing its explicit warning.
    assert_eq!(
        node_fragment_width(&output, &document, "outer"),
        Au::from_px(32)
    );
    assert!(output.warnings.iter().any(|warning| {
        warning.node_id == Some(edged)
            && warning.code == wild_buzzard_layout::LayoutWarningCode::InlineEdgesNotApplied
    }));
}

#[test]
fn generic_shrink_to_fit_geometry_is_identical_at_both_desktop_viewports() {
    let document = parsed("<div data-line-width=48><span id=atom data-stf>aaaa bbbb</span></div>");
    for viewport in [
        Viewport::from_css_pixels(1366, 768),
        Viewport::from_css_pixels(1920, 1080),
    ] {
        let output = shrink_to_fit_layout(&document, viewport);
        assert_eq!(
            node_fragment_width(&output, &document, "atom"),
            Au::from_px(48)
        );
    }
}

struct PanicTextMeasurer;

impl TextMeasurer for PanicTextMeasurer {
    fn measure(&self, _text: &str, _style: &ComputedStyle) -> TextMetrics {
        panic!("the work budget must fail before text measurement")
    }
}

#[test]
fn intrinsic_and_layout_work_have_an_exact_shared_budget_boundary() {
    let document = parsed("<div data-line-width=48><span id=atom data-stf>aaaa bbbb</span></div>");
    let snapshot = document.snapshot().unwrap();
    layout_document_with_limits(
        &snapshot,
        Viewport::from_css_pixels(1366, 768),
        &ShrinkToFitStyles,
        &MonospaceTextMeasurer,
        LayoutLimits {
            max_inline_work: 70,
            ..LayoutLimits::default()
        },
    )
    .unwrap();
    assert_eq!(
        layout_document_with_limits(
            &snapshot,
            Viewport::from_css_pixels(1366, 768),
            &ShrinkToFitStyles,
            &MonospaceTextMeasurer,
            LayoutLimits {
                max_inline_work: 69,
                ..LayoutLimits::default()
            },
        ),
        Err(LayoutError::InlineWorkLimitExceeded { limit: 69 })
    );

    assert_eq!(
        layout_document_with_limits(
            &snapshot,
            Viewport::from_css_pixels(1366, 768),
            &ShrinkToFitStyles,
            &PanicTextMeasurer,
            LayoutLimits {
                max_inline_work: 30,
                ..LayoutLimits::default()
            },
        ),
        Err(LayoutError::InlineWorkLimitExceeded { limit: 30 })
    );
}

#[test]
fn negative_shrink_space_selects_the_minimum_and_intrinsic_overflow_fails_checked() {
    let negative = parsed(
        "<div data-line-width=24><span id=atom data-stf data-margin-left=20 data-margin-right=20>aaaa bbbb</span></div>",
    );
    let negative_output = shrink_to_fit_layout(&negative, Viewport::from_css_pixels(1366, 768));
    assert_eq!(
        node_fragment_width(&negative_output, &negative, "atom"),
        Au::from_px(32)
    );

    let overflow = parsed(
        "<div data-line-width=100><span data-stf><span data-intrinsic-overflow></span></span></div>",
    );
    assert_eq!(
        layout_document(
            &overflow.snapshot().unwrap(),
            Viewport::from_css_pixels(1366, 768),
            &ShrinkToFitStyles,
            &MonospaceTextMeasurer,
        ),
        Err(LayoutError::InlineArithmeticOverflow)
    );

    let available_overflow =
        parsed("<div data-line-width=100><span data-available-overflow>aaaa bbbb</span></div>");
    assert_eq!(
        layout_document(
            &available_overflow.snapshot().unwrap(),
            Viewport::from_css_pixels(1366, 768),
            &ShrinkToFitStyles,
            &MonospaceTextMeasurer,
        ),
        Err(LayoutError::InlineArithmeticOverflow)
    );
}

#[test]
fn definite_inline_block_is_atomic_and_uses_an_independent_block_context() {
    let document =
        parsed("<div id=line>A <span id=atom data-edges><div id=child></div></span> Z</div>");
    let output = layout_document(
        &document.snapshot().unwrap(),
        Viewport::from_css_pixels(1366, 768),
        &InlineBlockStyles,
        &MonospaceTextMeasurer,
    )
    .unwrap();

    let atom = output
        .boxes_for_node(node_with_id(&document, "atom"))
        .next()
        .unwrap();
    let child = output
        .boxes_for_node(node_with_id(&document, "child"))
        .next()
        .unwrap();
    assert_eq!(atom.kind, BoxKind::InlineBlock);
    assert_eq!(atom.style.display, Display::InlineBlock);
    assert_eq!(atom.children, vec![child.id]);
    assert_eq!(child.kind, BoxKind::Block);
    assert_eq!(atom.fragments[0].rect.origin.x, Au::from_px(18));
    assert_eq!(atom.fragments[0].rect.origin.y, Au::from_px(4));
    assert_eq!(atom.fragments[0].rect.size.width, Au::from_px(36));
    assert_eq!(atom.fragments[0].rect.size.height, Au::from_px(26));
    assert_eq!(atom.style.background_color, rgba(20, 120, 60, 255));
    assert_eq!(child.fragments[0].rect.origin.x, Au::from_px(21));
    assert_eq!(child.fragments[0].rect.origin.y, Au::from_px(7));
    assert_eq!(child.fragments[0].rect.size.width, Au::from_px(10));
    assert_eq!(child.fragments[0].rect.size.height, Au::from_px(30));
    assert!(
        child.fragments[0].rect.origin.y + child.fragments[0].rect.size.height
            > atom.fragments[0].rect.origin.y + atom.fragments[0].rect.size.height
    );
    assert!(!output.warnings.iter().any(|warning| {
        warning.code == wild_buzzard_layout::LayoutWarningCode::BlockInsideInlineTreatedAsInline
            && warning.node_id == Some(child.node_id.unwrap())
    }));
}

#[test]
fn inline_block_wraps_only_at_a_soft_boundary_and_br_remains_forced() {
    let normal = parsed(
        "<div id=line data-width=48><span id=prefix>A </span><span id=atom data-width=40></span></div>",
    );
    let nowrap = parsed(
        "<div id=line data-width=48><span id=prefix data-nowrap>A </span><span id=atom data-width=40></span></div>",
    );
    let forced = parsed(
        "<div id=line data-width=48><span id=prefix>A</span><br><span id=atom data-width=40></span></div>",
    );
    let adjacent = parsed(
        "<div id=line data-width=40><span id=prefix>A</span><span id=atom data-width=40></span>B</div>",
    );
    let layout = |document: &Document| {
        layout_document(
            &document.snapshot().unwrap(),
            Viewport::from_css_pixels(1920, 1080),
            &InlineBlockStyles,
            &MonospaceTextMeasurer,
        )
        .unwrap()
    };
    let normal_layout = layout(&normal);
    let nowrap_layout = layout(&nowrap);
    let forced_layout = layout(&forced);
    let adjacent_layout = layout(&adjacent);
    let rect = |output: &wild_buzzard_layout::LayoutOutput, document: &Document| {
        output
            .boxes_for_node(node_with_id(document, "atom"))
            .next()
            .unwrap()
            .fragments[0]
            .rect
    };

    assert_eq!(rect(&normal_layout, &normal).origin.x, Au::ZERO);
    assert_eq!(rect(&normal_layout, &normal).origin.y, Au::from_px(10));
    assert_eq!(rect(&nowrap_layout, &nowrap).origin.x, Au::from_px(16));
    assert_eq!(rect(&nowrap_layout, &nowrap).origin.y, Au::ZERO);
    assert_eq!(rect(&forced_layout, &forced).origin.x, Au::ZERO);
    assert_eq!(rect(&forced_layout, &forced).origin.y, Au::from_px(10));
    assert_eq!(rect(&adjacent_layout, &adjacent).origin.x, Au::ZERO);
    assert_eq!(rect(&adjacent_layout, &adjacent).origin.y, Au::from_px(10));
    let trailing = adjacent_layout
        .boxes
        .iter()
        .flat_map(|layout_box| &layout_box.fragments)
        .find(|fragment| fragment.text.as_deref() == Some("B"))
        .unwrap();
    assert_eq!(trailing.rect.origin.x, Au::ZERO);
    assert_eq!(trailing.rect.origin.y, Au::from_px(30));
}

#[test]
fn atomic_boundaries_use_the_nearest_common_normal_ancestor() {
    let atom_to_atom = parsed(
        "<div id=line data-width=40><span data-pre><span id=atom-a data-width=40 data-pre></span></span><span data-pre><span id=atom-b data-width=40 data-pre></span></span></div>",
    );
    let text_to_atom = parsed(
        "<div id=line data-width=40><span data-pre>A</span><span data-pre><span id=atom data-width=40 data-pre></span></span></div>",
    );
    let atom_to_text = parsed(
        "<div id=line data-width=40><span data-pre><span id=atom data-width=40 data-pre></span></span><span data-pre>B</span></div>",
    );
    let layout = |document: &Document| {
        layout_document(
            &document.snapshot().unwrap(),
            Viewport::from_css_pixels(1366, 768),
            &InlineBlockStyles,
            &MonospaceTextMeasurer,
        )
        .unwrap()
    };
    let atom_rect = |output: &wild_buzzard_layout::LayoutOutput, document: &Document, id: &str| {
        output
            .boxes_for_node(node_with_id(document, id))
            .next()
            .unwrap()
            .fragments[0]
            .rect
    };
    let text_rect = |output: &wild_buzzard_layout::LayoutOutput, text: &str| {
        output
            .boxes
            .iter()
            .flat_map(|layout_box| &layout_box.fragments)
            .find(|fragment| fragment.text.as_deref() == Some(text))
            .unwrap()
            .rect
    };

    let atom_to_atom_layout = layout(&atom_to_atom);
    assert_eq!(
        atom_rect(&atom_to_atom_layout, &atom_to_atom, "atom-a").origin,
        point(Au::ZERO, Au::ZERO)
    );
    assert_eq!(
        atom_rect(&atom_to_atom_layout, &atom_to_atom, "atom-b").origin,
        point(Au::ZERO, Au::from_px(20))
    );

    let text_to_atom_layout = layout(&text_to_atom);
    assert_eq!(
        text_rect(&text_to_atom_layout, "A").origin,
        point(Au::ZERO, Au::ZERO)
    );
    assert_eq!(
        atom_rect(&text_to_atom_layout, &text_to_atom, "atom").origin,
        point(Au::ZERO, Au::from_px(10))
    );

    let atom_to_text_layout = layout(&atom_to_text);
    assert_eq!(
        atom_rect(&atom_to_text_layout, &atom_to_text, "atom").origin,
        point(Au::ZERO, Au::ZERO)
    );
    assert_eq!(
        text_rect(&atom_to_text_layout, "B").origin,
        point(Au::ZERO, Au::from_px(20))
    );
}

#[test]
fn atomic_boundaries_use_the_nearest_common_nowrap_ancestor() {
    let atom_to_atom = parsed(
        "<div id=line data-width=40 data-nowrap><span data-normal><span id=atom-a data-width=40 data-normal></span></span><span data-normal><span id=atom-b data-width=40 data-normal></span></span></div>",
    );
    let text_to_atom = parsed(
        "<div id=line data-width=40 data-nowrap><span data-normal>A</span><span data-normal><span id=atom data-width=40 data-normal></span></span></div>",
    );
    let atom_to_text = parsed(
        "<div id=line data-width=40 data-nowrap><span data-normal><span id=atom data-width=40 data-normal></span></span><span data-normal>B</span></div>",
    );
    let layout = |document: &Document| {
        layout_document(
            &document.snapshot().unwrap(),
            Viewport::from_css_pixels(1366, 768),
            &InlineBlockStyles,
            &MonospaceTextMeasurer,
        )
        .unwrap()
    };
    let atom_rect = |output: &wild_buzzard_layout::LayoutOutput, document: &Document, id: &str| {
        output
            .boxes_for_node(node_with_id(document, id))
            .next()
            .unwrap()
            .fragments[0]
            .rect
    };
    let text_rect = |output: &wild_buzzard_layout::LayoutOutput, text: &str| {
        output
            .boxes
            .iter()
            .flat_map(|layout_box| &layout_box.fragments)
            .find(|fragment| fragment.text.as_deref() == Some(text))
            .unwrap()
            .rect
    };

    let atom_to_atom_layout = layout(&atom_to_atom);
    assert_eq!(
        atom_rect(&atom_to_atom_layout, &atom_to_atom, "atom-a").origin,
        point(Au::ZERO, Au::ZERO)
    );
    assert_eq!(
        atom_rect(&atom_to_atom_layout, &atom_to_atom, "atom-b").origin,
        point(Au::from_px(40), Au::ZERO)
    );

    let text_to_atom_layout = layout(&text_to_atom);
    assert_eq!(
        text_rect(&text_to_atom_layout, "A").origin,
        point(Au::ZERO, Au::ZERO)
    );
    assert_eq!(
        atom_rect(&text_to_atom_layout, &text_to_atom, "atom").origin,
        point(Au::from_px(8), Au::ZERO)
    );

    let atom_to_text_layout = layout(&atom_to_text);
    assert_eq!(
        atom_rect(&atom_to_text_layout, &atom_to_text, "atom").origin,
        point(Au::ZERO, Au::ZERO)
    );
    assert_eq!(
        text_rect(&atom_to_text_layout, "B").origin,
        point(Au::from_px(40), Au::ZERO)
    );
}

#[test]
fn empty_inline_block_auto_width_and_hostile_bounds_are_bounded() {
    let auto = parsed("<div id=line><span id=atom data-width=auto></span></div>");
    let auto_layout = layout_document(
        &auto.snapshot().unwrap(),
        Viewport::from_css_pixels(1366, 768),
        &InlineBlockStyles,
        &MonospaceTextMeasurer,
    )
    .unwrap();
    let auto_box = auto_layout
        .boxes_for_node(node_with_id(&auto, "atom"))
        .next()
        .unwrap();
    assert_eq!(auto_box.fragments[0].rect.size.width, Au::ZERO);

    let bounded = parsed("<div id=line><span id=atom data-width=40></span></div>");
    assert_eq!(
        layout_document_with_limits(
            &bounded.snapshot().unwrap(),
            Viewport::from_css_pixels(1366, 768),
            &InlineBlockStyles,
            &MonospaceTextMeasurer,
            LayoutLimits {
                max_boxes: 2,
                ..LayoutLimits::default()
            },
        ),
        Err(LayoutError::BoxLimitExceeded { limit: 2 })
    );
    assert_eq!(
        layout_document_with_limits(
            &bounded.snapshot().unwrap(),
            Viewport::from_css_pixels(1366, 768),
            &InlineBlockStyles,
            &MonospaceTextMeasurer,
            LayoutLimits {
                max_inline_work: 0,
                ..LayoutLimits::default()
            },
        ),
        Err(LayoutError::InlineWorkLimitExceeded { limit: 0 })
    );

    let text_work = parsed("<body>abcd</body>");
    assert_eq!(
        layout_document_with_limits(
            &text_work.snapshot().unwrap(),
            Viewport::from_css_pixels(1366, 768),
            &InitialStyleResolver,
            &MonospaceTextMeasurer,
            LayoutLimits {
                max_inline_work: 4,
                ..LayoutLimits::default()
            },
        ),
        Err(LayoutError::InlineWorkLimitExceeded { limit: 4 })
    );

    let overflow =
        parsed("<div id=line><span id=atom data-width=overflow data-overflow></span></div>");
    assert!(matches!(
        layout_document(
            &overflow.snapshot().unwrap(),
            Viewport::from_css_pixels(1366, 768),
            &InlineBlockStyles,
            &MonospaceTextMeasurer,
        ),
        Err(LayoutError::InlineArithmeticOverflow)
    ));

    let vertical_overflow =
        parsed("<div id=line><span id=atom data-width=40 data-vertical-overflow></span></div>");
    assert!(matches!(
        layout_document(
            &vertical_overflow.snapshot().unwrap(),
            Viewport::from_css_pixels(1366, 768),
            &InlineBlockStyles,
            &MonospaceTextMeasurer,
        ),
        Err(LayoutError::InlineArithmeticOverflow)
    ));
}

#[test]
fn inline_block_triggered_wrap_y_overflow_fails_before_saturation() {
    let document = parsed(
        "<div id=line data-width=40 data-wrap-y-overflow>A<span id=atom data-width=40></span></div>",
    );
    assert_eq!(
        layout_document(
            &document.snapshot().unwrap(),
            Viewport::from_css_pixels(1366, 768),
            &InlineBlockStyles,
            &MonospaceTextMeasurer,
        ),
        Err(LayoutError::InlineArithmeticOverflow)
    );
}

#[test]
fn growing_inline_prefix_work_fails_at_the_exact_next_charged_unit() {
    let document = parsed("<div id=line data-width=40>abcdef</div>");
    let exact = layout_document_with_limits(
        &document.snapshot().unwrap(),
        Viewport::from_css_pixels(1366, 768),
        &InlineBlockStyles,
        &MonospaceTextMeasurer,
        LayoutLimits {
            max_inline_work: 30,
            ..LayoutLimits::default()
        },
    )
    .unwrap();
    let text = exact
        .boxes
        .iter()
        .flat_map(|layout_box| &layout_box.fragments)
        .filter_map(|fragment| fragment.text.as_deref())
        .collect::<Vec<_>>();
    assert_eq!(text, vec!["abcde", "f"]);

    assert_eq!(
        layout_document_with_limits(
            &document.snapshot().unwrap(),
            Viewport::from_css_pixels(1366, 768),
            &InlineBlockStyles,
            &MonospaceTextMeasurer,
            LayoutLimits {
                max_inline_work: 29,
                ..LayoutLimits::default()
            },
        ),
        Err(LayoutError::InlineWorkLimitExceeded { limit: 29 })
    );
}

#[test]
fn inline_fragment_line_comparisons_are_charged_before_quadratic_work() {
    let document = parsed("<div id=line data-width=40><span>abcdef</span></div>");
    layout_document_with_limits(
        &document.snapshot().unwrap(),
        Viewport::from_css_pixels(1366, 768),
        &InlineBlockStyles,
        &MonospaceTextMeasurer,
        LayoutLimits {
            max_inline_work: 36,
            ..LayoutLimits::default()
        },
    )
    .unwrap();

    assert_eq!(
        layout_document_with_limits(
            &document.snapshot().unwrap(),
            Viewport::from_css_pixels(1366, 768),
            &InlineBlockStyles,
            &MonospaceTextMeasurer,
            LayoutLimits {
                max_inline_work: 35,
                ..LayoutLimits::default()
            },
        ),
        Err(LayoutError::InlineWorkLimitExceeded { limit: 35 })
    );
}

#[test]
fn inline_block_horizontal_auto_margins_compute_to_zero_without_block_distribution() {
    let document = parsed("<div id=line>A <span id=atom data-auto-margin></span></div>");
    let output = layout_document(
        &document.snapshot().unwrap(),
        Viewport::from_css_pixels(1366, 768),
        &InlineBlockStyles,
        &MonospaceTextMeasurer,
    )
    .unwrap();
    let atom = output
        .boxes_for_node(node_with_id(&document, "atom"))
        .next()
        .unwrap();
    assert!(atom.style.automatic_margin.left);
    assert!(atom.style.automatic_margin.right);
    assert_eq!(atom.fragments[0].rect.origin.x, Au::from_px(16));
    assert_eq!(atom.fragments[0].rect.size.width, Au::from_px(30));
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

struct NowrapStyles;

impl StyleResolver for NowrapStyles {
    fn resolve(&self, input: StyleInput<'_>) -> ComputedStyle {
        let mut style = InitialStyleResolver.resolve(input);
        style.white_space = WhiteSpace::Nowrap;
        style
    }
}

struct MixedWrapStyles;

impl StyleResolver for MixedWrapStyles {
    fn resolve(&self, input: StyleInput<'_>) -> ComputedStyle {
        let is_body = input.element.name.local_name.as_str() == "body";
        let nowrap = input.element.html_attribute("data-nowrap").is_some();
        let normal = input.element.html_attribute("data-normal").is_some();
        let width = input.element.html_attribute("data-width");
        let mut style = InitialStyleResolver.resolve(input);
        if is_body {
            style.margin = Edges::default();
            style.width = SizeValue::length(match width {
                Some("zero") => Au::ZERO,
                Some("five-ch") => Au::from_px(40),
                _ => Au::from_px(80),
            });
        }
        if nowrap {
            style.white_space = WhiteSpace::Nowrap;
        } else if normal {
            style.white_space = WhiteSpace::Normal;
        }
        style
    }
}

#[test]
fn collapsed_nowrap_spans_overflow_one_soft_line_but_br_forces_a_line() {
    let document = parsed("<body>one \t\n <span> two </span> three<br>four five</body>");
    let output = layout_document(
        &document.snapshot().unwrap(),
        Viewport::from_css_pixels(40, 40),
        &NowrapStyles,
        &MonospaceTextMeasurer,
    )
    .unwrap();
    let fragments = output
        .boxes
        .iter()
        .flat_map(|layout_box| layout_box.fragments.iter())
        .filter(|fragment| fragment.text.is_some())
        .collect::<Vec<_>>();

    assert_eq!(
        fragments
            .iter()
            .map(|fragment| fragment.text.as_deref().unwrap())
            .collect::<Vec<_>>(),
        vec!["one", " two", " three", "four", " five"]
    );
    assert!(fragments[..3].windows(2).all(|pair| {
        pair[0].rect.origin.y == pair[1].rect.origin.y
            && pair[0].rect.right() == pair[1].rect.origin.x
    }));
    assert_eq!(fragments[3].rect.origin.y, fragments[4].rect.origin.y);
    assert!(fragments[3].rect.origin.y > fragments[0].rect.origin.y);

    let body = output
        .boxes_for_node(node(&document, "body"))
        .next()
        .unwrap();
    assert_eq!(fragments[2].rect.right(), Au::from_px(112));
    assert!(fragments[2].rect.right() > body.fragments[0].rect.right());
}

#[test]
fn normal_collapsed_space_before_nowrap_retains_its_boundary_break() {
    for source in [
        "<body><span data-nowrap>12345</span> 67890</body>",
        "<body data-nowrap><span data-normal><span data-nowrap>12345</span> </span>67890</body>",
        "<body data-nowrap><span data-normal><span data-nowrap>12345 </span> </span>67890</body>",
    ] {
        let document = parsed(source);
        let output = layout_document(
            &document.snapshot().unwrap(),
            Viewport::from_css_pixels(200, 80),
            &MixedWrapStyles,
            &MonospaceTextMeasurer,
        )
        .unwrap();
        let fragments = output
            .boxes
            .iter()
            .flat_map(|layout_box| layout_box.fragments.iter())
            .filter(|fragment| fragment.text.is_some())
            .collect::<Vec<_>>();

        assert_eq!(
            fragments
                .iter()
                .map(|fragment| fragment.text.as_deref().unwrap())
                .collect::<Vec<_>>(),
            vec!["12345", "67890"]
        );
        assert!(fragments[1].rect.origin.y > fragments[0].rect.origin.y);
    }
}

#[test]
fn nowrap_owned_collapsed_space_glues_the_following_normal_word_across_spans() {
    for width in ["zero", "five-ch"] {
        for content in [
            "<span>Hello<span data-nowrap> </span>Kitty</span>",
            "<span>Hello</span><span><span data-nowrap> </span><span>Kitty</span></span>",
        ] {
            let document = parsed(&format!("<body data-width={width}>{content}</body>"));
            let output = layout_document(
                &document.snapshot().unwrap(),
                Viewport::from_css_pixels(200, 120),
                &MixedWrapStyles,
                &MonospaceTextMeasurer,
            )
            .unwrap();
            let fragments = output
                .boxes
                .iter()
                .flat_map(|layout_box| layout_box.fragments.iter())
                .filter(|fragment| fragment.text.is_some())
                .collect::<Vec<_>>();

            assert_eq!(
                fragments
                    .iter()
                    .filter_map(|fragment| fragment.text.as_deref())
                    .collect::<String>(),
                "Hello Kitty"
            );
            let kitty = fragments
                .iter()
                .position(|fragment| fragment.text.as_deref() == Some(" Kitty"))
                .unwrap();
            assert!(kitty > 0);
            assert_eq!(
                fragments[kitty - 1].rect.origin.y,
                fragments[kitty].rect.origin.y
            );
        }
    }
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
    assert_eq!(first.document_version, snapshot.version());
    assert!(document.revision() > first.document_version.revision());
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
            LayoutLimits {
                max_tree_depth: 5,
                ..LayoutLimits::default()
            },
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

struct AutomaticMarginStyles;

impl StyleResolver for AutomaticMarginStyles {
    fn resolve(&self, input: StyleInput<'_>) -> ComputedStyle {
        let name = input.element.name.local_name.as_str();
        let id = input.element.html_attribute("id");
        let mut style = InitialStyleResolver.resolve(input);
        if name == "body" {
            style.margin = Edges::default();
        }
        if matches!(
            id,
            Some(
                "both"
                    | "left"
                    | "right"
                    | "constrained"
                    | "over"
                    | "over-left"
                    | "vertical"
                    | "after"
                    | "item"
            )
        ) {
            style.display = Display::Block;
            style.height = SizeValue::length(Au::from_px(10));
        }
        match id {
            Some("both") => {
                style.width = SizeValue::length(Au::from_raw(Au::from_px(80).raw() + 1));
                style.automatic_margin.left = true;
                style.automatic_margin.right = true;
            }
            Some("left") => {
                style.width = SizeValue::length(Au::from_px(80));
                style.margin.right = Au::from_px(10);
                style.automatic_margin.left = true;
            }
            Some("right") => {
                style.width = SizeValue::length(Au::from_px(80));
                style.margin.left = Au::from_px(10);
                style.automatic_margin.right = true;
            }
            Some("constrained") => {
                style.max_width = MaxSizeValue::length(Au::from_px(60));
                style.automatic_margin.left = true;
                style.automatic_margin.right = true;
            }
            Some("over") => {
                style.width = SizeValue::length(Au::from_px(220));
                style.automatic_margin.left = true;
                style.automatic_margin.right = true;
            }
            Some("over-left") => {
                style.width = SizeValue::length(Au::from_px(220));
                style.margin.right = Au::from_px(10);
                style.automatic_margin.left = true;
            }
            Some("vertical") => {
                style.automatic_margin.top = true;
                style.automatic_margin.bottom = true;
            }
            Some("inline") => {
                style.automatic_margin.left = true;
            }
            Some("flex") => {
                style.display = Display::Flex;
                style.width = SizeValue::length(Au::from_px(100));
            }
            Some("item") => {
                style.width = SizeValue::length(Au::from_px(10));
                style.automatic_margin.left = true;
            }
            _ => {}
        }
        style
    }
}

#[test]
fn block_auto_margins_center_absorb_constrain_and_do_not_move_inline_start_negative() {
    let document = parsed(
        "<body><div id=both></div><div id=left></div><div id=right></div>\
         <div id=constrained></div><div id=over></div><div id=over-left></div></body>",
    );
    let output = layout_document(
        &document.snapshot().unwrap(),
        Viewport::from_css_pixels(200, 200),
        &AutomaticMarginStyles,
        &MonospaceTextMeasurer,
    )
    .unwrap();
    let rect = |id: &str| {
        let target = document
            .elements_by_tag_name("div")
            .unwrap()
            .into_iter()
            .find(|node| document.attribute(*node, None, "id").unwrap() == Some(id))
            .unwrap();
        output.boxes_for_node(target).next().unwrap().fragments[0].rect
    };

    assert_eq!(rect("both").origin.x, Au::from_raw(3_599));
    assert_eq!(rect("both").size.width, Au::from_raw(4_801));
    assert_eq!(rect("left").origin.x, Au::from_px(110));
    assert_eq!(rect("right").origin.x, Au::from_px(10));
    assert_eq!(rect("constrained").origin.x, Au::from_px(70));
    assert_eq!(rect("constrained").size.width, Au::from_px(60));
    assert_eq!(rect("over").origin.x, Au::ZERO);
    assert_eq!(rect("over-left").origin.x, Au::ZERO);
}

#[test]
fn vertical_auto_block_margins_have_zero_used_value() {
    let document = parsed("<body><div id=vertical></div><div id=after></div></body>");
    let elements = document.elements_by_tag_name("div").unwrap();
    let output = layout_document(
        &document.snapshot().unwrap(),
        Viewport::from_css_pixels(200, 100),
        &AutomaticMarginStyles,
        &MonospaceTextMeasurer,
    )
    .unwrap();
    let vertical = output.boxes_for_node(elements[0]).next().unwrap();
    let after = output.boxes_for_node(elements[1]).next().unwrap();
    assert!(vertical.style.automatic_margin.top);
    assert!(vertical.style.automatic_margin.bottom);
    assert_eq!(vertical.fragments[0].rect.origin.y, Au::ZERO);
    assert_eq!(after.fragments[0].rect.origin.y, Au::from_px(10));
}

#[test]
fn auto_margins_in_unimplemented_flex_item_and_inline_contexts_fail_typed() {
    let inline_document = parsed("<body><span id=inline>x</span></body>");
    let inline = node(&inline_document, "span");
    assert!(matches!(
        layout_document(
            &inline_document.snapshot().unwrap(),
            Viewport::from_css_pixels(200, 100),
            &AutomaticMarginStyles,
            &MonospaceTextMeasurer,
        ),
        Err(LayoutError::UnsupportedAutomaticMargin {
            node_id: Some(reported),
            context: AutomaticMarginContext::InlineFormatting,
        }) if reported == inline
    ));

    let flex_document = parsed("<body><div id=flex><div id=item></div></div></body>");
    let item = flex_document.elements_by_tag_name("div").unwrap()[1];
    assert!(matches!(
        layout_document(
            &flex_document.snapshot().unwrap(),
            Viewport::from_css_pixels(200, 100),
            &AutomaticMarginStyles,
            &MonospaceTextMeasurer,
        ),
        Err(LayoutError::UnsupportedAutomaticMargin {
            node_id: Some(reported),
            context: AutomaticMarginContext::FlexItem,
        }) if reported == item
    ));
}

struct DirectionStyles {
    automatic_margin: bool,
}

impl StyleResolver for DirectionStyles {
    fn resolve(&self, input: StyleInput<'_>) -> ComputedStyle {
        let is_target = input.element.html_attribute("id") == Some("target");
        let is_body = input.element.name.local_name == "body";
        let mut style = InitialStyleResolver.resolve(input);
        if is_body {
            style.margin = Edges::default();
        }
        if is_target {
            style.display = Display::Block;
            style.inline_direction = InlineDirection::Rtl;
            style.width = SizeValue::length(Au::from_px(220));
            if self.automatic_margin {
                style.automatic_margin.left = true;
                style.automatic_margin.right = true;
            }
        }
        style
    }
}

fn assert_rtl_block_fails_before_layout(automatic_margin: bool) {
    let document = parsed("<body><div id=target></div></body>");
    let target = node(&document, "div");
    assert_eq!(
        layout_document(
            &document.snapshot().unwrap(),
            Viewport::from_css_pixels(200, 100),
            &DirectionStyles { automatic_margin },
            &MonospaceTextMeasurer,
        ),
        Err(LayoutError::UnsupportedInlineDirection {
            node: target,
            direction: InlineDirection::Rtl,
        })
    );
}

#[test]
fn rtl_block_with_auto_margins_fails_before_fragment_publication() {
    assert_rtl_block_fails_before_layout(true);
}

#[test]
fn rtl_block_without_auto_margins_fails_before_fragment_publication() {
    assert_rtl_block_fails_before_layout(false);
}

#[test]
fn inherited_style_contract_preserves_direction_for_anonymous_boxes() {
    let parent = ComputedStyle {
        inline_direction: InlineDirection::Rtl,
        ..ComputedStyle::default()
    };
    let inherited = ComputedStyle::inherit_from(Some(&parent));
    assert_eq!(inherited.inline_direction, InlineDirection::Rtl);
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

struct BoundedFlexStyles;

impl StyleResolver for BoundedFlexStyles {
    fn resolve(&self, input: StyleInput<'_>) -> ComputedStyle {
        let is_container = input.element.html_attribute("id") == Some("flex");
        let is_item = input.element.html_attribute("data-item").is_some();
        let mut style = InitialStyleResolver.resolve(input);
        if is_container {
            style.display = Display::Flex;
            style.width = SizeValue::length(Au::from_px(50));
            style.flex.wrap = FlexWrap::Wrap;
        } else if is_item {
            style.height = SizeValue::length(Au::from_px(10));
            style.flex.basis = FlexBasis::LengthPercentage(
                wild_buzzard_layout::LengthPercentage::length(Au::from_px(40)),
            );
            style.flex.shrink = FlexFactor::default();
        }
        style
    }
}

#[test]
fn flex_item_line_and_work_limits_fail_with_typed_errors() {
    let document = parsed(
        "<div id=flex><div data-item></div><div data-item></div><div data-item></div></div>",
    );
    let snapshot = document.snapshot().unwrap();
    assert!(matches!(
        layout_document_with_limits(
            &snapshot,
            Viewport::from_css_pixels(200, 200),
            &BoundedFlexStyles,
            &MonospaceTextMeasurer,
            LayoutLimits {
                max_flex_items: 2,
                ..LayoutLimits::default()
            },
        ),
        Err(LayoutError::FlexItemLimitExceeded {
            limit: 2,
            actual: 3,
        })
    ));
    assert!(matches!(
        layout_document_with_limits(
            &snapshot,
            Viewport::from_css_pixels(200, 200),
            &BoundedFlexStyles,
            &MonospaceTextMeasurer,
            LayoutLimits {
                max_flex_lines: 1,
                ..LayoutLimits::default()
            },
        ),
        Err(LayoutError::FlexLineLimitExceeded { limit: 1 })
    ));
    assert!(matches!(
        layout_document_with_limits(
            &snapshot,
            Viewport::from_css_pixels(200, 200),
            &BoundedFlexStyles,
            &MonospaceTextMeasurer,
            LayoutLimits {
                max_flex_work: 2,
                ..LayoutLimits::default()
            },
        ),
        Err(LayoutError::FlexWorkLimitExceeded { limit: 2 })
    ));
}

#[test]
fn flex_blockifies_element_items_but_drops_whitespace_only_anonymous_items() {
    let document =
        parsed("<div id=flex>\n  <span data-item>A</span>\n  <span data-item>B</span>\n</div>");
    let flex = document.elements_by_tag_name("div").unwrap()[0];
    let spans = document.elements_by_tag_name("span").unwrap();
    let output = layout_document(
        &document.snapshot().unwrap(),
        Viewport::from_css_pixels(200, 200),
        &BoundedFlexStyles,
        &MonospaceTextMeasurer,
    )
    .unwrap();
    let flex_box = output.boxes_for_node(flex).next().unwrap();
    assert_eq!(flex_box.kind, BoxKind::Flex);
    assert_eq!(flex_box.children.len(), 2);
    assert_eq!(
        flex_box
            .children
            .iter()
            .map(|child| output.box_by_id(*child).unwrap().node_id)
            .collect::<Vec<_>>(),
        vec![Some(spans[0]), Some(spans[1])]
    );
    assert!(
        flex_box
            .children
            .iter()
            .all(|child| { output.box_by_id(*child).unwrap().kind == BoxKind::Block })
    );
}
