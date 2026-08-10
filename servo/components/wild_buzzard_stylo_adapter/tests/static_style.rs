use wild_buzzard_dom::{
    AttributeName, Document, DocumentSnapshot, Namespace, NodeId, NodeKind, QualifiedName,
};
use wild_buzzard_html::parse_document;
use wild_buzzard_layout::{
    layout_document_with_style_snapshot, AlignItems, AlignSelf, Au, AutomaticMarginContext,
    AutomaticMarginEdges, BoxSizing, Color, Display, FlexBasis, FlexDirection, FlexFactor,
    FlexWrap, InlineDirection, JustifyContent, LayoutError, LengthPercentage, MaxSizeValue,
    MonospaceTextMeasurer, SizeValue, Viewport, WritingMode,
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

fn fragment_for(
    layout: &wild_buzzard_layout::LayoutOutput,
    node: NodeId,
) -> &wild_buzzard_layout::Fragment {
    &layout.boxes_for_node(node).next().unwrap().fragments[0]
}

fn assert_generic_desktop_geometry(
    snapshot: &DocumentSnapshot,
    styles: &wild_buzzard_layout::ComputedStyleSnapshot,
    viewport_width: i32,
    viewport_height: i32,
    expected_form_width: i32,
    expected_field_width: i32,
) {
    let layout = layout_document_with_style_snapshot(
        snapshot,
        Viewport::from_css_pixels(viewport_width, viewport_height),
        styles,
        &MonospaceTextMeasurer,
    )
    .unwrap();
    assert_eq!(layout.document_version, snapshot.version());
    let rect = |id: &str| fragment_for(&layout, node_with_id(snapshot, id)).rect;
    assert_eq!(rect("header").size.width, Au::from_px(viewport_width));
    assert_eq!(rect("header").size.height, Au::from_px(64));
    assert_eq!(rect("brand").origin.x, Au::from_px(12));
    assert_eq!(rect("brand").origin.y, Au::from_px(16));
    assert_eq!(rect("search-form").origin.x, Au::from_px(184));
    assert_eq!(
        rect("search-form").size.width,
        Au::from_px(expected_form_width)
    );
    assert_eq!(rect("field-shell").origin.x, Au::from_px(184));
    assert_eq!(
        rect("field-shell").size.width,
        Au::from_px(expected_field_width)
    );
    assert_eq!(rect("submit").origin.x, Au::from_px(viewport_width - 240));
    assert_eq!(rect("actions").origin.x, Au::from_px(viewport_width - 132));

    let expected_results_x = Au::from_raw(viewport_width * Au::PER_CSS_PX / 8);
    let expected_results_width = Au::from_raw(viewport_width * Au::PER_CSS_PX * 3 / 4);
    assert_eq!(rect("results").origin.x, expected_results_x);
    assert_eq!(rect("results").size.width, expected_results_width);
    assert_eq!(rect("result-one").origin.x, expected_results_x);
    assert_eq!(rect("result-one").origin.y, Au::from_px(88));
    assert_eq!(rect("result-one").size.width, expected_results_width);
    assert_eq!(rect("result-one").size.height, Au::from_px(96));
    assert_eq!(rect("result-two").origin.y, Au::from_px(200));
    assert_eq!(rect("result-three").origin.y, Au::from_px(312));
}

#[test]
fn stylo_flex_values_are_projected_without_reparsing_css() {
    let parsed = parse_document(
        r"<style>
          #container {
            display: flex; flex-direction: column; flex-wrap: wrap;
            justify-content: space-evenly; align-items: center;
            row-gap: 5px; column-gap: 10%;
          }
          #item { flex: 2 3 25%; align-self: flex-end; order: -2 }
        </style><div id=container><div id=item></div></div>",
    )
    .unwrap();
    let snapshot = parsed.document.snapshot().unwrap();
    let container = node_with_id(&snapshot, "container");
    let item = node_with_id(&snapshot, "item");
    let result = prepare_computed_styles(snapshot, StaticStyleOptions::default()).unwrap();
    let container = result.layout_styles().get(container).unwrap();
    assert_eq!(container.display, Display::Flex);
    assert_eq!(container.flex.direction, FlexDirection::Column);
    assert_eq!(container.flex.wrap, FlexWrap::Wrap);
    assert_eq!(container.flex.justify_content, JustifyContent::SpaceEvenly);
    assert_eq!(container.flex.align_items, AlignItems::Center);
    assert_eq!(
        container.flex.row_gap,
        LengthPercentage::length(Au::from_px(5))
    );
    assert_eq!(
        container.flex.column_gap,
        LengthPercentage::percentage(100_000)
    );

    let item = result.layout_styles().get(item).unwrap();
    assert_eq!(
        item.flex.basis,
        FlexBasis::LengthPercentage(LengthPercentage::percentage(250_000))
    );
    assert_eq!(item.flex.grow, FlexFactor::from_millionths(2_000_000));
    assert_eq!(item.flex.shrink, FlexFactor::from_millionths(3_000_000));
    assert_eq!(item.flex.align_self, AlignSelf::End);
    assert_eq!(item.flex.order, -2);
}

#[test]
fn stylo_auto_margins_project_and_follow_generic_css2_block_width_resolution() {
    let parsed = parse_document(
        r"<style>
          html, body { margin:0 }
          #both { width:80px; height:10px; margin-left:auto; margin-right:auto }
          #left { width:80px; height:10px; margin-left:auto; margin-right:10px }
          #right { width:80px; height:10px; margin-left:10px; margin-right:auto }
          #constrained { max-width:60px; height:10px; margin-left:auto; margin-right:auto }
          #over { width:220px; height:10px; margin-left:auto; margin-right:auto }
          #vertical { height:10px; margin-top:auto; margin-bottom:auto }
          #after { height:10px }
        </style>
        <div id=both></div><div id=left></div><div id=right></div>
        <div id=constrained></div><div id=over></div>
        <div id=vertical></div><div id=after></div>",
    )
    .unwrap();
    let snapshot = parsed.document.snapshot().unwrap();
    let styles = prepare_computed_styles(snapshot.clone(), StaticStyleOptions::default()).unwrap();
    let both = node_with_id(&snapshot, "both");
    assert_eq!(
        styles.layout_styles().get(both).unwrap().automatic_margin,
        AutomaticMarginEdges {
            right: true,
            left: true,
            ..AutomaticMarginEdges::default()
        }
    );
    let vertical = node_with_id(&snapshot, "vertical");
    assert_eq!(
        styles
            .layout_styles()
            .get(vertical)
            .unwrap()
            .automatic_margin,
        AutomaticMarginEdges {
            top: true,
            bottom: true,
            ..AutomaticMarginEdges::default()
        }
    );

    let layout = layout_document_with_style_snapshot(
        &snapshot,
        Viewport::from_css_pixels(200, 200),
        styles.layout_styles(),
        &MonospaceTextMeasurer,
    )
    .unwrap();
    let rect = |id: &str| fragment_for(&layout, node_with_id(&snapshot, id)).rect;
    assert_eq!(rect("both").origin.x, Au::from_px(60));
    assert_eq!(rect("left").origin.x, Au::from_px(110));
    assert_eq!(rect("right").origin.x, Au::from_px(10));
    assert_eq!(rect("constrained").origin.x, Au::from_px(70));
    assert_eq!(rect("constrained").size.width, Au::from_px(60));
    assert_eq!(rect("over").origin.x, Au::ZERO);
    assert_eq!(rect("vertical").origin.y, Au::from_px(50));
    assert_eq!(rect("after").origin.y, Au::from_px(60));
}

#[test]
fn stylo_auto_margins_in_unsupported_inline_and_flex_item_contexts_fail_typed() {
    for (source, id, expected_context) in [
        (
            "<style>html,body{margin:0} #target{margin-left:auto}</style>\
             <span id=target>x</span>",
            "target",
            AutomaticMarginContext::InlineFormatting,
        ),
        (
            "<style>html,body{margin:0} #container{display:flex} \
             #target{margin-left:auto}</style><div id=container><div id=target></div></div>",
            "target",
            AutomaticMarginContext::FlexItem,
        ),
    ] {
        let parsed = parse_document(source).unwrap();
        let snapshot = parsed.document.snapshot().unwrap();
        let target = node_with_id(&snapshot, id);
        let styles =
            prepare_computed_styles(snapshot.clone(), StaticStyleOptions::default()).unwrap();
        assert!(
            styles
                .layout_styles()
                .get(target)
                .unwrap()
                .automatic_margin
                .left
        );
        assert!(matches!(
            layout_document_with_style_snapshot(
                &snapshot,
                Viewport::from_css_pixels(200, 100),
                styles.layout_styles(),
                &MonospaceTextMeasurer,
            ),
            Err(LayoutError::UnsupportedAutomaticMargin {
                node_id: Some(reported),
                context,
            }) if reported == target && context == expected_context
        ));
    }
}

#[test]
fn generic_viewport_sized_centered_block_is_exact_at_desktop_viewports() {
    let parsed = parse_document(
        r"<style>
          html, body { margin:0 }
          #panel { width:60vw; height:10px; margin:15vh auto }
        </style><main id=panel></main>",
    )
    .unwrap();
    let snapshot = parsed.document.snapshot().unwrap();
    let panel = node_with_id(&snapshot, "panel");
    for (viewport_width, viewport_height, expected_x, expected_y, expected_width) in [
        (1366_u32, 768_u32, 16_392, 6_912, 49_176),
        (1920_u32, 1080_u32, 23_040, 9_720, 69_120),
    ] {
        let styles = prepare_computed_styles(
            snapshot.clone(),
            StaticStyleOptions {
                viewport_width,
                viewport_height,
                ..StaticStyleOptions::default()
            },
        )
        .unwrap();
        let style = styles.layout_styles().get(panel).unwrap();
        assert_eq!(
            style.automatic_margin,
            AutomaticMarginEdges {
                right: true,
                left: true,
                ..AutomaticMarginEdges::default()
            }
        );
        assert_eq!(style.margin.top, Au::from_raw(expected_y));
        assert_eq!(style.margin.bottom, Au::from_raw(expected_y));
        assert_eq!(style.width, SizeValue::length(Au::from_raw(expected_width)));

        let layout = layout_document_with_style_snapshot(
            &snapshot,
            Viewport::from_css_pixels(
                i32::try_from(viewport_width).unwrap(),
                i32::try_from(viewport_height).unwrap(),
            ),
            styles.layout_styles(),
            &MonospaceTextMeasurer,
        )
        .unwrap();
        let rect = fragment_for(&layout, panel).rect;
        assert_eq!(rect.origin.x, Au::from_raw(expected_x));
        assert_eq!(rect.origin.y, Au::from_raw(expected_y));
        assert_eq!(rect.size.width, Au::from_raw(expected_width));
        assert_eq!(rect.size.height, Au::from_px(10));
    }
}

#[test]
fn stylo_projects_explicit_ltr_inline_direction() {
    let parsed = parse_document(
        "<style>#parent{direction:rtl} #target{direction:ltr}</style>\
         <div id=parent><div id=target></div></div>",
    )
    .unwrap();
    let snapshot = parsed.document.snapshot().unwrap();
    let parent = node_with_id(&snapshot, "parent");
    let target = node_with_id(&snapshot, "target");
    let styles = prepare_computed_styles(snapshot, StaticStyleOptions::default()).unwrap();
    assert_eq!(
        styles.layout_styles().get(parent).unwrap().inline_direction,
        InlineDirection::Rtl
    );
    assert_eq!(
        styles.layout_styles().get(target).unwrap().inline_direction,
        InlineDirection::Ltr
    );
}

fn assert_stylo_rtl_block_fails_before_layout(declarations: &str) {
    let parsed = parse_document(&format!(
        "<style>html,body{{margin:0}} #target{{{declarations}}}</style>\
         <div id=target></div>"
    ))
    .unwrap();
    let snapshot = parsed.document.snapshot().unwrap();
    let target = node_with_id(&snapshot, "target");
    let styles = prepare_computed_styles(snapshot.clone(), StaticStyleOptions::default()).unwrap();
    assert_eq!(
        styles.layout_styles().get(target).unwrap().inline_direction,
        InlineDirection::Rtl
    );
    assert_eq!(
        layout_document_with_style_snapshot(
            &snapshot,
            Viewport::from_css_pixels(200, 100),
            styles.layout_styles(),
            &MonospaceTextMeasurer,
        ),
        Err(LayoutError::UnsupportedInlineDirection {
            node: target,
            direction: InlineDirection::Rtl,
        })
    );
}

#[test]
fn stylo_rtl_block_with_auto_margins_fails_before_fragment_publication() {
    assert_stylo_rtl_block_fails_before_layout("direction:rtl;width:220px;margin:auto");
}

#[test]
fn stylo_rtl_block_without_auto_margins_fails_before_fragment_publication() {
    assert_stylo_rtl_block_fails_before_layout("direction:rtl;width:40px");
}

#[test]
fn unsupported_reverse_and_baseline_flex_values_fail_typed() {
    for (css, property) in [
        ("display:flex; flex-direction:row-reverse", "flex-direction"),
        ("display:flex; flex-wrap:wrap-reverse", "flex-wrap"),
        ("display:flex; align-content:center", "align-content"),
        ("display:flex; align-items:baseline", "align-items"),
        ("display:flex; flex-basis:max-content", "flex-basis"),
        ("display:flex; row-gap:calc(10px + 10%)", "row-gap"),
        ("display:flex; flex-grow:5000", "flex-grow"),
    ] {
        let parsed = parse_document(&format!("<div style='{css}'></div>")).unwrap();
        assert!(matches!(
            prepare_computed_styles(
                parsed.document.snapshot().unwrap(),
                StaticStyleOptions::default(),
            ),
            Err(StyleAdapterError::UnsupportedComputedValue {
                value: UnsupportedComputedValue::Flex(reported, _),
                ..
            }) if reported == property
        ));
    }
}

#[test]
fn unused_flex_container_values_do_not_reject_a_non_flex_box() {
    let parsed = parse_document(
        "<div id=box style='flex-direction:row-reverse; flex-wrap:wrap-reverse; \
         align-content:center; row-gap:calc(10px + 10%)'></div>",
    )
    .unwrap();
    let snapshot = parsed.document.snapshot().unwrap();
    let target = node_with_id(&snapshot, "box");
    let result = prepare_computed_styles(snapshot, StaticStyleOptions::default()).unwrap();
    assert_eq!(
        result.layout_styles().get(target).unwrap().display,
        Display::Block
    );
}

#[test]
fn inline_flex_is_classified_as_an_unsupported_display_value() {
    let parsed = parse_document("<div style='display:inline-flex'></div>").unwrap();
    assert!(matches!(
        prepare_computed_styles(
            parsed.document.snapshot().unwrap(),
            StaticStyleOptions::default(),
        ),
        Err(StyleAdapterError::UnsupportedComputedValue {
            value: UnsupportedComputedValue::Display(_),
            ..
        })
    ));
}

#[test]
fn wpt_derived_row_flex_freezes_clamps_orders_and_aligns() {
    let parsed = parse_document(
        r"<style>
          html, body { margin: 0 }
          #container { display:flex; width:400px; height:100px; column-gap:10px; align-items:center }
          .item { flex:1 1 100px }
          #first { height:20px; max-width:120px; order:2 }
          #second { height:30px; align-self:flex-end; order:1 }
        </style><div id=container><div class=item id=first></div><div class=item id=second></div></div>",
    )
    .unwrap();
    let snapshot = parsed.document.snapshot().unwrap();
    let container = node_with_id(&snapshot, "container");
    let first = node_with_id(&snapshot, "first");
    let second = node_with_id(&snapshot, "second");
    let styles = prepare_computed_styles(snapshot.clone(), StaticStyleOptions::default()).unwrap();
    let layout = layout_document_with_style_snapshot(
        &snapshot,
        Viewport::from_css_pixels(1366, 768),
        styles.layout_styles(),
        &MonospaceTextMeasurer,
    )
    .unwrap();
    assert_eq!(
        fragment_for(&layout, container).rect.size.width,
        Au::from_px(400)
    );
    let second_fragment = fragment_for(&layout, second);
    assert_eq!(second_fragment.rect.origin.x, Au::ZERO);
    assert_eq!(second_fragment.rect.origin.y, Au::from_px(70));
    assert_eq!(second_fragment.rect.size.width, Au::from_px(270));
    assert_eq!(second_fragment.rect.size.height, Au::from_px(30));
    let first_fragment = fragment_for(&layout, first);
    assert_eq!(first_fragment.rect.origin.x, Au::from_px(280));
    assert_eq!(first_fragment.rect.origin.y, Au::from_px(40));
    assert_eq!(first_fragment.rect.size.width, Au::from_px(120));
    assert_eq!(first_fragment.rect.size.height, Au::from_px(20));
    // Box-tree children retain DOM order even though visual placement uses `order`.
    let container_box = layout.boxes_for_node(container).next().unwrap();
    assert_eq!(
        layout.box_by_id(container_box.children[0]).unwrap().node_id,
        Some(first)
    );
    assert_eq!(
        layout.box_by_id(container_box.children[1]).unwrap().node_id,
        Some(second)
    );
}

#[test]
fn wpt_derived_wrap_gap_justify_and_column_geometry_is_exact() {
    let parsed = parse_document(
        r"<style>
          html, body { margin:0 }
          #row { display:flex; flex-wrap:wrap; width:130px; row-gap:20px; column-gap:10px }
          #row > div { flex:0 0 60px; height:10px }
          #column { display:flex; flex-direction:column; width:100px; height:200px;
                    justify-content:space-between; align-items:center; row-gap:10px }
          #column > div { flex:0 0 40px; width:20px }
          #column > #end { align-self:flex-end }
        </style>
        <div id=row><div id=r1></div><div id=r2></div><div id=r3></div></div>
        <div id=column><div id=c1></div><div id=end></div><div id=c3></div></div>",
    )
    .unwrap();
    let snapshot = parsed.document.snapshot().unwrap();
    let styles = prepare_computed_styles(snapshot.clone(), StaticStyleOptions::default()).unwrap();
    let layout = layout_document_with_style_snapshot(
        &snapshot,
        Viewport::from_css_pixels(1366, 768),
        styles.layout_styles(),
        &MonospaceTextMeasurer,
    )
    .unwrap();
    let rect = |id: &str| fragment_for(&layout, node_with_id(&snapshot, id)).rect;
    assert_eq!(rect("r1").origin.x, Au::ZERO);
    assert_eq!(rect("r2").origin.x, Au::from_px(70));
    assert_eq!(rect("r3").origin.x, Au::ZERO);
    assert_eq!(rect("r3").origin.y, Au::from_px(30));
    assert_eq!(rect("column").origin.y, Au::from_px(40));
    assert_eq!(rect("c1").origin.x, Au::from_px(40));
    assert_eq!(rect("c1").origin.y, Au::from_px(40));
    assert_eq!(rect("end").origin.x, Au::from_px(80));
    assert_eq!(rect("end").origin.y, Au::from_px(120));
    assert_eq!(rect("c3").origin.x, Au::from_px(40));
    assert_eq!(rect("c3").origin.y, Au::from_px(200));
}

#[test]
fn wpt_derived_basis_auto_content_and_scaled_shrink_use_real_item_geometry() {
    let parsed = parse_document(
        r"<style>
          html, body { margin:0 }
          #basis { display:flex; width:100px }
          #content { flex:0 0 content }
          #auto { flex:0 0 auto; width:30px }
          #percent { flex:0 0 25% }
          #shrink { display:flex; width:100px; margin-top:10px }
          #shrink > div { flex:0 1 60px; height:10px }
          #shrink > #minimum { min-width:55px }
          #indefinite { display:flex; flex-direction:column }
          #indefinite-item { flex:0 0 50%; height:30px }
        </style>
        <div id=basis><span id=content>abc</span><span id=auto></span><span id=percent></span></div>
        <div id=shrink><div id=minimum></div><div id=remainder></div></div>
        <div id=indefinite><div id=indefinite-item></div></div>",
    )
    .unwrap();
    let snapshot = parsed.document.snapshot().unwrap();
    let styles = prepare_computed_styles(snapshot.clone(), StaticStyleOptions::default()).unwrap();
    let layout = layout_document_with_style_snapshot(
        &snapshot,
        Viewport::from_css_pixels(1366, 768),
        styles.layout_styles(),
        &MonospaceTextMeasurer,
    )
    .unwrap();
    let rect = |id: &str| fragment_for(&layout, node_with_id(&snapshot, id)).rect;
    assert_eq!(rect("content").size.width, Au::from_px(24));
    assert_eq!(rect("auto").origin.x, Au::from_px(24));
    assert_eq!(rect("auto").size.width, Au::from_px(30));
    assert_eq!(rect("percent").origin.x, Au::from_px(54));
    assert_eq!(rect("percent").size.width, Au::from_px(25));
    assert_eq!(rect("minimum").size.width, Au::from_px(55));
    assert_eq!(rect("remainder").origin.x, Au::from_px(55));
    assert_eq!(rect("remainder").size.width, Au::from_px(45));
    assert_eq!(rect("indefinite").size.height, Au::ZERO);
    assert_eq!(rect("indefinite-item").size.height, Au::ZERO);
}

#[test]
fn row_flex_remeasures_auto_cross_size_at_the_resolved_item_width() {
    let parsed = parse_document(
        r"<style>
          html, body { margin:0 }
          #container { display:flex; width:100px; align-items:flex-start }
          #item { flex:0 0 20px; font-size:10px; line-height:10px }
        </style><div id=container><div id=item>abcdefghij</div></div>",
    )
    .unwrap();
    let snapshot = parsed.document.snapshot().unwrap();
    let container = node_with_id(&snapshot, "container");
    let item = node_with_id(&snapshot, "item");
    let text_node = snapshot
        .nodes_in_document_order()
        .iter()
        .find_map(|node| match &node.kind {
            NodeKind::Text(data) if data == "abcdefghij" => Some(node.id),
            _ => None,
        })
        .unwrap();
    let styles = prepare_computed_styles(snapshot.clone(), StaticStyleOptions::default()).unwrap();
    let layout = layout_document_with_style_snapshot(
        &snapshot,
        Viewport::from_css_pixels(1366, 768),
        styles.layout_styles(),
        &MonospaceTextMeasurer,
    )
    .unwrap();

    assert_eq!(fragment_for(&layout, item).rect.size.width, Au::from_px(20));
    assert_eq!(
        fragment_for(&layout, item).rect.size.height,
        Au::from_px(30)
    );
    assert_eq!(
        fragment_for(&layout, container).rect.size.height,
        Au::from_px(30)
    );
    let text_box = layout.boxes_for_node(text_node).next().unwrap();
    assert_eq!(text_box.fragments.len(), 3);
    assert_eq!(text_box.fragments[0].text.as_deref(), Some("abcd"));
    assert_eq!(text_box.fragments[1].text.as_deref(), Some("efgh"));
    assert_eq!(text_box.fragments[2].text.as_deref(), Some("ij"));
    assert_eq!(text_box.fragments[2].rect.origin.y, Au::from_px(20));
}

#[test]
fn generic_search_header_form_and_results_scale_at_normal_desktop_viewports() {
    let parsed = parse_document(
        r"<style>
          html, body { margin:0 }
          #header {
            display:flex; width:100%; height:64px; padding:12px;
            box-sizing:border-box; align-items:center; column-gap:12px;
          }
          #brand { flex:0 0 160px; height:32px }
          #search-form { display:flex; flex:1 1 auto; height:40px; column-gap:8px }
          #field-shell { flex:1 1 auto; min-width:240px; height:40px }
          #submit { flex:0 0 96px; height:40px }
          #actions { flex:0 0 120px; height:32px }
          #results {
            display:flex; flex-direction:column; width:75%; margin-left:12.5%;
            margin-top:24px; row-gap:16px;
          }
          .result {
            display:flex; flex-direction:column; flex:0 0 96px;
            padding:12px; box-sizing:border-box; row-gap:8px;
          }
          .title { flex:0 0 20px }
          .summary { flex:0 0 44px }
        </style>
        <header id=header>
          <div id=brand></div>
          <form id=search-form role=search><div id=field-shell></div><button id=submit></button></form>
          <nav id=actions></nav>
        </header>
        <main id=results>
          <article class=result id=result-one><div class=title></div><div class=summary></div></article>
          <article class=result id=result-two><div class=title></div><div class=summary></div></article>
          <article class=result id=result-three><div class=title></div><div class=summary></div></article>
        </main>",
    )
    .unwrap();
    let snapshot = parsed.document.snapshot().unwrap();
    let styles = prepare_computed_styles(snapshot.clone(), StaticStyleOptions::default()).unwrap();
    let first = node_with_id(&snapshot, "result-one");
    let first_style = styles.layout_styles().get(first).unwrap();
    assert_eq!(first_style.padding.top, Au::from_px(12));
    assert_eq!(first_style.box_sizing, BoxSizing::BorderBox);
    assert_eq!(
        first_style.flex.basis,
        FlexBasis::LengthPercentage(LengthPercentage::length(Au::from_px(96)))
    );

    for (viewport_width, viewport_height, form_width, field_width) in
        [(1366, 768, 1038, 934), (1920, 1080, 1592, 1488)]
    {
        assert_generic_desktop_geometry(
            &snapshot,
            styles.layout_styles(),
            viewport_width,
            viewport_height,
            form_width,
            field_width,
        );
    }
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
