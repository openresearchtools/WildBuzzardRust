use webrender_api::{
    BorderDetails, BuiltDisplayList, ClipChainId, DisplayItem, FontInstanceKey, GlyphInstance,
    IdNamespace, PipelineId, PropertyBinding, SpatialId,
};
use wild_buzzard_dom::{Document, DocumentVersion, NodeId};
use wild_buzzard_html::parse_document;
use wild_buzzard_layout::{
    Au, Color as LayoutColor, ComputedStyle, Edges, InitialStyleResolver, LayoutOutput,
    MonospaceTextMeasurer, Size, StyleInput, StyleResolver, Viewport, layout_document,
};
use wild_buzzard_renderer::{
    CompileRequest, GeometryField, PipelineKey, ResourceKind, SceneBuildError, SceneCompiler,
    SceneItem, SceneLimits, SceneTextDescriptor, SceneTextMetrics,
};

const PIPELINE: PipelineKey = PipelineKey::new(7, 11);
const FONT_NAMESPACE: IdNamespace = IdNamespace(99);

struct FixtureStyles;

impl StyleResolver for FixtureStyles {
    fn resolve(&self, input: StyleInput<'_>) -> ComputedStyle {
        let background = input.element.html_attribute("data-bg").map(str::to_owned);
        let has_border = input.element.html_attribute("data-border").is_some();
        let mut style = InitialStyleResolver.resolve(input);
        style.background_color = match background.as_deref() {
            Some("red") => rgba(220, 20, 30, 255),
            Some("green") => rgba(30, 180, 70, 255),
            Some("blue") => rgba(20, 80, 220, 192),
            _ => style.background_color,
        };
        if has_border {
            style.border = Edges::all(Au::from_px(1));
            style.color = rgba(5, 10, 15, 255);
        }
        style
    }
}

const fn rgba(red: u8, green: u8, blue: u8, alpha: u8) -> LayoutColor {
    LayoutColor {
        red,
        green,
        blue,
        alpha,
    }
}

fn parsed_layout(source: &str) -> (Document, LayoutOutput) {
    let document = parse_document(source)
        .expect("fixture HTML must parse")
        .document;
    let output = layout_document(
        &document.snapshot().expect("fixture snapshot must succeed"),
        Viewport::from_css_pixels(320, 180),
        &FixtureStyles,
        &MonospaceTextMeasurer,
    )
    .expect("fixture layout must succeed");
    (document, output)
}

fn compile(output: &LayoutOutput) -> wild_buzzard_renderer::CompiledScene {
    SceneCompiler::default()
        .compile(
            output,
            CompileRequest::new(output.document_version, PIPELINE),
        )
        .expect("valid fixture must compile")
}

fn expect_error(
    result: Result<wild_buzzard_renderer::CompiledScene, SceneBuildError>,
) -> SceneBuildError {
    match result {
        Ok(_) => panic!("malformed fixture unexpectedly compiled"),
        Err(error) => error,
    }
}

fn assert_close(actual: f32, expected: f32) {
    assert!((actual - expected).abs() <= f32::EPSILON);
}

fn node(document: &Document, tag: &str) -> NodeId {
    document
        .elements_by_tag_name(tag)
        .expect("tag query must succeed")[0]
}

fn box_index(output: &LayoutOutput, node: NodeId) -> u32 {
    u32::try_from(
        output
            .boxes_for_node(node)
            .next()
            .expect("node must have a layout box")
            .id
            .index(),
    )
    .expect("test box index must fit u32")
}

#[allow(clippy::cast_precision_loss)]
fn css_px(app_units: i32) -> f32 {
    app_units as f32 / Au::PER_CSS_PX as f32
}

fn scene_text_metrics(run: &wild_buzzard_renderer::PendingTextRun) -> SceneTextMetrics {
    SceneTextMetrics::new(
        css_px(run.rect().width()),
        css_px(run.rect().height()),
        css_px(run.baseline()),
        css_px(run.font_size()),
        css_px(run.line_height()),
    )
}

fn descriptor(
    document_version: DocumentVersion,
    run: &wild_buzzard_renderer::PendingTextRun,
) -> SceneTextDescriptor<'_> {
    SceneTextDescriptor::new(
        document_version,
        run.id().index(),
        run.text(),
        scene_text_metrics(run),
    )
}

#[test]
fn scene_is_deterministic_and_uses_preorder_painting() {
    let (document, output) = parsed_layout(
        "<body data-bg=red data-border><div data-bg=green data-border>alpha</div><p data-bg=blue data-border>beta</p></body>",
    );
    let first = compile(&output);
    let second = compile(&output);

    assert_eq!(first.scene(), second.scene());
    assert_eq!(
        first.built_display_list().items_data(),
        second.built_display_list().items_data()
    );
    assert_eq!(first.scene().document_version(), output.document_version);
    assert_eq!(first.scene().viewport().width(), Au::from_px(320).raw());
    assert_eq!(first.scene().viewport().height(), Au::from_px(180).raw());
    assert_eq!(first.scene().spatial_root().index(), 0);
    assert_eq!(first.scene().viewport_clip().index(), 0);

    let body = box_index(&output, node(&document, "body"));
    let div = box_index(&output, node(&document, "div"));
    let paragraph = box_index(&output, node(&document, "p"));
    let observed = first
        .scene()
        .items()
        .iter()
        .map(|item| match item {
            SceneItem::Background(item) => ("background", item.source_box().index()),
            SceneItem::Border(item) => ("border", item.source_box().index()),
            SceneItem::PendingText(item) => {
                let run = first
                    .scene()
                    .pending_text_by_id(item.pending_text())
                    .expect("pending text ID must resolve");
                (run.text(), run.source_box().index())
            }
            SceneItem::Text(_) => panic!("freshly compiled text must still be pending"),
        })
        .collect::<Vec<_>>();
    assert_eq!(
        observed,
        vec![
            ("background", body),
            ("border", body),
            ("background", div),
            ("border", div),
            (
                "alpha",
                first.scene().pending_text()[0].source_box().index()
            ),
            ("background", paragraph),
            ("border", paragraph),
            ("beta", first.scene().pending_text()[1].source_box().index()),
        ]
    );
    for (expected, item) in first.scene().items().iter().enumerate() {
        assert_eq!(
            first.scene().item_id(item).index(),
            u32::try_from(expected).expect("small test index")
        );
    }
}

#[test]
fn builds_real_webrender_clip_rectangle_and_border_items() {
    let (_, output) = parsed_layout(
        "<body data-bg=red data-border><div data-bg=green data-border>text</div></body>",
    );
    let compiled = compile(&output);
    let mut iterator = compiled.built_display_list().iter();
    let mut kinds = Vec::new();
    let mut saw_clip_chain_member = false;
    let mut saw_expected_border = false;
    let root_spatial = SpatialId::root_scroll_node(PipelineId(7, 11));

    while let Some(item) = iterator.next() {
        match *item.item() {
            DisplayItem::RectClip(clip) => {
                kinds.push("clip");
                assert_eq!(clip.spatial_id, root_spatial);
                assert_close(clip.clip_rect.min.x, 0.0);
                assert_close(clip.clip_rect.min.y, 0.0);
                assert_close(clip.clip_rect.width(), 320.0);
                assert_close(clip.clip_rect.height(), 180.0);
            }
            DisplayItem::ClipChain(_) => {
                kinds.push("clip-chain");
                saw_clip_chain_member = item.clip_chain_items().iter().len() == 1;
            }
            DisplayItem::Rectangle(rectangle) => {
                kinds.push("rectangle");
                assert_eq!(rectangle.common.spatial_id, root_spatial);
                assert!(rectangle.bounds.width() > 0.0);
                assert!(rectangle.bounds.height() > 0.0);
                assert!(matches!(rectangle.color, PropertyBinding::Value(_)));
            }
            DisplayItem::Border(border) => {
                kinds.push("border");
                assert_eq!(border.common.spatial_id, root_spatial);
                assert_close(border.widths.top, 1.0);
                assert_close(border.widths.right, 1.0);
                assert_close(border.widths.bottom, 1.0);
                assert_close(border.widths.left, 1.0);
                let BorderDetails::Normal(details) = border.details else {
                    panic!("first-wave border must be normal");
                };
                saw_expected_border = !details.do_aa;
            }
            DisplayItem::Text(_) => panic!("unshaped text must not be sent to WebRender"),
            _ => panic!("unexpected first-wave WebRender display item"),
        }
    }

    assert_eq!(
        kinds,
        vec![
            "clip",
            "clip-chain",
            "rectangle",
            "border",
            "rectangle",
            "border"
        ]
    );
    assert!(saw_clip_chain_member);
    assert!(saw_expected_border);
    assert!(!compiled.built_display_list().items_data().is_empty());
}

#[test]
fn webrender_payload_round_trips_through_public_data_contract() {
    let (_, output) = parsed_layout("<body data-bg=blue data-border>round trip</body>");
    let compiled = compile(&output);
    let original = compiled.built_display_list().items_data().to_vec();
    let cloned = compiled.built_display_list().clone();
    let (payload, descriptor) = cloned.into_data();
    let rebuilt = BuiltDisplayList::from_data(payload, descriptor);

    assert_eq!(rebuilt.items_data(), original);
    let rebuilt_kinds = display_item_kinds(&rebuilt);
    assert_eq!(
        rebuilt_kinds,
        vec!["clip", "clip-chain", "rectangle", "border"]
    );
}

fn display_item_kinds(display_list: &BuiltDisplayList) -> Vec<&'static str> {
    let mut iterator = display_list.iter();
    let mut kinds = Vec::new();
    while let Some(item) = iterator.next() {
        kinds.push(match item.item() {
            DisplayItem::RectClip(_) => "clip",
            DisplayItem::ClipChain(_) => "clip-chain",
            DisplayItem::Rectangle(_) => "rectangle",
            DisplayItem::Border(_) => "border",
            DisplayItem::Text(_) => "text",
            _ => "other",
        });
    }
    kinds
}

#[test]
fn preserves_unshaped_text_as_typed_pending_resources() {
    let (_, output) = parsed_layout("<body>A\u{00a0}Wild🦅Buzzard</body>");
    let compiled = compile(&output);
    let pending = compiled.scene().pending_text();

    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id().index(), 0);
    assert_eq!(pending[0].text(), "A\u{00a0}Wild🦅Buzzard");
    assert!(pending[0].rect().width() > 0);
    assert!(pending[0].rect().height() > 0);
    assert!(pending[0].baseline() > 0);
    assert_eq!(pending[0].font_size(), Au::from_px(16).raw());
    assert!(pending[0].line_height() >= pending[0].font_size());
    assert_eq!(pending[0].color().alpha(), 255);
    assert_eq!(pending[0].spatial_root().index(), 0);
    assert_eq!(pending[0].clip().index(), 0);
    assert!(
        display_item_kinds(compiled.built_display_list())
            .iter()
            .all(|kind| *kind != "text")
    );
}

#[test]
fn an_empty_layout_still_builds_a_valid_clipped_webrender_list() {
    let document = Document::new();
    let output = LayoutOutput {
        document_version: DocumentVersion::new(document.id(), 9),
        viewport: Viewport::from_css_pixels(80, 60),
        root: None,
        boxes: Vec::new(),
        content_size: Size {
            width: Au::from_px(80),
            height: Au::from_px(60),
        },
        warnings: Vec::new(),
    };
    let compiled = compile(&output);

    assert!(compiled.scene().items().is_empty());
    assert!(compiled.scene().pending_text().is_empty());
    assert_eq!(
        display_item_kinds(compiled.built_display_list()),
        vec!["clip", "clip-chain"]
    );
}

#[test]
fn rejects_wrong_document_stale_revision_and_invalid_pipeline() {
    let (_, output) = parsed_layout("<p>revision</p>");
    let next_version = DocumentVersion::new(
        output.document_version.document_id(),
        output.document_version.revision() + 1,
    );
    assert_eq!(
        expect_error(
            SceneCompiler::default().compile(&output, CompileRequest::new(next_version, PIPELINE))
        ),
        SceneBuildError::DocumentVersionMismatch {
            expected: next_version,
            actual: output.document_version,
        }
    );

    let (_, other_output) = parsed_layout("<p>revision</p>");
    assert_ne!(
        output.document_version.document_id(),
        other_output.document_version.document_id()
    );
    assert_eq!(
        expect_error(SceneCompiler::default().compile(
            &output,
            CompileRequest::new(other_output.document_version, PIPELINE)
        )),
        SceneBuildError::DocumentVersionMismatch {
            expected: other_output.document_version,
            actual: output.document_version,
        }
    );

    assert_eq!(
        expect_error(SceneCompiler::default().compile(
            &output,
            CompileRequest::new(
                output.document_version,
                PipelineKey::new(u32::MAX, u32::MAX)
            )
        )),
        SceneBuildError::InvalidPipeline
    );
}

#[test]
fn rejects_missing_and_misidentified_boxes() {
    let (_, mut missing_root) = parsed_layout("<body><div>x</div></body>");
    let root_index = missing_root.root.expect("root").index();
    missing_root.boxes.clear();
    assert_eq!(
        expect_error(SceneCompiler::default().compile(
            &missing_root,
            CompileRequest::new(missing_root.document_version, PIPELINE)
        )),
        SceneBuildError::MissingRootBox {
            box_index: root_index
        }
    );

    let (_, mut missing_child) = parsed_layout("<body><div>x</div></body>");
    let removed = missing_child.boxes.pop().expect("last box").id.index();
    assert!(missing_child.boxes.iter().any(|layout_box| {
        layout_box
            .children
            .iter()
            .any(|child| child.index() == removed)
    }));
    assert!(matches!(
        SceneCompiler::default().compile(
            &missing_child,
            CompileRequest::new(missing_child.document_version, PIPELINE)
        ),
        Err(SceneBuildError::MissingChildBox { child, .. }) if child == removed
    ));

    let (_, mut identity) = parsed_layout("<body><div>x</div></body>");
    identity.boxes.swap(0, 1);
    assert!(matches!(
        SceneCompiler::default().compile(
            &identity,
            CompileRequest::new(identity.document_version, PIPELINE)
        ),
        Err(SceneBuildError::InvalidBoxIdentity { slot: 0, .. })
    ));
}

#[test]
fn rejects_multiple_parents_unreachable_boxes_and_leaf_children() {
    let (_, mut multiple) = parsed_layout("<body><div><span>x</span></div></body>");
    let root = multiple.root.expect("root");
    let text_id = multiple
        .boxes
        .iter()
        .find(|layout_box| layout_box.kind == wild_buzzard_layout::BoxKind::Text)
        .expect("text box")
        .id;
    multiple.boxes[root.index()].children.push(text_id);
    assert_eq!(
        expect_error(SceneCompiler::default().compile(
            &multiple,
            CompileRequest::new(multiple.document_version, PIPELINE)
        )),
        SceneBuildError::MultipleParents {
            box_index: text_id.index()
        }
    );

    let (_, mut unreachable) = parsed_layout("<body><div>x</div><p>y</p></body>");
    let root = unreachable.root.expect("root");
    let detached = unreachable.boxes[root.index()]
        .children
        .pop()
        .expect("root child");
    assert!(matches!(
        SceneCompiler::default().compile(
            &unreachable,
            CompileRequest::new(unreachable.document_version, PIPELINE)
        ),
        Err(SceneBuildError::UnreachableBox { box_index }) if box_index == detached.index()
    ));

    let (_, mut leaf) = parsed_layout("<body>x</body>");
    let root = leaf.root.expect("root");
    let text_index = leaf
        .boxes
        .iter()
        .position(|layout_box| layout_box.kind == wild_buzzard_layout::BoxKind::Text)
        .expect("text box");
    leaf.boxes[text_index].children.push(root);
    assert_eq!(
        expect_error(
            SceneCompiler::default()
                .compile(&leaf, CompileRequest::new(leaf.document_version, PIPELINE))
        ),
        SceneBuildError::LeafHasChildren {
            box_index: text_index
        }
    );
}

#[test]
fn rejects_malformed_text_fragments() {
    let (_, mut wrong_owner) = parsed_layout("<body>x</body>");
    let block_index = wrong_owner.root.expect("root").index();
    wrong_owner.boxes[block_index].fragments[0].text = Some("not block text".to_owned());
    wrong_owner.boxes[block_index].fragments[0].baseline = Some(Au::ZERO);
    assert_eq!(
        expect_error(SceneCompiler::default().compile(
            &wrong_owner,
            CompileRequest::new(wrong_owner.document_version, PIPELINE)
        )),
        SceneBuildError::TextOnNonTextBox {
            box_index: block_index
        }
    );

    let (_, mut missing_baseline) = parsed_layout("<body>x</body>");
    let text_index = missing_baseline
        .boxes
        .iter()
        .position(|layout_box| layout_box.kind == wild_buzzard_layout::BoxKind::Text)
        .expect("text box");
    missing_baseline.boxes[text_index].fragments[0].baseline = None;
    assert_eq!(
        expect_error(SceneCompiler::default().compile(
            &missing_baseline,
            CompileRequest::new(missing_baseline.document_version, PIPELINE)
        )),
        SceneBuildError::TextMissingBaseline {
            box_index: text_index,
            fragment_index: 0,
        }
    );
}

#[test]
fn rejects_negative_out_of_range_and_overflowing_geometry() {
    let (_, mut negative) = parsed_layout("<body>x</body>");
    negative.boxes[0].fragments[0].rect.size.width = Au::from_raw(-1);
    assert_eq!(
        expect_error(SceneCompiler::default().compile(
            &negative,
            CompileRequest::new(negative.document_version, PIPELINE)
        )),
        SceneBuildError::NegativeGeometry {
            box_index: Some(0),
            field: GeometryField::Width,
            value: -1,
        }
    );

    let (_, mut out_of_range) = parsed_layout("<body>x</body>");
    out_of_range.boxes[0].fragments[0].rect.origin.x = Au::from_raw(60_000_001);
    assert_eq!(
        expect_error(SceneCompiler::default().compile(
            &out_of_range,
            CompileRequest::new(out_of_range.document_version, PIPELINE)
        )),
        SceneBuildError::GeometryOutOfRange {
            box_index: Some(0),
            field: GeometryField::X,
            value: 60_000_001,
            limit: 60_000_000,
        }
    );

    let (_, mut overflow) = parsed_layout("<body>x</body>");
    overflow.boxes[0].fragments[0].rect.origin.x = Au::from_raw(i32::MAX);
    overflow.boxes[0].fragments[0].rect.size.width = Au::from_raw(1);
    let compiler = SceneCompiler::new(SceneLimits::default().with_max_abs_app_units(i32::MAX));
    assert_eq!(
        expect_error(compiler.compile(
            &overflow,
            CompileRequest::new(overflow.document_version, PIPELINE)
        )),
        SceneBuildError::GeometryOverflow {
            box_index: Some(0),
            axis: GeometryField::X,
        }
    );
}

#[test]
fn enforces_box_fragment_item_and_depth_limits() {
    let (_, output) = parsed_layout("<body data-bg=red><div>x</div></body>");
    let cases = [
        (
            SceneLimits::default().with_max_boxes(0),
            ResourceKind::Boxes,
        ),
        (
            SceneLimits::default().with_max_fragments(0),
            ResourceKind::Fragments,
        ),
        (
            SceneLimits::default().with_max_scene_items(0),
            ResourceKind::SceneItems,
        ),
        (
            SceneLimits::default().with_max_tree_depth(1),
            ResourceKind::TreeDepth,
        ),
    ];
    for (limits, expected_resource) in cases {
        assert!(matches!(
            SceneCompiler::new(limits).compile(
                &output,
                CompileRequest::new(output.document_version, PIPELINE)
            ),
            Err(SceneBuildError::ResourceLimitExceeded { resource, .. })
                if resource == expected_resource
        ));
    }
}

#[test]
fn enforces_per_run_and_aggregate_text_limits() {
    let (_, one_run) = parsed_layout("<body>abcdef</body>");
    assert_eq!(
        expect_error(
            SceneCompiler::new(SceneLimits::default().with_max_text_run_bytes(5)).compile(
                &one_run,
                CompileRequest::new(one_run.document_version, PIPELINE)
            )
        ),
        SceneBuildError::ResourceLimitExceeded {
            resource: ResourceKind::TextRunBytes,
            observed: 6,
            limit: 5,
        }
    );

    let (_, two_runs) = parsed_layout("<body><div>abcd</div><p>efgh</p></body>");
    assert_eq!(
        expect_error(
            SceneCompiler::new(SceneLimits::default().with_max_total_text_bytes(7)).compile(
                &two_runs,
                CompileRequest::new(two_runs.document_version, PIPELINE)
            )
        ),
        SceneBuildError::ResourceLimitExceeded {
            resource: ResourceKind::TotalTextBytes,
            observed: 8,
            limit: 7,
        }
    );
}

#[test]
fn enforces_serialized_webrender_size_limit() {
    let (_, output) = parsed_layout("<body data-bg=red data-border>x</body>");
    assert!(matches!(
        SceneCompiler::new(SceneLimits::default().with_max_webrender_bytes(0)).compile(
            &output,
            CompileRequest::new(output.document_version, PIPELINE)
        ),
        Err(SceneBuildError::ResourceLimitExceeded {
            resource: ResourceKind::WebRenderBytes,
            observed,
            limit: 0,
        }) if observed > 0
    ));
}

#[test]
fn webrender_preflight_budget_has_an_exact_acceptance_boundary() {
    let (_, output) = parsed_layout("<body data-bg=red data-border>x</body>");
    let error = expect_error(
        SceneCompiler::new(SceneLimits::default().with_max_webrender_bytes(0)).compile(
            &output,
            CompileRequest::new(output.document_version, PIPELINE),
        ),
    );
    let SceneBuildError::ResourceLimitExceeded {
        resource: ResourceKind::WebRenderBytes,
        observed: preflight_budget,
        limit: 0,
    } = error
    else {
        panic!("zero byte limit must fail in WebRender preflight");
    };
    assert!(preflight_budget > 0);

    let accepted =
        SceneCompiler::new(SceneLimits::default().with_max_webrender_bytes(preflight_budget))
            .compile(
                &output,
                CompileRequest::new(output.document_version, PIPELINE),
            )
            .expect("the conservative preflight boundary must be sufficient");
    assert!(accepted.built_display_list().size_in_bytes() <= preflight_budget);

    assert_eq!(
        expect_error(
            SceneCompiler::new(
                SceneLimits::default().with_max_webrender_bytes(preflight_budget - 1),
            )
            .compile(
                &output,
                CompileRequest::new(output.document_version, PIPELINE),
            )
        ),
        SceneBuildError::ResourceLimitExceeded {
            resource: ResourceKind::WebRenderBytes,
            observed: preflight_budget,
            limit: preflight_budget - 1,
        }
    );

    // A rejected compile leaves no builder state behind and does not poison a
    // subsequent compilation of the same immutable input.
    let retried = compile(&output);
    assert_eq!(retried.scene(), accepted.scene());
}

#[test]
fn exact_scene_item_limit_succeeds_and_one_less_rejects() {
    let (_, output) = parsed_layout("<body data-bg=red data-border>x</body>");
    let expected_items = compile(&output).scene().items().len();
    assert!(expected_items > 0);

    let accepted = SceneCompiler::new(SceneLimits::default().with_max_scene_items(expected_items))
        .compile(
            &output,
            CompileRequest::new(output.document_version, PIPELINE),
        )
        .expect("exact item limit must compile");
    assert_eq!(accepted.scene().items().len(), expected_items);
    assert!(matches!(
        SceneCompiler::new(
            SceneLimits::default().with_max_scene_items(expected_items - 1)
        )
        .compile(
            &output,
            CompileRequest::new(output.document_version, PIPELINE)
        ),
        Err(SceneBuildError::ResourceLimitExceeded {
            resource: ResourceKind::SceneItems,
            observed,
            limit,
        }) if observed == expected_items && limit == expected_items - 1
    ));
}

#[test]
fn viewport_clip_is_local_to_root_space_for_out_of_viewport_bounds() {
    let (_, mut output) = parsed_layout("<html data-bg=red><body>x</body></html>");
    let root = output.root.expect("root").index();
    let root_fragment = &mut output.boxes[root].fragments[0];
    root_fragment.rect.origin.x = Au::from_px(-60);
    root_fragment.rect.origin.y = Au::from_px(-20);
    root_fragment.rect.size.width = Au::from_px(500);
    root_fragment.rect.size.height = Au::from_px(300);
    let compiled = compile(&output);
    let mut iterator = compiled.built_display_list().iter();
    let mut saw_outside_rectangle = false;

    while let Some(item) = iterator.next() {
        if let DisplayItem::Rectangle(rectangle) = *item.item() {
            assert_eq!(
                rectangle.common.spatial_id,
                SpatialId::root_scroll_node(PipelineId(7, 11))
            );
            assert_ne!(rectangle.common.clip_chain_id, ClipChainId::INVALID);
            assert_close(rectangle.common.clip_rect.min.x, 0.0);
            assert_close(rectangle.common.clip_rect.min.y, 0.0);
            assert_close(rectangle.common.clip_rect.width(), 320.0);
            assert_close(rectangle.common.clip_rect.height(), 180.0);
            assert_close(rectangle.bounds.min.x, -60.0);
            assert_close(rectangle.bounds.min.y, -20.0);
            assert_close(rectangle.bounds.width(), 500.0);
            assert_close(rectangle.bounds.height(), 300.0);
            saw_outside_rectangle = true;
            break;
        }
    }
    assert!(saw_outside_rectangle);
}

#[test]
fn rejects_boxes_when_root_is_absent() {
    let (_, mut output) = parsed_layout("<body>x</body>");
    let count = output.boxes.len();
    output.root = None;
    assert_eq!(
        expect_error(SceneCompiler::default().compile(
            &output,
            CompileRequest::new(output.document_version, PIPELINE)
        )),
        SceneBuildError::BoxesWithoutRoot { boxes: count }
    );
}

#[test]
fn resolves_positioned_text_with_first_baseline_and_preserves_paint_order() {
    let (_, output) = parsed_layout("<body data-bg=red data-border>baseline</body>");
    let compiled = compile(&output);
    assert_eq!(compiled.scene().pending_text().len(), 1);
    let pending = &compiled.scene().pending_text()[0];
    let metrics = scene_text_metrics(pending);
    assert_eq!(
        metrics.above_baseline().to_bits(),
        metrics.first_baseline().to_bits()
    );
    assert_eq!(
        metrics.below_baseline().to_bits(),
        (metrics.height() - metrics.first_baseline()).to_bits()
    );
    let map = compiled
        .validate_text_map(&[descriptor(output.document_version, pending)])
        .expect("exact text identity and first-baseline metrics must map");
    let mut resolution = map.begin_resolution(FONT_NAMESPACE).unwrap();
    let local_glyph = GlyphInstance {
        index: 37,
        point: webrender_api::units::LayoutPoint::new(2.0, metrics.first_baseline()),
    };
    resolution
        .resolve_next(
            output.document_version,
            pending.id().index(),
            std::iter::once((FontInstanceKey(FONT_NAMESPACE, 1), &[local_glyph][..])),
        )
        .unwrap();
    let resolved = resolution.finish().unwrap();
    let composed = compiled.compose_text(resolved).unwrap();

    assert!(composed.scene().pending_text().is_empty());
    assert_eq!(composed.scene().resolved_text_count(), 1);
    let text = composed
        .scene()
        .items()
        .iter()
        .find_map(|item| match item {
            SceneItem::Text(text) => Some(text),
            _ => None,
        })
        .expect("pending item must be replaced in place");
    let glyph = text.glyph_runs()[0].glyphs()[0];
    assert_close(glyph.x(), css_px(text.rect().x()) + 2.0);
    assert_close(
        glyph.y(),
        css_px(text.rect().y()) + metrics.first_baseline(),
    );

    let kinds = display_item_kinds(composed.built_display_list());
    let border_index = kinds.iter().position(|kind| *kind == "border").unwrap();
    let text_index = kinds.iter().position(|kind| *kind == "text").unwrap();
    assert!(
        border_index < text_index,
        "text must retain its scene paint slot"
    );
}

#[test]
fn rejects_noncanonical_scene_text_identity_before_resolution() {
    let (_, output) = parsed_layout("<div>alpha</div><p>beta</p>");
    let compiled = compile(&output);
    let pending = compiled.scene().pending_text();
    assert_eq!(pending.len(), 2);
    let first = descriptor(output.document_version, &pending[0]);
    let second = descriptor(output.document_version, &pending[1]);

    assert!(matches!(
        compiled.validate_text_map(&[first]),
        Err(SceneBuildError::MissingTextResolution { pending_index: 1 })
    ));
    assert!(matches!(
        compiled.validate_text_map(&[first, first]),
        Err(SceneBuildError::DuplicateTextResolution { pending_index: 0 })
    ));
    let unknown =
        SceneTextDescriptor::new(output.document_version, 2, first.text(), first.metrics());
    assert!(matches!(
        compiled.validate_text_map(&[first, second, unknown]),
        Err(SceneBuildError::UnknownTextResolution {
            pending_index: 2,
            available: 2
        })
    ));
    assert!(matches!(
        compiled.validate_text_map(&[second, first]),
        Err(SceneBuildError::OutOfOrderTextResolution {
            expected: 0,
            actual: 1
        })
    ));
    let next_version = DocumentVersion::new(
        output.document_version.document_id(),
        output.document_version.revision() + 1,
    );
    let wrong_version = SceneTextDescriptor::new(
        next_version,
        first.pending_index(),
        first.text(),
        first.metrics(),
    );
    assert!(matches!(
        compiled.validate_text_map(&[wrong_version, second]),
        Err(SceneBuildError::DocumentVersionMismatch {
            expected,
            actual
        }) if expected == output.document_version && actual == next_version
    ));
    let wrong_text = SceneTextDescriptor::new(
        output.document_version,
        first.pending_index(),
        "not alpha",
        first.metrics(),
    );
    assert!(matches!(
        compiled.validate_text_map(&[wrong_text, second]),
        Err(SceneBuildError::TextContentMismatch { pending_index: 0 })
    ));
}

#[test]
fn resolution_is_bound_to_one_compiled_scene_and_remains_usable_after_rejection() {
    let (_, source_output) = parsed_layout("<body>alpha</body>");
    let (_, mut target_output) = parsed_layout("<body>bravo</body>");
    // Exercise the stronger per-compilation binding rather than relying on the
    // public document-version check. The equal-length strings make every
    // retained slot field match while the pending UTF-8 differs.
    target_output.document_version = source_output.document_version;
    let source = compile(&source_output);
    let target = compile(&target_output);
    let source_pending = &source.scene().pending_text()[0];
    let target_pending = &target.scene().pending_text()[0];
    assert_ne!(source_pending.text(), target_pending.text());
    assert_eq!(source_pending.id(), target_pending.id());
    assert_eq!(source_pending.item_id(), target_pending.item_id());
    assert_eq!(source_pending.source_box(), target_pending.source_box());
    assert_eq!(source_pending.rect(), target_pending.rect());
    assert_eq!(source_pending.color(), target_pending.color());
    assert_eq!(source_pending.spatial_root(), target_pending.spatial_root());
    assert_eq!(source_pending.clip(), target_pending.clip());

    let map = source
        .validate_text_map(&[descriptor(source_output.document_version, source_pending)])
        .unwrap();
    let mut builder = map.begin_resolution(FONT_NAMESPACE).unwrap();
    let glyph = GlyphInstance {
        index: 37,
        point: webrender_api::units::LayoutPoint::new(2.0, css_px(source_pending.baseline())),
    };
    builder
        .resolve_next(
            source_output.document_version,
            source_pending.id().index(),
            std::iter::once((FontInstanceKey(FONT_NAMESPACE, 1), &[glyph][..])),
        )
        .unwrap();
    let resolved = builder.finish().unwrap();

    assert!(matches!(
        target.compose_text(resolved.clone()),
        Err(SceneBuildError::TextResolutionSceneMismatch)
    ));
    let composed = source
        .compose_text(resolved)
        .expect("a rejected cross-scene use must not invalidate the source resolution");
    assert!(composed.scene().pending_text().is_empty());
    assert_eq!(composed.scene().resolved_text_count(), 1);
}

#[test]
fn rejects_text_metric_mismatch_overflow_and_aggregate_glyph_excess() {
    let (_, output) = parsed_layout("<body>bounded</body>");
    let compiled = compile(&output);
    let pending = &compiled.scene().pending_text()[0];
    let exact = scene_text_metrics(pending);
    let wrong_width = SceneTextDescriptor::new(
        output.document_version,
        pending.id().index(),
        pending.text(),
        SceneTextMetrics::new(
            exact.full_width() + 1.0,
            exact.height(),
            exact.first_baseline(),
            exact.font_size(),
            exact.line_height(),
        ),
    );
    assert!(matches!(
        compiled.validate_text_map(&[wrong_width]),
        Err(SceneBuildError::TextMetricMismatch {
            pending_index: 0,
            field: GeometryField::Width,
            ..
        })
    ));
    let wrong_baseline = SceneTextDescriptor::new(
        output.document_version,
        pending.id().index(),
        pending.text(),
        SceneTextMetrics::new(
            exact.full_width(),
            exact.height(),
            exact.first_baseline() + css_px(1),
            exact.font_size(),
            exact.line_height(),
        ),
    );
    assert!(matches!(
        compiled.validate_text_map(&[wrong_baseline]),
        Err(SceneBuildError::TextMetricMismatch {
            pending_index: 0,
            field: GeometryField::Baseline,
            ..
        })
    ));
    let overflow = SceneTextDescriptor::new(
        output.document_version,
        pending.id().index(),
        pending.text(),
        SceneTextMetrics::new(
            f32::MAX,
            exact.height(),
            exact.first_baseline(),
            exact.font_size(),
            exact.line_height(),
        ),
    );
    assert!(matches!(
        compiled.validate_text_map(&[overflow]),
        Err(SceneBuildError::GeometryOverflow {
            axis: GeometryField::Width,
            ..
        })
    ));

    let constrained = SceneCompiler::new(SceneLimits::default().with_max_resolved_glyphs(0))
        .compile(
            &output,
            CompileRequest::new(output.document_version, PIPELINE),
        )
        .unwrap();
    let pending = &constrained.scene().pending_text()[0];
    let map = constrained
        .validate_text_map(&[descriptor(output.document_version, pending)])
        .unwrap();
    let mut resolution = map.begin_resolution(FONT_NAMESPACE).unwrap();
    let glyph = GlyphInstance {
        index: 1,
        point: webrender_api::units::LayoutPoint::new(0.0, css_px(pending.baseline())),
    };
    assert!(matches!(
        resolution.resolve_next(
            output.document_version,
            0,
            std::iter::once((FontInstanceKey(FONT_NAMESPACE, 1), &[glyph][..]))
        ),
        Err(SceneBuildError::ResourceLimitExceeded {
            resource: ResourceKind::ResolvedGlyphs,
            observed: 1,
            limit: 0
        })
    ));
}

#[test]
fn failed_resolution_entry_is_transactional_and_retryable() {
    let (_, output) = parsed_layout("<body>retry</body>");
    let compiled = compile(&output);
    let pending = &compiled.scene().pending_text()[0];
    let map = compiled
        .validate_text_map(&[descriptor(output.document_version, pending)])
        .unwrap();
    let baseline = css_px(pending.baseline());
    let mut resolution = map.begin_resolution(FONT_NAMESPACE).unwrap();
    let valid = GlyphInstance {
        index: 1,
        point: webrender_api::units::LayoutPoint::new(0.0, baseline),
    };
    assert!(matches!(
        resolution.resolve_next(
            output.document_version,
            0,
            std::iter::once((FontInstanceKey(IdNamespace(100), 1), &[valid][..]))
        ),
        Err(SceneBuildError::FontInstanceNamespaceMismatch {
            expected: FONT_NAMESPACE,
            actual: IdNamespace(100)
        })
    ));
    let invalid = GlyphInstance {
        index: 1,
        point: webrender_api::units::LayoutPoint::new(f32::NAN, baseline),
    };
    assert!(matches!(
        resolution.resolve_next(
            output.document_version,
            0,
            std::iter::once((FontInstanceKey(FONT_NAMESPACE, 1), &[invalid][..]))
        ),
        Err(SceneBuildError::NonFiniteConversion {
            field: GeometryField::X,
            ..
        })
    ));

    let overflowing = GlyphInstance {
        index: 1,
        point: webrender_api::units::LayoutPoint::new(f32::MAX, baseline),
    };
    assert!(matches!(
        resolution.resolve_next(
            output.document_version,
            0,
            std::iter::once((FontInstanceKey(FONT_NAMESPACE, 1), &[overflowing][..]))
        ),
        Err(SceneBuildError::GeometryOverflow {
            axis: GeometryField::X,
            ..
        })
    ));

    resolution
        .resolve_next(
            output.document_version,
            0,
            std::iter::once((FontInstanceKey(FONT_NAMESPACE, 1), &[valid][..])),
        )
        .expect("a failed entry must not consume its slot or aggregate budget");
    let resolved = resolution.finish().unwrap();
    assert_eq!(resolved.renderer_namespace(), FONT_NAMESPACE);
    let composed = compiled.compose_text(resolved).unwrap();
    assert_eq!(composed.scene().resolved_text_count(), 1);
    assert!(composed.scene().pending_text().is_empty());
}
