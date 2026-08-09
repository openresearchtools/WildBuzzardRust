use wild_buzzard_dom::{
    AttributeName, Document, DocumentSnapshot, Namespace, NodeId, NodeKind, QualifiedName,
};
use wild_buzzard_html::parse_document;
use wild_buzzard_layout::{
    layout_document_with_style_snapshot, Au, BoxSizing, Color, Display, LayoutError,
    LengthPercentage, MaxSizeValue, MonospaceTextMeasurer, SizeValue, Viewport, WritingMode,
};
use wild_buzzard_stylo_adapter::{
    prepare_computed_styles, prepare_computed_styles_with_states, ElementSelectorState,
    SelectorState, SelectorStateSnapshot, SelectorStateSnapshotError, StaticStyleOptions,
    StyleAdapterError, UnsupportedComputedValue,
};

fn node_with_id(snapshot: &DocumentSnapshot, id: &str) -> NodeId {
    snapshot
        .nodes_in_document_order()
        .iter()
        .find_map(|node| match &node.kind {
            NodeKind::Element(element) if element.html_attribute("id") == Some(id) => Some(node.id),
            _ => None,
        })
        .unwrap_or_else(|| panic!("missing test element #{id}"))
}

#[test]
fn real_stylo_matches_tag_id_class_descendant_and_child_selectors() {
    let parsed = parse_document(
        r"<style>
            p { margin-left: 1px; }
            #target { margin-right: 2px; }
            .note { padding-left: 3px; }
            section p { margin-top: 4px; }
            section > p { margin-bottom: 5px; }
        </style><section><p id=target class=note>text</p></section>",
    )
    .unwrap();
    let snapshot = parsed.document.snapshot().unwrap();
    let target = node_with_id(&snapshot, "target");
    let result = prepare_computed_styles(snapshot, StaticStyleOptions::default()).unwrap();
    let style = result.layout_styles().get(target).unwrap();
    assert_eq!(style.margin.left, Au::from_px(1));
    assert_eq!(style.margin.right, Au::from_px(2));
    assert_eq!(style.margin.top, Au::from_px(4));
    assert_eq!(style.margin.bottom, Au::from_px(5));
    assert_eq!(style.padding.left, Au::from_px(3));
}

#[test]
fn real_stylo_cascade_handles_inline_important_and_inheritance() {
    let parsed = parse_document(
        r#"<style>
            #winner { color: rgb(255 0 0) !important; }
            .ordinary { color: rgb(0 0 255); }
        </style>
        <div id=parent style="color: rgb(10 20 30)">
          <span id=inherited></span>
          <span id=winner class=ordinary style="color: rgb(0 255 0)"></span>
          <span id=inline-important class=ordinary
                style="color: rgb(0 255 0) !important"></span>
        </div>"#,
    )
    .unwrap();
    let snapshot = parsed.document.snapshot().unwrap();
    let inherited = node_with_id(&snapshot, "inherited");
    let winner = node_with_id(&snapshot, "winner");
    let inline_important = node_with_id(&snapshot, "inline-important");
    let result = prepare_computed_styles(snapshot, StaticStyleOptions::default()).unwrap();
    assert_eq!(
        result.layout_styles().get(inherited).unwrap().color,
        Color {
            red: 10,
            green: 20,
            blue: 30,
            alpha: 255,
        }
    );
    assert_eq!(
        result.layout_styles().get(winner).unwrap().color,
        Color {
            red: 255,
            green: 0,
            blue: 0,
            alpha: 255,
        }
    );
    assert_eq!(
        result.layout_styles().get(inline_important).unwrap().color,
        Color {
            red: 0,
            green: 255,
            blue: 0,
            alpha: 255,
        }
    );
}

#[test]
fn ua_and_author_display_none_use_stylo_computed_display() {
    let parsed = parse_document(
        "<style>#gone { display: none }</style><div id=gone>gone</div><div id=shown>shown</div>",
    )
    .unwrap();
    let snapshot = parsed.document.snapshot().unwrap();
    let gone = node_with_id(&snapshot, "gone");
    let style_element = snapshot
        .nodes_in_document_order()
        .iter()
        .find(|node| {
            matches!(&node.kind, NodeKind::Element(element) if element.name.local_name == "style")
        })
        .unwrap()
        .id;
    let result = prepare_computed_styles(snapshot, StaticStyleOptions::default()).unwrap();
    assert_eq!(
        result.layout_styles().get(gone).unwrap().display,
        Display::None
    );
    assert_eq!(
        result.layout_styles().get(style_element).unwrap().display,
        Display::None
    );
}

#[test]
fn percentages_remain_typed_and_layout_uses_containing_inline_size_for_all_edges() {
    let parsed = parse_document(
        r"<style>
          #target {
            display: block;
            margin-top: 10%; margin-bottom: 20%;
            padding-top: 5%; padding-bottom: 10%;
            border: 20px none;
          }
        </style><body><div id=target></div></body>",
    )
    .unwrap();
    let snapshot = parsed.document.snapshot().unwrap();
    let target = node_with_id(&snapshot, "target");
    let result = prepare_computed_styles(snapshot.clone(), StaticStyleOptions::default()).unwrap();
    let style = result.layout_styles().get(target).unwrap();
    assert_eq!(style.margin_percentage.top, 100_000);
    assert_eq!(style.margin_percentage.bottom, 200_000);
    assert_eq!(style.padding_percentage.top, 50_000);
    assert_eq!(style.padding_percentage.bottom, 100_000);
    assert_eq!(style.border.top, Au::ZERO);

    let layout = layout_document_with_style_snapshot(
        &snapshot,
        Viewport::from_css_pixels(200, 200),
        result.layout_styles(),
        &MonospaceTextMeasurer,
    )
    .unwrap();
    let target_box = layout.boxes_for_node(target).next().unwrap();
    let fragment = &target_box.fragments[0];
    // Body content width is 184px after its temporary-UA 8px margins. CSS
    // top/bottom percentages use that inline width too.
    assert_eq!(fragment.rect.origin.y, Au::from_raw(1_584));
    assert_eq!(fragment.rect.size.height, Au::from_raw(1_656));
}

#[test]
fn computed_font_line_height_background_border_and_white_space_are_projected() {
    let parsed = parse_document(
        r"<style>
          #normal {
            display: block; font-size: 20px; line-height: normal;
            color: rgb(1 2 3); background-color: rgb(4 5 6 / 50%);
            border: 2px solid; white-space: pre;
          }
          #number { display: block; font-size: 20px; line-height: 1.5; }
        </style><div id=normal></div><div id=number></div>",
    )
    .unwrap();
    let snapshot = parsed.document.snapshot().unwrap();
    let normal = node_with_id(&snapshot, "normal");
    let number = node_with_id(&snapshot, "number");
    let result = prepare_computed_styles(snapshot, StaticStyleOptions::default()).unwrap();
    let normal = result.layout_styles().get(normal).unwrap();
    assert_eq!(normal.font_size, Au::from_px(20));
    assert_eq!(normal.line_height, Au::from_px(24));
    assert_eq!(
        normal.border,
        wild_buzzard_layout::Edges::all(Au::from_px(2))
    );
    assert_eq!(normal.white_space, wild_buzzard_layout::WhiteSpace::Pre);
    assert_eq!(
        normal.background_color,
        Color {
            red: 4,
            green: 5,
            blue: 6,
            alpha: 128,
        }
    );
    assert_eq!(
        result.layout_styles().get(number).unwrap().line_height,
        Au::from_px(30)
    );
}

#[test]
fn malformed_css_recovers_but_imports_and_resource_overflow_fail_closed() {
    let parsed =
        parse_document("<style>p { color: ; margin-left: 7px }</style><p id=target>x</p>").unwrap();
    let snapshot = parsed.document.snapshot().unwrap();
    let target = node_with_id(&snapshot, "target");
    let result = prepare_computed_styles(snapshot, StaticStyleOptions::default()).unwrap();
    assert!(!result.diagnostics().is_empty());
    assert_eq!(result.diagnostics()[0].line, 1);
    assert!(result.diagnostics()[0].column > 0);
    assert_eq!(
        result.layout_styles().get(target).unwrap().margin.left,
        Au::from_px(7)
    );

    let imported = parse_document("<style>@import url(https://example.invalid/x.css);</style>")
        .unwrap()
        .document
        .snapshot()
        .unwrap();
    assert!(matches!(
        prepare_computed_styles(imported, StaticStyleOptions::default()),
        Err(StyleAdapterError::ImportRuleProhibited { .. })
    ));

    let oversized = parse_document("<style>p { color: red }</style><p>x</p>")
        .unwrap()
        .document
        .snapshot()
        .unwrap();
    let mut options = StaticStyleOptions::default();
    options.limits.max_stylesheet_bytes = 2;
    assert!(matches!(
        prepare_computed_styles(oversized, options),
        Err(StyleAdapterError::StylesheetByteLimitExceeded { .. })
    ));
}

#[test]
fn style_type_media_cssom_disabled_gap_and_shadow_only_selectors_are_fail_closed() {
    let parsed = parse_document(
        r#"<style type=text/plain>#target { margin-left: 90px }</style>
        <style type=" text/css ">#target { padding-top: 90px }</style>
        <style type="text/css; charset=utf-8">#target { padding-bottom: 90px }</style>
        <style type="TEXT/CSS">#target { margin-right: 7px }</style>
        <style disabled>#target { padding-left: 91px }</style>
        <style media=print>#target { margin-left: 92px }</style>
        <style>
          #target { margin-left: 1px }
          #target:hover { margin-left: 93px }
          #target:host { margin-left: 94px }
          #target:defined { margin-left: 95px }
        </style><p id=target>x</p>"#,
    )
    .unwrap();
    let snapshot = parsed.document.snapshot().unwrap();
    let target = node_with_id(&snapshot, "target");
    let result = prepare_computed_styles(snapshot, StaticStyleOptions::default()).unwrap();
    assert_eq!(
        result.layout_styles().get(target).unwrap().margin.left,
        Au::from_px(1)
    );
    // A literal disabled attribute does not represent the non-reflecting
    // CSSOM `HTMLStyleElement.disabled` state, so the static sheet stays active.
    assert_eq!(
        result.layout_styles().get(target).unwrap().padding.left,
        Au::from_px(91)
    );
    assert_eq!(
        result.layout_styles().get(target).unwrap().padding.top,
        Au::ZERO
    );
    assert_eq!(
        result.layout_styles().get(target).unwrap().padding.bottom,
        Au::ZERO
    );
    assert_eq!(
        result.layout_styles().get(target).unwrap().margin.right,
        Au::from_px(7)
    );
    assert!(!result.diagnostics().is_empty());
}

#[test]
fn style_sheet_text_uses_descendant_text_content_for_dom_created_trees() {
    let mut document = Document::new();
    let html = document.create_html_element("html").unwrap();
    let head = document.create_html_element("head").unwrap();
    let body = document.create_html_element("body").unwrap();
    let style = document.create_html_element("style").unwrap();
    let nested = document.create_html_element("span").unwrap();
    let css = document
        .create_text("#target { margin-left: 13px }")
        .unwrap();
    let target = document.create_html_element("p").unwrap();
    document
        .set_attribute(target, AttributeName::html("id"), "target")
        .unwrap();
    document
        .append_child(document.document_node(), html)
        .unwrap();
    document.append_child(html, head).unwrap();
    document.append_child(html, body).unwrap();
    document.append_child(head, style).unwrap();
    document.append_child(style, nested).unwrap();
    document.append_child(nested, css).unwrap();
    document.append_child(body, target).unwrap();

    let result =
        prepare_computed_styles(document.snapshot().unwrap(), StaticStyleOptions::default())
            .unwrap();
    assert_eq!(
        result.layout_styles().get(target).unwrap().margin.left,
        Au::from_px(13)
    );
}

#[test]
fn diagnostic_retention_cap_reports_exact_dropped_count() {
    let snapshot =
        parse_document("<style>p { color: ; width: ; height: ; margin-left: 1px }</style><p>x</p>")
            .unwrap()
            .document
            .snapshot()
            .unwrap();
    let uncapped =
        prepare_computed_styles(snapshot.clone(), StaticStyleOptions::default()).unwrap();
    let total = uncapped
        .diagnostics()
        .len()
        .checked_add(uncapped.dropped_diagnostic_count())
        .unwrap();
    assert!(total >= 3);

    let mut zero_options = StaticStyleOptions::default();
    zero_options.limits.max_diagnostics = 0;
    let zero = prepare_computed_styles(snapshot.clone(), zero_options).unwrap();
    assert!(zero.diagnostics().is_empty());
    assert_eq!(zero.dropped_diagnostic_count(), total);

    let mut one_options = StaticStyleOptions::default();
    one_options.limits.max_diagnostics = 1;
    let one = prepare_computed_styles(snapshot, one_options).unwrap();
    assert_eq!(one.diagnostics().len(), 1);
    assert_eq!(one.dropped_diagnostic_count(), total - 1);
}

#[test]
fn revision_and_document_identity_are_checked_before_layout() {
    let mut parsed = parse_document("<p id=target>x</p>").unwrap();
    let original = parsed.document.snapshot().unwrap();
    let target = node_with_id(&original, "target");
    let result = prepare_computed_styles(original.clone(), StaticStyleOptions::default()).unwrap();
    parsed
        .document
        .set_attribute(target, AttributeName::html("class"), "changed")
        .unwrap();
    let newer = parsed.document.snapshot().unwrap();
    assert!(matches!(
        layout_document_with_style_snapshot(
            &newer,
            Viewport::from_css_pixels(200, 200),
            result.layout_styles(),
            &MonospaceTextMeasurer,
        ),
        Err(LayoutError::StyleRevisionMismatch { .. })
    ));

    let other = parse_document("<p>other</p>")
        .unwrap()
        .document
        .snapshot()
        .unwrap();
    assert!(matches!(
        layout_document_with_style_snapshot(
            &other,
            Viewport::from_css_pixels(200, 200),
            result.layout_styles(),
            &MonospaceTextMeasurer,
        ),
        Err(LayoutError::StyleDocumentMismatch { .. })
    ));
}

#[test]
fn copied_dom_inputs_are_bounded_before_atomization() {
    let snapshot = parse_document("<p id=long-identifier>x</p>")
        .unwrap()
        .document
        .snapshot()
        .unwrap();
    let mut options = StaticStyleOptions::default();
    options.limits.max_identifier_bytes = 3;
    assert!(matches!(
        prepare_computed_styles(snapshot, options),
        Err(StyleAdapterError::SnapshotResourceLimitExceeded {
            resource: "id bytes",
            ..
        })
    ));
}

#[test]
fn root_style_completion_updates_rem_basis_before_descendants() {
    let parsed = parse_document(
        r"<style>
          html { font-size: 20px; margin-left: 2rem }
          #target { display: block; font-size: 2rem; width: 3rem }
        </style><div id=target></div>",
    )
    .unwrap();
    let snapshot = parsed.document.snapshot().unwrap();
    let root = snapshot.document_element().unwrap();
    let target = node_with_id(&snapshot, "target");
    let result = prepare_computed_styles(snapshot, StaticStyleOptions::default()).unwrap();

    assert_eq!(
        result.layout_styles().get(root).unwrap().font_size,
        Au::from_px(20)
    );
    assert_eq!(
        result.layout_styles().get(root).unwrap().margin.left,
        Au::from_px(40)
    );
    let target_style = result.layout_styles().get(target).unwrap();
    assert_eq!(target_style.font_size, Au::from_px(40));
    assert_eq!(
        target_style.width,
        SizeValue::LengthPercentage(LengthPercentage::length(Au::from_px(60)))
    );
}

#[test]
fn width_height_min_max_and_box_sizing_reach_layout_used_geometry() {
    let parsed = parse_document(
        r"<style>
          body { margin: 0 }
          #exact { display: block; width: 50px; height: 10px }
          #constrained {
            display: block; width: 50px; min-width: 70px; max-width: 60px;
            height: 10px; min-height: 20px; max-height: 15px;
            padding: 5px; border: 2px solid; box-sizing: content-box;
          }
          #border-box {
            display: block; width: 50px; height: 30px;
            padding: 5px; border: 2px solid; box-sizing: border-box;
          }
          #percentage { display: block; width: 50%; height: 1px }
        </style>
        <body><div id=exact></div><div id=constrained></div>
        <div id=border-box></div><div id=percentage></div></body>",
    )
    .unwrap();
    let snapshot = parsed.document.snapshot().unwrap();
    let exact = node_with_id(&snapshot, "exact");
    let constrained = node_with_id(&snapshot, "constrained");
    let border_box = node_with_id(&snapshot, "border-box");
    let percentage = node_with_id(&snapshot, "percentage");
    let result = prepare_computed_styles(snapshot.clone(), StaticStyleOptions::default()).unwrap();

    let constrained_style = result.layout_styles().get(constrained).unwrap();
    assert_eq!(constrained_style.box_sizing, BoxSizing::ContentBox);
    assert_eq!(
        constrained_style.min_width,
        SizeValue::length(Au::from_px(70))
    );
    assert_eq!(
        constrained_style.max_width,
        MaxSizeValue::length(Au::from_px(60))
    );

    let layout = layout_document_with_style_snapshot(
        &snapshot,
        Viewport::from_css_pixels(200, 200),
        result.layout_styles(),
        &MonospaceTextMeasurer,
    )
    .unwrap();
    let fragment = |node| &layout.boxes_for_node(node).next().unwrap().fragments[0];
    assert_eq!(fragment(exact).rect.size.width, Au::from_px(50));
    assert_eq!(fragment(exact).rect.size.height, Au::from_px(10));
    // min wins over max; content-box adds 5px padding and 2px border per side.
    assert_eq!(fragment(constrained).rect.size.width, Au::from_px(84));
    assert_eq!(fragment(constrained).rect.size.height, Au::from_px(34));
    assert_eq!(fragment(border_box).rect.size.width, Au::from_px(50));
    assert_eq!(fragment(border_box).rect.size.height, Au::from_px(30));
    assert_eq!(fragment(percentage).rect.size.width, Au::from_px(100));
}

#[test]
fn vertical_writing_mode_is_projected_and_layout_fails_closed() {
    let parsed = parse_document(
        "<style>#vertical { display: block; writing-mode: vertical-rl }</style><div id=vertical>x</div>",
    )
    .unwrap();
    let snapshot = parsed.document.snapshot().unwrap();
    let vertical = node_with_id(&snapshot, "vertical");
    let result = prepare_computed_styles(snapshot.clone(), StaticStyleOptions::default()).unwrap();
    assert_eq!(
        result.layout_styles().get(vertical).unwrap().writing_mode,
        WritingMode::VerticalRl
    );
    assert!(matches!(
        layout_document_with_style_snapshot(
            &snapshot,
            Viewport::from_css_pixels(200, 200),
            result.layout_styles(),
            &MonospaceTextMeasurer,
        ),
        Err(LayoutError::UnsupportedWritingMode {
            node,
            writing_mode: WritingMode::VerticalRl,
        }) if node == vertical
    ));
}

#[test]
fn concrete_adapter_covers_structural_attribute_language_and_relative_selectors() {
    let parsed = parse_document(
        r#"<style>
          section[data-kind~="card"] > p:first-child + p.note:nth-child(2) {
            margin-left: 11px;
          }
          section:has(> #target) > #target:is(.note):not(.missing) {
            margin-right: 12px;
          }
          p:first-child ~ #target { margin-top: 13px }
          #target:lang(en) { padding-left: 14px }
          #target:empty { padding-right: 15px }
        </style>
        <section lang=en data-kind="wide card"><p></p><p id=target class=note></p></section>"#,
    )
    .unwrap();
    let snapshot = parsed.document.snapshot().unwrap();
    let target = node_with_id(&snapshot, "target");
    let result = prepare_computed_styles(snapshot, StaticStyleOptions::default()).unwrap();
    let style = result.layout_styles().get(target).unwrap();
    assert_eq!(style.margin.left, Au::from_px(11));
    assert_eq!(style.margin.right, Au::from_px(12));
    assert_eq!(style.margin.top, Au::from_px(13));
    assert_eq!(style.padding.left, Au::from_px(14));
    assert_eq!(style.padding.right, Au::from_px(15));
}

#[test]
fn exact_revision_selector_state_drives_dynamic_pseudo_classes() {
    let mut parsed = parse_document(
        r"<style>
          #target:hover { margin-left: 11px }
          #target:focus-visible { margin-right: 12px }
          #target:enabled { padding-left: 13px }
          #target:checked { padding-right: 14px }
          #target:required { margin-top: 15px }
          #target:focus { margin-bottom: 16px }
          #target:disabled { padding-left: 99px }
        </style><button id=target></button>",
    )
    .unwrap();
    let original = parsed.document.snapshot().unwrap();
    let target = node_with_id(&original, "target");
    let state = ElementSelectorState::empty()
        .with(SelectorState::Hover)
        .with(SelectorState::Focus)
        .with(SelectorState::FocusVisible)
        .with(SelectorState::Enabled)
        .with(SelectorState::Checked)
        .with(SelectorState::Required);
    let states = SelectorStateSnapshot::try_new(&original, [(target, state)]).unwrap();

    let without_state =
        prepare_computed_styles(original.clone(), StaticStyleOptions::default()).unwrap();
    assert_eq!(
        without_state
            .layout_styles()
            .get(target)
            .unwrap()
            .margin
            .left,
        Au::ZERO
    );
    let with_state = prepare_computed_styles_with_states(
        original.clone(),
        StaticStyleOptions::default(),
        &states,
    )
    .unwrap();
    let style = with_state.layout_styles().get(target).unwrap();
    assert_eq!(style.margin.left, Au::from_px(11));
    assert_eq!(style.margin.right, Au::from_px(12));
    assert_eq!(style.padding.left, Au::from_px(13));
    assert_eq!(style.padding.right, Au::from_px(14));
    assert_eq!(style.margin.top, Au::from_px(15));
    assert_eq!(style.margin.bottom, Au::from_px(16));

    assert!(matches!(
        SelectorStateSnapshot::try_new(
            &original,
            [(
                target,
                ElementSelectorState::empty()
                    .with(SelectorState::Enabled)
                    .with(SelectorState::Disabled),
            )],
        ),
        Err(SelectorStateSnapshotError::ConflictingStates { node, .. }) if node == target
    ));

    parsed
        .document
        .set_attribute(target, AttributeName::html("class"), "changed")
        .unwrap();
    let newer = parsed.document.snapshot().unwrap();
    assert!(matches!(
        prepare_computed_styles_with_states(newer, StaticStyleOptions::default(), &states),
        Err(StyleAdapterError::SelectorState(
            SelectorStateSnapshotError::RevisionMismatch { .. }
        ))
    ));
}

#[test]
fn html_and_svg_href_links_match_link_and_any_link_as_unvisited() {
    const XLINK_URI: &str = "http://www.w3.org/1999/xlink";

    let mut document = Document::new();
    let html = document.create_html_element("html").unwrap();
    let head = document.create_html_element("head").unwrap();
    let body = document.create_html_element("body").unwrap();
    let style = document.create_html_element("style").unwrap();
    let css = document
        .create_text(
            r"#html-link:link { margin-left: 11px }
              #svg-link:any-link { margin-left: 12px }
              #svg-xlink:any-link { margin-left: 13px }
              #html-area:any-link { margin-left: 14px }
              #metadata-link:any-link { margin-left: 15px }
              #plain-anchor:any-link { margin-left: 88px }
              :visited { margin-right: 99px }",
        )
        .unwrap();
    let navigation_anchor = document.create_html_element("a").unwrap();
    let image_map_area = document.create_html_element("area").unwrap();
    let metadata_link = document.create_html_element("link").unwrap();
    let plain_anchor = document.create_html_element("a").unwrap();
    let svg = document
        .create_element(QualifiedName::new(Namespace::Svg, None, "svg").unwrap())
        .unwrap();
    let modern_svg_anchor = document
        .create_element(QualifiedName::new(Namespace::Svg, None, "a").unwrap())
        .unwrap();
    let legacy_namespaced_anchor = document
        .create_element(QualifiedName::new(Namespace::Svg, None, "a").unwrap())
        .unwrap();
    for (element, id) in [
        (navigation_anchor, "html-link"),
        (image_map_area, "html-area"),
        (metadata_link, "metadata-link"),
        (plain_anchor, "plain-anchor"),
        (modern_svg_anchor, "svg-link"),
        (legacy_namespaced_anchor, "svg-xlink"),
    ] {
        document
            .set_attribute(element, AttributeName::html("id"), id)
            .unwrap();
    }
    document
        .set_attribute(navigation_anchor, AttributeName::html("href"), "/html")
        .unwrap();
    document
        .set_attribute(image_map_area, AttributeName::html("href"), "/map")
        .unwrap();
    document
        .set_attribute(metadata_link, AttributeName::html("href"), "/metadata")
        .unwrap();
    document
        .set_attribute(modern_svg_anchor, AttributeName::html("href"), "/svg")
        .unwrap();
    document
        .set_attribute(
            legacy_namespaced_anchor,
            AttributeName::new(Some(XLINK_URI.to_owned()), Some("xlink".to_owned()), "href")
                .unwrap(),
            "/legacy-svg",
        )
        .unwrap();
    document
        .append_child(document.document_node(), html)
        .unwrap();
    document.append_child(html, head).unwrap();
    document.append_child(html, body).unwrap();
    document.append_child(head, style).unwrap();
    document.append_child(head, metadata_link).unwrap();
    document.append_child(style, css).unwrap();
    document.append_child(body, navigation_anchor).unwrap();
    document.append_child(body, image_map_area).unwrap();
    document.append_child(body, plain_anchor).unwrap();
    document.append_child(body, svg).unwrap();
    document.append_child(svg, modern_svg_anchor).unwrap();
    document
        .append_child(svg, legacy_namespaced_anchor)
        .unwrap();

    let result =
        prepare_computed_styles(document.snapshot().unwrap(), StaticStyleOptions::default())
            .unwrap();
    for (node, expected) in [
        (navigation_anchor, Au::from_px(11)),
        (modern_svg_anchor, Au::from_px(12)),
        (legacy_namespaced_anchor, Au::from_px(13)),
        (image_map_area, Au::from_px(14)),
        (metadata_link, Au::from_px(15)),
        (plain_anchor, Au::ZERO),
    ] {
        let style = result.layout_styles().get(node).unwrap();
        assert_eq!(style.margin.left, expected);
        assert_eq!(style.margin.right, Au::ZERO);
    }
}

#[test]
fn repeated_and_preinitialized_thread_roles_do_not_panic() {
    std::thread::spawn(|| {
        // `ThreadState::get()` cannot distinguish uninitialized from an
        // explicitly initialized empty role. The adapter must handle both
        // without calling the imported one-shot initializer.
        style::thread_state::initialize(style::thread_state::ThreadState::empty());
        for _ in 0..2 {
            let snapshot = parse_document("<p>x</p>")
                .unwrap()
                .document
                .snapshot()
                .unwrap();
            prepare_computed_styles(snapshot, StaticStyleOptions::default()).unwrap();
        }
    })
    .join()
    .unwrap();

    std::thread::spawn(|| {
        style::thread_state::initialize(style::thread_state::ThreadState::LAYOUT);
        for _ in 0..2 {
            let snapshot = parse_document("<p>x</p>")
                .unwrap()
                .document
                .snapshot()
                .unwrap();
            prepare_computed_styles(snapshot, StaticStyleOptions::default()).unwrap();
        }
    })
    .join()
    .unwrap();

    std::thread::spawn(|| {
        style::thread_state::initialize(style::thread_state::ThreadState::SCRIPT);
        let snapshot = parse_document("<p>x</p>")
            .unwrap()
            .document
            .snapshot()
            .unwrap();
        assert!(matches!(
            prepare_computed_styles(snapshot, StaticStyleOptions::default()),
            Err(StyleAdapterError::IncompatibleThreadState { .. })
        ));
    })
    .join()
    .unwrap();
}

#[test]
fn unsupported_intrinsic_sizing_fails_instead_of_becoming_auto_or_full_width() {
    let snapshot = parse_document(
        "<style>#target { display: block; width: min-content }</style><div id=target>x</div>",
    )
    .unwrap()
    .document
    .snapshot()
    .unwrap();
    assert!(matches!(
        prepare_computed_styles(snapshot, StaticStyleOptions::default()),
        Err(StyleAdapterError::UnsupportedComputedValue {
            value: UnsupportedComputedValue::Sizing("width", _),
            ..
        })
    ));
}
