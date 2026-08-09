use peek_poke::Poke;
use webrender_api::units::{LayoutRect, LayoutSideOffsets, LayoutSize};
use webrender_api::{
    BorderDetails, BorderRadius, BorderSide, BorderStyle, ClipId, ColorF, CommonItemProperties,
    DisplayItem, DisplayListBuilder, NormalBorder, PipelineId, SpaceAndClipInfo, SpatialTreeItem,
};
use wild_buzzard_dom::DocumentVersion;
use wild_buzzard_layout::{
    Au, BoxKind, Color as LayoutColor, Edges, Fragment, LayoutBox, LayoutOutput,
    Rect as LayoutRectAu,
};

use crate::contract::{
    AppUnitEdges, AppUnitRect, AppUnitSize, BackgroundPrimitive, BorderPrimitive, Color,
    CompiledScene, PendingTextId, PendingTextPrimitive, PendingTextRun, Scene, SceneItem,
    SceneItemId, SourceBoxId, SpatialRootId, ViewportClipId,
};
use crate::error::{GeometryField, ResourceKind, SceneBuildError};

const SPATIAL_ROOT: SpatialRootId = SpatialRootId(0);
const VIEWPORT_CLIP: ViewportClipId = ViewportClipId(0);

/// A caller-owned `WebRender` pipeline identity without exposing `WebRender` types
/// throughout the engine facade.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PipelineKey {
    source: u32,
    pipeline: u32,
}

impl PipelineKey {
    /// Creates a pipeline key from a process/source namespace and local key.
    #[must_use]
    pub const fn new(source: u32, pipeline: u32) -> Self {
        Self { source, pipeline }
    }

    /// Returns the source namespace.
    #[must_use]
    pub const fn source(self) -> u32 {
        self.source
    }

    /// Returns the source-local pipeline key.
    #[must_use]
    pub const fn pipeline(self) -> u32 {
        self.pipeline
    }

    const fn as_webrender(self) -> PipelineId {
        PipelineId(self.source, self.pipeline)
    }
}

/// Inputs that bind one exact document version to one renderer pipeline.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompileRequest {
    expected_document_version: DocumentVersion,
    pipeline: PipelineKey,
}

impl CompileRequest {
    /// Creates a compilation request.
    #[must_use]
    pub const fn new(expected_document_version: DocumentVersion, pipeline: PipelineKey) -> Self {
        Self {
            expected_document_version,
            pipeline,
        }
    }

    /// Returns the document identity and revision that must exactly match layout output.
    #[must_use]
    pub const fn expected_document_version(self) -> DocumentVersion {
        self.expected_document_version
    }

    /// Returns the destination pipeline.
    #[must_use]
    pub const fn pipeline(self) -> PipelineKey {
        self.pipeline
    }
}

/// Resource and geometry limits applied before `WebRender` serialization.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(clippy::struct_field_names)]
pub struct SceneLimits {
    max_boxes: usize,
    max_child_references: usize,
    max_fragments: usize,
    max_scene_items: usize,
    max_text_run_bytes: usize,
    max_total_text_bytes: usize,
    max_tree_depth: usize,
    max_webrender_bytes: usize,
    max_abs_app_units: i32,
}

impl Default for SceneLimits {
    fn default() -> Self {
        Self {
            max_boxes: 100_000,
            max_child_references: 200_000,
            max_fragments: 500_000,
            max_scene_items: 1_000_000,
            max_text_run_bytes: 1 << 20,
            max_total_text_bytes: 32 << 20,
            max_tree_depth: 4_096,
            max_webrender_bytes: 128 << 20,
            // One million CSS pixels at 60 app units per CSS pixel.
            max_abs_app_units: 60_000_000,
        }
    }
}

impl SceneLimits {
    /// Returns the maximum layout-box count.
    #[must_use]
    pub const fn max_boxes(self) -> usize {
        self.max_boxes
    }

    /// Returns the maximum child-reference count.
    #[must_use]
    pub const fn max_child_references(self) -> usize {
        self.max_child_references
    }

    /// Returns the maximum fragment count.
    #[must_use]
    pub const fn max_fragments(self) -> usize {
        self.max_fragments
    }

    /// Returns the maximum scene-item count.
    #[must_use]
    pub const fn max_scene_items(self) -> usize {
        self.max_scene_items
    }

    /// Returns the maximum UTF-8 byte count for one text run.
    #[must_use]
    pub const fn max_text_run_bytes(self) -> usize {
        self.max_text_run_bytes
    }

    /// Returns the maximum aggregate UTF-8 byte count.
    #[must_use]
    pub const fn max_total_text_bytes(self) -> usize {
        self.max_total_text_bytes
    }

    /// Returns the maximum box-tree depth.
    #[must_use]
    pub const fn max_tree_depth(self) -> usize {
        self.max_tree_depth
    }

    /// Returns the maximum serialized `WebRender` display-list byte count.
    #[must_use]
    pub const fn max_webrender_bytes(self) -> usize {
        self.max_webrender_bytes
    }

    /// Returns the maximum absolute geometry value in app units.
    #[must_use]
    pub const fn max_abs_app_units(self) -> i32 {
        self.max_abs_app_units
    }

    /// Replaces the maximum layout-box count.
    #[must_use]
    pub const fn with_max_boxes(mut self, limit: usize) -> Self {
        self.max_boxes = limit;
        self
    }

    /// Replaces the maximum child-reference count.
    #[must_use]
    pub const fn with_max_child_references(mut self, limit: usize) -> Self {
        self.max_child_references = limit;
        self
    }

    /// Replaces the maximum fragment count.
    #[must_use]
    pub const fn with_max_fragments(mut self, limit: usize) -> Self {
        self.max_fragments = limit;
        self
    }

    /// Replaces the maximum scene-item count.
    #[must_use]
    pub const fn with_max_scene_items(mut self, limit: usize) -> Self {
        self.max_scene_items = limit;
        self
    }

    /// Replaces the maximum UTF-8 byte count for one text run.
    #[must_use]
    pub const fn with_max_text_run_bytes(mut self, limit: usize) -> Self {
        self.max_text_run_bytes = limit;
        self
    }

    /// Replaces the maximum aggregate UTF-8 byte count.
    #[must_use]
    pub const fn with_max_total_text_bytes(mut self, limit: usize) -> Self {
        self.max_total_text_bytes = limit;
        self
    }

    /// Replaces the maximum box-tree depth.
    #[must_use]
    pub const fn with_max_tree_depth(mut self, limit: usize) -> Self {
        self.max_tree_depth = limit;
        self
    }

    /// Replaces the maximum serialized `WebRender` byte count.
    #[must_use]
    pub const fn with_max_webrender_bytes(mut self, limit: usize) -> Self {
        self.max_webrender_bytes = limit;
        self
    }

    /// Replaces the maximum absolute geometry value in app units.
    #[must_use]
    pub const fn with_max_abs_app_units(mut self, limit: i32) -> Self {
        self.max_abs_app_units = limit;
        self
    }
}

/// Validates and compiles immutable layout output.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SceneCompiler {
    limits: SceneLimits,
}

impl Default for SceneCompiler {
    fn default() -> Self {
        Self::new(SceneLimits::default())
    }
}

impl SceneCompiler {
    /// Creates a compiler with explicit resource limits.
    #[must_use]
    pub const fn new(limits: SceneLimits) -> Self {
        Self { limits }
    }

    /// Returns the active resource limits.
    #[must_use]
    pub const fn limits(self) -> SceneLimits {
        self.limits
    }

    /// Compiles one exact document version into a scene and `WebRender` list.
    ///
    /// # Errors
    ///
    /// Returns a structured error for document-version mismatch, malformed box graphs,
    /// invalid geometry, resource exhaustion, or invalid renderer identities.
    pub fn compile(
        &self,
        layout: &LayoutOutput,
        request: CompileRequest,
    ) -> Result<CompiledScene, SceneBuildError> {
        if layout.document_version != request.expected_document_version {
            return Err(SceneBuildError::DocumentVersionMismatch {
                expected: request.expected_document_version,
                actual: layout.document_version,
            });
        }
        if request.pipeline.as_webrender() == PipelineId::INVALID {
            return Err(SceneBuildError::InvalidPipeline);
        }
        if self.limits.max_abs_app_units < 0 {
            return Err(SceneBuildError::GeometryOutOfRange {
                box_index: None,
                field: GeometryField::Width,
                value: self.limits.max_abs_app_units,
                limit: 0,
            });
        }

        let validated = validate_layout(layout, self.limits)?;
        let preflight_bytes = preflight_webrender_bytes(validated.webrender_primitive_count)?;
        enforce_limit(
            ResourceKind::WebRenderBytes,
            preflight_bytes,
            self.limits.max_webrender_bytes,
        )?;
        let scene = build_scene(layout, &validated)?;
        let pipeline_id = request.pipeline.as_webrender();
        let display_list = build_webrender_list(&scene, pipeline_id)?;
        enforce_limit(
            ResourceKind::WebRenderBytes,
            display_list.size_in_bytes(),
            self.limits.max_webrender_bytes,
        )?;

        Ok(CompiledScene {
            scene,
            pipeline_id,
            display_list,
        })
    }
}

struct ValidatedLayout {
    paint_order: Vec<usize>,
    viewport: AppUnitSize,
    content_size: AppUnitSize,
    scene_item_count: usize,
    pending_text_count: usize,
    webrender_primitive_count: usize,
}

#[derive(Default)]
struct ValidationCounts {
    child_references: usize,
    fragments: usize,
    scene_items: usize,
    pending_text: usize,
    webrender_primitives: usize,
    total_text_bytes: usize,
}

fn validate_layout(
    layout: &LayoutOutput,
    limits: SceneLimits,
) -> Result<ValidatedLayout, SceneBuildError> {
    enforce_limit(ResourceKind::Boxes, layout.boxes.len(), limits.max_boxes)?;

    let viewport = validate_size(
        layout.viewport.size.width,
        layout.viewport.size.height,
        true,
        None,
        limits,
    )?;
    let content_size = validate_size(
        layout.content_size.width,
        layout.content_size.height,
        false,
        None,
        limits,
    )?;

    let Some(root) = layout.root else {
        if layout.boxes.is_empty() {
            return Ok(ValidatedLayout {
                paint_order: Vec::new(),
                viewport,
                content_size,
                scene_item_count: 0,
                pending_text_count: 0,
                webrender_primitive_count: 0,
            });
        }
        return Err(SceneBuildError::BoxesWithoutRoot {
            boxes: layout.boxes.len(),
        });
    };
    let root_index = root.index();
    if root_index >= layout.boxes.len() {
        return Err(SceneBuildError::MissingRootBox {
            box_index: root_index,
        });
    }

    let mut incoming = vec![0_u32; layout.boxes.len()];
    let mut counts = ValidationCounts::default();
    for (slot, layout_box) in layout.boxes.iter().enumerate() {
        validate_box(layout, layout_box, slot, &mut incoming, &mut counts, limits)?;
    }
    let paint_order = validate_graph(layout, root_index, &incoming, limits)?;

    Ok(ValidatedLayout {
        paint_order,
        viewport,
        content_size,
        scene_item_count: counts.scene_items,
        pending_text_count: counts.pending_text,
        webrender_primitive_count: counts.webrender_primitives,
    })
}

fn validate_box(
    layout: &LayoutOutput,
    layout_box: &LayoutBox,
    slot: usize,
    incoming: &mut [u32],
    counts: &mut ValidationCounts,
    limits: SceneLimits,
) -> Result<(), SceneBuildError> {
    if layout_box.id.index() != slot {
        return Err(SceneBuildError::InvalidBoxIdentity {
            slot,
            reported: layout_box.id.index(),
        });
    }
    if matches!(layout_box.kind, BoxKind::Text | BoxKind::LineBreak)
        && !layout_box.children.is_empty()
    {
        return Err(SceneBuildError::LeafHasChildren { box_index: slot });
    }
    counts.child_references = checked_resource_add(
        ResourceKind::ChildReferences,
        counts.child_references,
        layout_box.children.len(),
        limits.max_child_references,
    )?;
    counts.fragments = checked_resource_add(
        ResourceKind::Fragments,
        counts.fragments,
        layout_box.fragments.len(),
        limits.max_fragments,
    )?;

    validate_font_metric(
        layout_box.style.font_size,
        slot,
        GeometryField::FontSize,
        limits,
    )?;
    validate_font_metric(
        layout_box.style.line_height,
        slot,
        GeometryField::LineHeight,
        limits,
    )?;
    validate_children(layout, layout_box, slot, incoming)?;

    let border = validate_edges(layout_box.style.border, slot, limits)?;
    for (fragment_index, fragment) in layout_box.fragments.iter().enumerate() {
        validate_fragment(
            layout_box,
            fragment,
            fragment_index,
            slot,
            border,
            counts,
            limits,
        )?;
    }
    Ok(())
}

fn validate_children(
    layout: &LayoutOutput,
    layout_box: &LayoutBox,
    slot: usize,
    incoming: &mut [u32],
) -> Result<(), SceneBuildError> {
    for child in &layout_box.children {
        let child_index = child.index();
        if child_index >= layout.boxes.len() {
            return Err(SceneBuildError::MissingChildBox {
                parent: slot,
                child: child_index,
            });
        }
        incoming[child_index] =
            incoming[child_index]
                .checked_add(1)
                .ok_or(SceneBuildError::MultipleParents {
                    box_index: child_index,
                })?;
        if incoming[child_index] > 1 {
            return Err(SceneBuildError::MultipleParents {
                box_index: child_index,
            });
        }
    }
    Ok(())
}

fn validate_fragment(
    layout_box: &LayoutBox,
    fragment: &Fragment,
    fragment_index: usize,
    slot: usize,
    border: AppUnitEdges,
    counts: &mut ValidationCounts,
    limits: SceneLimits,
) -> Result<(), SceneBuildError> {
    validate_rect(fragment.rect, Some(slot), limits)?;
    let paints_decorations = paints_box_decorations(layout_box.kind);
    if paints_decorations && !color(layout_box.style.background_color).is_transparent() {
        counts.scene_items = checked_resource_add(
            ResourceKind::SceneItems,
            counts.scene_items,
            1,
            limits.max_scene_items,
        )?;
        counts.webrender_primitives = checked_counter_add(
            ResourceKind::SceneItems,
            counts.webrender_primitives,
            1,
            limits.max_scene_items,
        )?;
    }
    if paints_decorations && !border.is_zero() {
        counts.scene_items = checked_resource_add(
            ResourceKind::SceneItems,
            counts.scene_items,
            1,
            limits.max_scene_items,
        )?;
        counts.webrender_primitives = checked_counter_add(
            ResourceKind::SceneItems,
            counts.webrender_primitives,
            1,
            limits.max_scene_items,
        )?;
    }

    let Some(text) = &fragment.text else {
        return Ok(());
    };
    if layout_box.kind != BoxKind::Text {
        return Err(SceneBuildError::TextOnNonTextBox { box_index: slot });
    }
    let Some(baseline) = fragment.baseline else {
        return Err(SceneBuildError::TextMissingBaseline {
            box_index: slot,
            fragment_index,
        });
    };
    validate_baseline(baseline, fragment.rect.size.height, slot, limits)?;
    enforce_limit(
        ResourceKind::TextRunBytes,
        text.len(),
        limits.max_text_run_bytes,
    )?;
    counts.total_text_bytes = checked_resource_add(
        ResourceKind::TotalTextBytes,
        counts.total_text_bytes,
        text.len(),
        limits.max_total_text_bytes,
    )?;
    counts.scene_items = checked_resource_add(
        ResourceKind::SceneItems,
        counts.scene_items,
        1,
        limits.max_scene_items,
    )?;
    counts.pending_text = checked_counter_add(
        ResourceKind::PendingTextRuns,
        counts.pending_text,
        1,
        limits.max_scene_items,
    )?;
    Ok(())
}

fn validate_graph(
    layout: &LayoutOutput,
    root_index: usize,
    incoming: &[u32],
    limits: SceneLimits,
) -> Result<Vec<usize>, SceneBuildError> {
    if incoming[root_index] != 0 {
        return Err(SceneBuildError::RootHasParent {
            box_index: root_index,
        });
    }
    detect_cycles(layout)?;
    let paint_order = collect_paint_order(layout, root_index, limits.max_tree_depth)?;
    if paint_order.len() == layout.boxes.len() {
        return Ok(paint_order);
    }

    let mut seen = vec![false; layout.boxes.len()];
    for box_index in &paint_order {
        seen[*box_index] = true;
    }
    let box_index = seen.iter().position(|was_seen| !was_seen).unwrap_or(0);
    Err(SceneBuildError::UnreachableBox { box_index })
}

fn detect_cycles(layout: &LayoutOutput) -> Result<(), SceneBuildError> {
    let mut state = vec![0_u8; layout.boxes.len()];
    for start in 0..layout.boxes.len() {
        if state[start] != 0 {
            continue;
        }
        let mut stack = vec![(start, false)];
        while let Some((box_index, exiting)) = stack.pop() {
            if exiting {
                state[box_index] = 2;
                continue;
            }
            match state[box_index] {
                1 => return Err(SceneBuildError::BoxCycle { box_index }),
                2 => continue,
                _ => {}
            }
            state[box_index] = 1;
            stack.push((box_index, true));
            for child in layout.boxes[box_index].children.iter().rev() {
                stack.push((child.index(), false));
            }
        }
    }
    Ok(())
}

fn collect_paint_order(
    layout: &LayoutOutput,
    root: usize,
    max_depth: usize,
) -> Result<Vec<usize>, SceneBuildError> {
    let mut order = Vec::with_capacity(layout.boxes.len());
    let mut stack = vec![(root, 1_usize)];
    while let Some((box_index, depth)) = stack.pop() {
        enforce_limit(ResourceKind::TreeDepth, depth, max_depth)?;
        order.push(box_index);
        for child in layout.boxes[box_index].children.iter().rev() {
            stack.push((child.index(), depth.saturating_add(1)));
        }
    }
    Ok(order)
}

fn build_scene(
    layout: &LayoutOutput,
    validated: &ValidatedLayout,
) -> Result<Scene, SceneBuildError> {
    let mut items = Vec::new();
    let mut pending_text = Vec::new();
    items
        .try_reserve_exact(validated.scene_item_count)
        .map_err(|_| SceneBuildError::AllocationFailed {
            resource: ResourceKind::SceneItems,
            requested: validated.scene_item_count,
        })?;
    pending_text
        .try_reserve_exact(validated.pending_text_count)
        .map_err(|_| SceneBuildError::AllocationFailed {
            resource: ResourceKind::PendingTextRuns,
            requested: validated.pending_text_count,
        })?;

    for box_index in &validated.paint_order {
        let layout_box = &layout.boxes[*box_index];
        let source_box = SourceBoxId(
            u32::try_from(*box_index).map_err(|_| SceneBuildError::IdentifierCapacityExceeded)?,
        );
        let background = color(layout_box.style.background_color);
        let foreground = color(layout_box.style.color);
        let border = app_unit_edges(layout_box.style.border);

        for fragment in &layout_box.fragments {
            let rect = app_unit_rect(fragment.rect);
            if paints_box_decorations(layout_box.kind) && !background.is_transparent() {
                let id = next_item_id(items.len())?;
                items.push(SceneItem::Background(BackgroundPrimitive::new(
                    id,
                    source_box,
                    rect,
                    background,
                    SPATIAL_ROOT,
                    VIEWPORT_CLIP,
                )));
            }
            if paints_box_decorations(layout_box.kind) && !border.is_zero() {
                let id = next_item_id(items.len())?;
                items.push(SceneItem::Border(BorderPrimitive::new(
                    id,
                    source_box,
                    rect,
                    border,
                    foreground,
                    SPATIAL_ROOT,
                    VIEWPORT_CLIP,
                )));
            }
            if let Some(text) = &fragment.text {
                let item_id = next_item_id(items.len())?;
                let text_id = PendingTextId(
                    u32::try_from(pending_text.len())
                        .map_err(|_| SceneBuildError::IdentifierCapacityExceeded)?,
                );
                let baseline = fragment
                    .baseline
                    .ok_or(SceneBuildError::TextMissingBaseline {
                        box_index: *box_index,
                        fragment_index: 0,
                    })?
                    .raw();
                let mut owned_text = String::new();
                owned_text.try_reserve_exact(text.len()).map_err(|_| {
                    SceneBuildError::AllocationFailed {
                        resource: ResourceKind::TextRunBytes,
                        requested: text.len(),
                    }
                })?;
                owned_text.push_str(text);
                pending_text.push(PendingTextRun::new(
                    text_id,
                    item_id,
                    source_box,
                    rect,
                    baseline,
                    owned_text,
                    foreground,
                    layout_box.style.font_size.raw(),
                    layout_box.style.line_height.raw(),
                    SPATIAL_ROOT,
                    VIEWPORT_CLIP,
                ));
                items.push(SceneItem::PendingText(PendingTextPrimitive::new(
                    item_id, text_id,
                )));
            }
        }
    }

    Ok(Scene::new(
        layout.document_version,
        validated.viewport,
        validated.content_size,
        SPATIAL_ROOT,
        VIEWPORT_CLIP,
        items,
        pending_text,
    ))
}

fn build_webrender_list(
    scene: &Scene,
    pipeline_id: PipelineId,
) -> Result<webrender_api::BuiltDisplayList, SceneBuildError> {
    let viewport = webrender_rect(
        AppUnitRect::new(0, 0, scene.viewport().width(), scene.viewport().height()),
        None,
    )?;
    let root = SpaceAndClipInfo::root_scroll(pipeline_id);
    let mut builder = DisplayListBuilder::new(pipeline_id);
    builder.begin();
    let clip_id = builder.define_clip_rect(root.spatial_id, viewport);
    let clip_chain_id = builder.define_clip_chain(None, [clip_id]);
    let space_and_clip = SpaceAndClipInfo {
        spatial_id: root.spatial_id,
        clip_chain_id,
    };
    let common = CommonItemProperties::new(viewport, space_and_clip);

    for item in scene.items() {
        match item {
            SceneItem::Background(background) => {
                let bounds = webrender_rect(background.rect(), Some(background.source_box()))?;
                builder.push_rect(&common, bounds, webrender_color(background.color()));
            }
            SceneItem::Border(border) => {
                let bounds = webrender_rect(border.rect(), Some(border.source_box()))?;
                let color = webrender_color(border.color());
                let side = BorderSide {
                    color,
                    style: BorderStyle::Solid,
                };
                let details = BorderDetails::Normal(NormalBorder {
                    left: side,
                    right: side,
                    top: side,
                    bottom: side,
                    radius: BorderRadius::default(),
                    do_aa: false,
                });
                builder.push_border(
                    &common,
                    bounds,
                    webrender_edges(border.widths(), Some(border.source_box()))?,
                    details,
                );
            }
            SceneItem::PendingText(_) => {}
        }
    }
    let (_, display_list) = builder.end();
    Ok(display_list)
}

fn next_item_id(index: usize) -> Result<SceneItemId, SceneBuildError> {
    Ok(SceneItemId(u32::try_from(index).map_err(|_| {
        SceneBuildError::IdentifierCapacityExceeded
    })?))
}

fn preflight_webrender_bytes(primitive_count: usize) -> Result<usize, SceneBuildError> {
    // RectClip + ClipChain + primitives + the final DisplayItem red zone.
    let display_items = checked_unbounded_add(primitive_count, 3)?;
    let display_bytes = checked_unbounded_mul(display_items, DisplayItem::max_size())?;
    // Clip-chain array: byte-size, item-count, one ClipId, and one ClipId red zone.
    let clip_array_bytes = checked_unbounded_add(
        checked_unbounded_mul(2, std::mem::size_of::<usize>())?,
        checked_unbounded_mul(2, ClipId::max_size())?,
    )?;
    // No explicit spatial item is emitted, but `end()` appends its red zone.
    checked_unbounded_add(
        checked_unbounded_add(display_bytes, clip_array_bytes)?,
        SpatialTreeItem::max_size(),
    )
}

const fn paints_box_decorations(kind: BoxKind) -> bool {
    matches!(kind, BoxKind::Block | BoxKind::Inline)
}

fn validate_size(
    width: Au,
    height: Au,
    strictly_positive: bool,
    box_index: Option<usize>,
    limits: SceneLimits,
) -> Result<AppUnitSize, SceneBuildError> {
    let width_raw = width.raw();
    let height_raw = height.raw();
    if (strictly_positive && width_raw <= 0) || (!strictly_positive && width_raw < 0) {
        return Err(SceneBuildError::NegativeGeometry {
            box_index,
            field: GeometryField::Width,
            value: width_raw,
        });
    }
    if (strictly_positive && height_raw <= 0) || (!strictly_positive && height_raw < 0) {
        return Err(SceneBuildError::NegativeGeometry {
            box_index,
            field: GeometryField::Height,
            value: height_raw,
        });
    }
    validate_range(
        width_raw,
        box_index,
        GeometryField::Width,
        limits.max_abs_app_units,
    )?;
    validate_range(
        height_raw,
        box_index,
        GeometryField::Height,
        limits.max_abs_app_units,
    )?;
    finite_css_pixels(width_raw, box_index, GeometryField::Width)?;
    finite_css_pixels(height_raw, box_index, GeometryField::Height)?;
    Ok(AppUnitSize::new(width_raw, height_raw))
}

fn validate_rect(
    rect: LayoutRectAu,
    box_index: Option<usize>,
    limits: SceneLimits,
) -> Result<(), SceneBuildError> {
    let x = rect.origin.x.raw();
    let y = rect.origin.y.raw();
    let width = rect.size.width.raw();
    let height = rect.size.height.raw();
    if width < 0 {
        return Err(SceneBuildError::NegativeGeometry {
            box_index,
            field: GeometryField::Width,
            value: width,
        });
    }
    if height < 0 {
        return Err(SceneBuildError::NegativeGeometry {
            box_index,
            field: GeometryField::Height,
            value: height,
        });
    }
    for (field, value) in [
        (GeometryField::X, x),
        (GeometryField::Y, y),
        (GeometryField::Width, width),
        (GeometryField::Height, height),
    ] {
        validate_range(value, box_index, field, limits.max_abs_app_units)?;
        finite_css_pixels(value, box_index, field)?;
    }
    x.checked_add(width)
        .ok_or(SceneBuildError::GeometryOverflow {
            box_index,
            axis: GeometryField::X,
        })?;
    y.checked_add(height)
        .ok_or(SceneBuildError::GeometryOverflow {
            box_index,
            axis: GeometryField::Y,
        })?;
    Ok(())
}

fn validate_edges(
    edges: Edges,
    box_index: usize,
    limits: SceneLimits,
) -> Result<AppUnitEdges, SceneBuildError> {
    let values = [
        (GeometryField::Top, edges.top.raw()),
        (GeometryField::Right, edges.right.raw()),
        (GeometryField::Bottom, edges.bottom.raw()),
        (GeometryField::Left, edges.left.raw()),
    ];
    for (field, value) in values {
        if value < 0 {
            return Err(SceneBuildError::NegativeGeometry {
                box_index: Some(box_index),
                field,
                value,
            });
        }
        validate_range(value, Some(box_index), field, limits.max_abs_app_units)?;
        finite_css_pixels(value, Some(box_index), field)?;
    }
    Ok(app_unit_edges(edges))
}

fn validate_font_metric(
    metric: Au,
    box_index: usize,
    field: GeometryField,
    limits: SceneLimits,
) -> Result<(), SceneBuildError> {
    let value = metric.raw();
    if value <= 0 {
        return Err(SceneBuildError::InvalidFontMetric {
            box_index,
            field,
            value,
        });
    }
    validate_range(value, Some(box_index), field, limits.max_abs_app_units)?;
    finite_css_pixels(value, Some(box_index), field).map(|_| ())
}

fn validate_baseline(
    baseline: Au,
    height: Au,
    box_index: usize,
    limits: SceneLimits,
) -> Result<(), SceneBuildError> {
    let value = baseline.raw();
    if value < 0 {
        return Err(SceneBuildError::NegativeGeometry {
            box_index: Some(box_index),
            field: GeometryField::Baseline,
            value,
        });
    }
    if value > height.raw() {
        return Err(SceneBuildError::GeometryOutOfRange {
            box_index: Some(box_index),
            field: GeometryField::Baseline,
            value,
            limit: height.raw(),
        });
    }
    validate_range(
        value,
        Some(box_index),
        GeometryField::Baseline,
        limits.max_abs_app_units,
    )?;
    finite_css_pixels(value, Some(box_index), GeometryField::Baseline).map(|_| ())
}

fn validate_range(
    value: i32,
    box_index: Option<usize>,
    field: GeometryField,
    limit: i32,
) -> Result<(), SceneBuildError> {
    if value < -limit || value > limit {
        return Err(SceneBuildError::GeometryOutOfRange {
            box_index,
            field,
            value,
            limit,
        });
    }
    Ok(())
}

fn finite_css_pixels(
    value: i32,
    box_index: Option<usize>,
    field: GeometryField,
) -> Result<f32, SceneBuildError> {
    let converted = webrender_api::units::Au(value).to_f32_px();
    if converted.is_finite() {
        Ok(converted)
    } else {
        Err(SceneBuildError::NonFiniteConversion { box_index, field })
    }
}

fn app_unit_rect(rect: LayoutRectAu) -> AppUnitRect {
    AppUnitRect::new(
        rect.origin.x.raw(),
        rect.origin.y.raw(),
        rect.size.width.raw(),
        rect.size.height.raw(),
    )
}

const fn app_unit_edges(edges: Edges) -> AppUnitEdges {
    AppUnitEdges::new(
        edges.top.raw(),
        edges.right.raw(),
        edges.bottom.raw(),
        edges.left.raw(),
    )
}

const fn color(layout_color: LayoutColor) -> Color {
    Color::new(
        layout_color.red,
        layout_color.green,
        layout_color.blue,
        layout_color.alpha,
    )
}

fn webrender_rect(
    rect: AppUnitRect,
    source: Option<SourceBoxId>,
) -> Result<LayoutRect, SceneBuildError> {
    let box_index = source.map(|id| id.index() as usize);
    let x = finite_css_pixels(rect.x(), box_index, GeometryField::X)?;
    let y = finite_css_pixels(rect.y(), box_index, GeometryField::Y)?;
    let width = finite_css_pixels(rect.width(), box_index, GeometryField::Width)?;
    let height = finite_css_pixels(rect.height(), box_index, GeometryField::Height)?;
    Ok(LayoutRect::from_origin_and_size(
        webrender_api::units::LayoutPoint::new(x, y),
        LayoutSize::new(width, height),
    ))
}

fn webrender_edges(
    edges: AppUnitEdges,
    source: Option<SourceBoxId>,
) -> Result<LayoutSideOffsets, SceneBuildError> {
    let box_index = source.map(|id| id.index() as usize);
    Ok(LayoutSideOffsets::new(
        finite_css_pixels(edges.top(), box_index, GeometryField::Top)?,
        finite_css_pixels(edges.right(), box_index, GeometryField::Right)?,
        finite_css_pixels(edges.bottom(), box_index, GeometryField::Bottom)?,
        finite_css_pixels(edges.left(), box_index, GeometryField::Left)?,
    ))
}

fn webrender_color(color: Color) -> ColorF {
    const CHANNEL_MAX: f32 = 255.0;
    ColorF::new(
        f32::from(color.red()) / CHANNEL_MAX,
        f32::from(color.green()) / CHANNEL_MAX,
        f32::from(color.blue()) / CHANNEL_MAX,
        f32::from(color.alpha()) / CHANNEL_MAX,
    )
}

fn enforce_limit(
    resource: ResourceKind,
    observed: usize,
    limit: usize,
) -> Result<(), SceneBuildError> {
    if observed > limit {
        Err(SceneBuildError::ResourceLimitExceeded {
            resource,
            observed,
            limit,
        })
    } else {
        Ok(())
    }
}

fn checked_resource_add(
    resource: ResourceKind,
    current: usize,
    additional: usize,
    limit: usize,
) -> Result<usize, SceneBuildError> {
    let observed =
        current
            .checked_add(additional)
            .ok_or(SceneBuildError::ResourceLimitExceeded {
                resource,
                observed: usize::MAX,
                limit,
            })?;
    enforce_limit(resource, observed, limit)?;
    Ok(observed)
}

fn checked_counter_add(
    resource: ResourceKind,
    current: usize,
    additional: usize,
    limit: usize,
) -> Result<usize, SceneBuildError> {
    checked_resource_add(resource, current, additional, limit)
}

fn checked_unbounded_add(left: usize, right: usize) -> Result<usize, SceneBuildError> {
    left.checked_add(right)
        .ok_or(SceneBuildError::ResourceLimitExceeded {
            resource: ResourceKind::WebRenderBytes,
            observed: usize::MAX,
            limit: usize::MAX,
        })
}

fn checked_unbounded_mul(left: usize, right: usize) -> Result<usize, SceneBuildError> {
    left.checked_mul(right)
        .ok_or(SceneBuildError::ResourceLimitExceeded {
            resource: ResourceKind::WebRenderBytes,
            observed: usize::MAX,
            limit: usize::MAX,
        })
}
