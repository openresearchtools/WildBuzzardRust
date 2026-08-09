use peek_poke::Poke;
use std::sync::atomic::{AtomicU64, Ordering};
use webrender_api::units::{LayoutRect, LayoutSideOffsets, LayoutSize};
use webrender_api::{
    BorderDetails, BorderRadius, BorderSide, BorderStyle, ClipId, ColorF, CommonItemProperties,
    DisplayItem, DisplayListBuilder, FontInstanceKey, GlyphInstance, NormalBorder, PipelineId,
    SpaceAndClipInfo, SpatialTreeItem,
};
use wild_buzzard_dom::DocumentVersion;
use wild_buzzard_layout::{
    Au, BoxKind, Color as LayoutColor, Edges, Fragment, LayoutBox, LayoutOutput,
    Rect as LayoutRectAu,
};

use crate::contract::{
    AppUnitEdges, AppUnitRect, AppUnitSize, BackgroundPrimitive, BorderPrimitive, Color,
    CompiledScene, PendingTextId, PendingTextPrimitive, PendingTextRun, ResolvedGlyph,
    ResolvedGlyphRun, ResolvedTextPrimitive, ResolvedTextSet, Scene, SceneItem, SceneItemId,
    SceneResolutionIdentity, SceneTextDescriptor, SceneTextMetrics, SourceBoxId, SpatialRootId,
    TextResolutionBuilder, ValidatedTextMap, ValidatedTextSlot, ViewportClipId,
};
use crate::error::{GeometryField, ResourceKind, SceneBuildError};

const SPATIAL_ROOT: SpatialRootId = SpatialRootId(0);
const VIEWPORT_CLIP: ViewportClipId = ViewportClipId(0);
static NEXT_SCENE_RESOLUTION_IDENTITY: AtomicU64 = AtomicU64::new(1);

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
    max_resolved_glyph_runs: usize,
    max_resolved_glyphs: usize,
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
            max_resolved_glyph_runs: 100_000,
            max_resolved_glyphs: 1_000_000,
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

    /// Returns the maximum aggregate resolved font/glyph-run count.
    #[must_use]
    pub const fn max_resolved_glyph_runs(self) -> usize {
        self.max_resolved_glyph_runs
    }

    /// Returns the maximum aggregate positioned-glyph count.
    #[must_use]
    pub const fn max_resolved_glyphs(self) -> usize {
        self.max_resolved_glyphs
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

    /// Replaces the maximum aggregate resolved font/glyph-run count.
    #[must_use]
    pub const fn with_max_resolved_glyph_runs(mut self, limit: usize) -> Self {
        self.max_resolved_glyph_runs = limit;
        self
    }

    /// Replaces the maximum aggregate positioned-glyph count.
    #[must_use]
    pub const fn with_max_resolved_glyphs(mut self, limit: usize) -> Self {
        self.max_resolved_glyphs = limit;
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
    /// Returns a structured error for document-version mismatch, malformed box
    /// graphs, invalid geometry, resource or scene-identity exhaustion, or
    /// invalid renderer identities.
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

        let scene_resolution_identity = allocate_scene_resolution_identity()?;
        Ok(CompiledScene {
            scene_resolution_identity,
            scene,
            pipeline_id,
            display_list,
            limits: self.limits,
        })
    }
}

impl CompiledScene {
    /// Validates an exact, canonical shaped-text inventory against this scene.
    ///
    /// Validation is completed before any `WebRender` font key is generated or a
    /// live font registry is changed. Every descriptor must carry this scene's
    /// exact document version, appear once in pending-index order, preserve the
    /// UTF-8 bytes, and quantize to the exact layout metrics.
    ///
    /// # Errors
    ///
    /// Rejects wrong versions, missing, duplicate, unknown, or out-of-order
    /// indices, text/metric mismatches, non-finite values, overflow, resource
    /// excess, and fallible allocation failure.
    pub fn validate_text_map(
        &self,
        descriptors: &[SceneTextDescriptor<'_>],
    ) -> Result<ValidatedTextMap, SceneBuildError> {
        let pending = self.scene.pending_text();
        enforce_limit(
            ResourceKind::PendingTextRuns,
            descriptors.len(),
            self.limits.max_scene_items,
        )?;
        let mut seen = Vec::new();
        seen.try_reserve_exact(pending.len())
            .map_err(|_| SceneBuildError::AllocationFailed {
                resource: ResourceKind::PendingTextRuns,
                requested: pending.len(),
            })?;
        seen.resize(pending.len(), false);
        let mut slots = Vec::new();
        slots
            .try_reserve_exact(pending.len())
            .map_err(|_| SceneBuildError::AllocationFailed {
                resource: ResourceKind::PendingTextRuns,
                requested: pending.len(),
            })?;

        for (position, descriptor) in descriptors.iter().copied().enumerate() {
            if descriptor.document_version() != self.scene.document_version() {
                return Err(SceneBuildError::DocumentVersionMismatch {
                    expected: self.scene.document_version(),
                    actual: descriptor.document_version(),
                });
            }
            let observed = descriptor.pending_index();
            let observed_index = observed as usize;
            if observed_index >= pending.len() {
                return Err(SceneBuildError::UnknownTextResolution {
                    pending_index: observed,
                    available: pending.len(),
                });
            }
            if seen[observed_index] {
                return Err(SceneBuildError::DuplicateTextResolution {
                    pending_index: observed,
                });
            }
            let expected =
                u32::try_from(position).map_err(|_| SceneBuildError::IdentifierCapacityExceeded)?;
            if observed != expected {
                return Err(SceneBuildError::OutOfOrderTextResolution {
                    expected,
                    actual: observed,
                });
            }
            seen[observed_index] = true;
            let record = &pending[observed_index];
            if descriptor.text() != record.text() {
                return Err(SceneBuildError::TextContentMismatch {
                    pending_index: observed,
                });
            }
            validate_resolved_metrics(record, descriptor.metrics(), self.limits)?;
            slots.push(ValidatedTextSlot {
                pending_text: record.id(),
                item_id: record.item_id(),
                source_box: record.source_box(),
                rect: record.rect(),
                color: record.color(),
                spatial_root: record.spatial_root(),
                clip: record.clip(),
            });
        }

        if descriptors.len() < pending.len() {
            return Err(SceneBuildError::MissingTextResolution {
                pending_index: u32::try_from(descriptors.len())
                    .map_err(|_| SceneBuildError::IdentifierCapacityExceeded)?,
            });
        }

        Ok(ValidatedTextMap {
            scene_resolution_identity: self.scene_resolution_identity,
            document_version: self.scene.document_version(),
            slots,
            max_glyph_runs: self.limits.max_resolved_glyph_runs,
            max_glyphs: self.limits.max_resolved_glyphs,
            max_abs_app_units: self.limits.max_abs_app_units,
        })
    }

    /// Replaces every pending text item with one validated resolved primitive
    /// and rebuilds a single immutable display list in original paint order.
    ///
    /// # Errors
    ///
    /// Rejects a set from another compiled scene before mutation, followed by
    /// wrong document/namespace, incomplete resolution, serialized-size excess,
    /// geometry failure, or allocation failure.
    pub fn compose_text(mut self, resolved: ResolvedTextSet) -> Result<Self, SceneBuildError> {
        if resolved.scene_resolution_identity != self.scene_resolution_identity {
            return Err(SceneBuildError::TextResolutionSceneMismatch);
        }
        if resolved.document_version != self.scene.document_version() {
            return Err(SceneBuildError::DocumentVersionMismatch {
                expected: self.scene.document_version(),
                actual: resolved.document_version,
            });
        }
        if let Some(expected) = self.scene.renderer_namespace
            && resolved.renderer_namespace != expected
        {
            return Err(SceneBuildError::FontInstanceNamespaceMismatch {
                expected,
                actual: resolved.renderer_namespace,
            });
        }
        let renderer_namespace = resolved.renderer_namespace;
        if resolved.entries.len() < self.scene.pending_text.len() {
            return Err(SceneBuildError::MissingTextResolution {
                pending_index: u32::try_from(resolved.entries.len())
                    .map_err(|_| SceneBuildError::IdentifierCapacityExceeded)?,
            });
        }
        if resolved.entries.len() > self.scene.pending_text.len() {
            let entry = &resolved.entries[self.scene.pending_text.len()];
            return Err(SceneBuildError::UnknownTextResolution {
                pending_index: entry.pending_text().index(),
                available: self.scene.pending_text.len(),
            });
        }

        let mut resolved_entries = resolved.entries.into_iter();
        for item in &mut self.scene.items {
            let SceneItem::PendingText(pending_item) = item else {
                continue;
            };
            let Some(resolved_item) = resolved_entries.next() else {
                return Err(SceneBuildError::MissingTextResolution {
                    pending_index: pending_item.pending_text().index(),
                });
            };
            let Some(pending_record) = self
                .scene
                .pending_text
                .get(pending_item.pending_text().index() as usize)
            else {
                return Err(SceneBuildError::ResolvedTextItemMismatch {
                    pending_index: pending_item.pending_text().index(),
                });
            };
            if resolved_item.pending_text() != pending_item.pending_text()
                || resolved_item.id() != pending_item.id()
                || resolved_item.id() != pending_record.item_id()
                || resolved_item.source_box() != pending_record.source_box()
                || resolved_item.rect() != pending_record.rect()
                || resolved_item.color() != pending_record.color()
                || resolved_item.spatial_root() != pending_record.spatial_root()
                || resolved_item.clip() != pending_record.clip()
            {
                return Err(SceneBuildError::ResolvedTextItemMismatch {
                    pending_index: pending_item.pending_text().index(),
                });
            }
            *item = SceneItem::Text(resolved_item);
        }
        if let Some(extra) = resolved_entries.next() {
            return Err(SceneBuildError::UnknownTextResolution {
                pending_index: extra.pending_text().index(),
                available: self.scene.pending_text.len(),
            });
        }
        self.scene.pending_text.clear();
        self.scene.renderer_namespace = Some(renderer_namespace);

        let (glyph_runs, glyphs) = resolved_counts(&self.scene)?;
        enforce_limit(
            ResourceKind::ResolvedGlyphRuns,
            glyph_runs,
            self.limits.max_resolved_glyph_runs,
        )?;
        enforce_limit(
            ResourceKind::ResolvedGlyphs,
            glyphs,
            self.limits.max_resolved_glyphs,
        )?;
        let preflight = preflight_composed_webrender_bytes(&self.scene, glyph_runs, glyphs)?;
        enforce_limit(
            ResourceKind::WebRenderBytes,
            preflight,
            self.limits.max_webrender_bytes,
        )?;
        self.display_list = build_webrender_list(&self.scene, self.pipeline_id)?;
        enforce_limit(
            ResourceKind::WebRenderBytes,
            self.display_list.size_in_bytes(),
            self.limits.max_webrender_bytes,
        )?;
        Ok(self)
    }
}

impl ValidatedTextMap {
    /// Begins checked resolution for one actual `WebRender` namespace and local
    /// glyph positions. The returned builder rejects every key from another
    /// namespace and cannot finish until every mapped entry is supplied exactly
    /// once in canonical order. Namespace equality is not registry-membership
    /// authority; the renderer owner must still supply keys from its registry.
    ///
    /// # Errors
    ///
    /// Returns a structured allocation failure without consuming font-registry
    /// state.
    pub fn begin_resolution(
        self,
        renderer_namespace: webrender_api::IdNamespace,
    ) -> Result<TextResolutionBuilder, SceneBuildError> {
        let mut entries = Vec::new();
        entries.try_reserve_exact(self.slots.len()).map_err(|_| {
            SceneBuildError::AllocationFailed {
                resource: ResourceKind::PendingTextRuns,
                requested: self.slots.len(),
            }
        })?;
        Ok(TextResolutionBuilder {
            scene_resolution_identity: self.scene_resolution_identity,
            document_version: self.document_version,
            renderer_namespace,
            slots: self.slots,
            next_slot: 0,
            entries,
            glyph_runs: 0,
            glyphs: 0,
            max_glyph_runs: self.max_glyph_runs,
            max_glyphs: self.max_glyphs,
            max_abs_app_units: self.max_abs_app_units,
        })
    }
}

impl TextResolutionBuilder {
    /// Resolves the next canonical text entry from local shaped glyphs.
    ///
    /// Glyph coordinates are relative to the top of the shaped line. Their Y
    /// values already include Parley's `first_baseline`; this method adds only
    /// the pending fragment's top edge. Font ascent is never added.
    ///
    /// # Errors
    ///
    /// Rejects a wrong document, missing/duplicate/unknown/out-of-order index,
    /// aggregate limit excess, non-finite/overflowing coordinates, or fallible
    /// allocation failure.
    #[allow(clippy::too_many_lines)]
    pub fn resolve_next<'glyph, I>(
        &mut self,
        document_version: DocumentVersion,
        pending_index: u32,
        runs: I,
    ) -> Result<(), SceneBuildError>
    where
        I: IntoIterator<Item = (FontInstanceKey, &'glyph [GlyphInstance])>,
    {
        if document_version != self.document_version {
            return Err(SceneBuildError::DocumentVersionMismatch {
                expected: self.document_version,
                actual: document_version,
            });
        }
        let observed = pending_index as usize;
        if observed >= self.slots.len() {
            return Err(SceneBuildError::UnknownTextResolution {
                pending_index,
                available: self.slots.len(),
            });
        }
        if observed < self.next_slot {
            return Err(SceneBuildError::DuplicateTextResolution { pending_index });
        }
        let expected = u32::try_from(self.next_slot)
            .map_err(|_| SceneBuildError::IdentifierCapacityExceeded)?;
        if pending_index != expected {
            return Err(SceneBuildError::OutOfOrderTextResolution {
                expected,
                actual: pending_index,
            });
        }

        let slot = &self.slots[self.next_slot];
        let origin_x = finite_css_pixels(
            slot.rect.x(),
            Some(slot.source_box.index() as usize),
            GeometryField::X,
        )?;
        let origin_y = finite_css_pixels(
            slot.rect.y(),
            Some(slot.source_box.index() as usize),
            GeometryField::Y,
        )?;
        let mut resolved_runs = Vec::new();
        let mut next_glyph_runs = self.glyph_runs;
        let mut next_glyphs = self.glyphs;
        for (font_instance, glyphs) in runs {
            if font_instance.0 != self.renderer_namespace {
                return Err(SceneBuildError::FontInstanceNamespaceMismatch {
                    expected: self.renderer_namespace,
                    actual: font_instance.0,
                });
            }
            next_glyph_runs = checked_resource_add(
                ResourceKind::ResolvedGlyphRuns,
                next_glyph_runs,
                1,
                self.max_glyph_runs,
            )?;
            next_glyphs = checked_resource_add(
                ResourceKind::ResolvedGlyphs,
                next_glyphs,
                glyphs.len(),
                self.max_glyphs,
            )?;
            resolved_runs
                .try_reserve(1)
                .map_err(|_| SceneBuildError::AllocationFailed {
                    resource: ResourceKind::ResolvedGlyphRuns,
                    requested: 1,
                })?;
            let mut resolved_glyphs = Vec::new();
            resolved_glyphs
                .try_reserve_exact(glyphs.len())
                .map_err(|_| SceneBuildError::AllocationFailed {
                    resource: ResourceKind::ResolvedGlyphs,
                    requested: glyphs.len(),
                })?;
            for glyph in glyphs {
                validate_resolved_coordinate(
                    glyph.point.x,
                    slot.source_box,
                    GeometryField::X,
                    self.max_abs_app_units,
                )?;
                validate_resolved_coordinate(
                    glyph.point.y,
                    slot.source_box,
                    GeometryField::Y,
                    self.max_abs_app_units,
                )?;
                let x = origin_x + glyph.point.x;
                let y = origin_y + glyph.point.y;
                validate_resolved_coordinate(
                    x,
                    slot.source_box,
                    GeometryField::X,
                    self.max_abs_app_units,
                )?;
                validate_resolved_coordinate(
                    y,
                    slot.source_box,
                    GeometryField::Y,
                    self.max_abs_app_units,
                )?;
                resolved_glyphs.push(ResolvedGlyph::new(glyph.index, x, y));
            }
            resolved_runs.push(ResolvedGlyphRun::new(font_instance, resolved_glyphs));
        }

        // Commit only after the complete entry has passed every fallible
        // validation/allocation step. A rejected entry can be retried safely.
        self.glyph_runs = next_glyph_runs;
        self.glyphs = next_glyphs;
        self.entries.push(ResolvedTextPrimitive::new(
            slot.item_id,
            slot.pending_text,
            slot.source_box,
            slot.rect,
            slot.color,
            slot.spatial_root,
            slot.clip,
            resolved_runs,
        ));
        self.next_slot += 1;
        Ok(())
    }

    /// Completes the immutable set only if every validated entry was resolved.
    ///
    /// # Errors
    ///
    /// Returns the first missing canonical pending-text index.
    pub fn finish(self) -> Result<ResolvedTextSet, SceneBuildError> {
        if self.next_slot != self.slots.len() {
            return Err(SceneBuildError::MissingTextResolution {
                pending_index: u32::try_from(self.next_slot)
                    .map_err(|_| SceneBuildError::IdentifierCapacityExceeded)?,
            });
        }
        Ok(ResolvedTextSet {
            scene_resolution_identity: self.scene_resolution_identity,
            document_version: self.document_version,
            renderer_namespace: self.renderer_namespace,
            entries: self.entries,
        })
    }
}

fn allocate_scene_resolution_identity() -> Result<SceneResolutionIdentity, SceneBuildError> {
    let mut current = NEXT_SCENE_RESOLUTION_IDENTITY.load(Ordering::Relaxed);
    loop {
        let (identity, next) = checked_scene_resolution_identity(current)?;
        match NEXT_SCENE_RESOLUTION_IDENTITY.compare_exchange_weak(
            current,
            next,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return Ok(identity),
            Err(observed) => current = observed,
        }
    }
}

const fn checked_scene_resolution_identity(
    current: u64,
) -> Result<(SceneResolutionIdentity, u64), SceneBuildError> {
    match current.checked_add(1) {
        Some(next) => Ok((SceneResolutionIdentity::new(current), next)),
        None => Err(SceneBuildError::SceneResolutionIdentityExhausted),
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
            SceneItem::Text(text) => {
                let bounds = webrender_rect(text.rect(), Some(text.source_box()))?;
                for run in text.glyph_runs() {
                    let mut glyphs = Vec::new();
                    glyphs.try_reserve_exact(run.glyphs().len()).map_err(|_| {
                        SceneBuildError::AllocationFailed {
                            resource: ResourceKind::ResolvedGlyphs,
                            requested: run.glyphs().len(),
                        }
                    })?;
                    glyphs.extend(run.glyphs().iter().copied().map(|glyph| GlyphInstance {
                        index: glyph.index(),
                        point: webrender_api::units::LayoutPoint::new(glyph.x(), glyph.y()),
                    }));
                    builder.push_text(
                        &common,
                        bounds,
                        &glyphs,
                        run.font_instance(),
                        webrender_color(text.color()),
                        None,
                    );
                }
            }
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

fn preflight_composed_webrender_bytes(
    scene: &Scene,
    glyph_runs: usize,
    glyphs: usize,
) -> Result<usize, SceneBuildError> {
    let decoration_count = scene
        .items()
        .iter()
        .filter(|item| matches!(item, SceneItem::Background(_) | SceneItem::Border(_)))
        .count();
    let primitive_count = checked_unbounded_add(decoration_count, glyph_runs)?;
    let base = preflight_webrender_bytes(primitive_count)?;
    let glyph_bytes = checked_unbounded_mul(glyphs, std::mem::size_of::<GlyphInstance>())?;
    // Every glyph slice carries a serialized byte length and item count, plus a
    // red-zone glyph consumed by WebRender's auxiliary iterator.
    let glyph_run_overhead = checked_unbounded_mul(
        glyph_runs,
        checked_unbounded_add(
            checked_unbounded_mul(2, std::mem::size_of::<usize>())?,
            std::mem::size_of::<GlyphInstance>(),
        )?,
    )?;
    checked_unbounded_add(
        checked_unbounded_add(base, glyph_bytes)?,
        glyph_run_overhead,
    )
}

fn resolved_counts(scene: &Scene) -> Result<(usize, usize), SceneBuildError> {
    let mut run_count = 0_usize;
    let mut glyph_count = 0_usize;
    for item in scene.items() {
        let SceneItem::Text(text) = item else {
            continue;
        };
        run_count = checked_unbounded_add(run_count, text.glyph_runs().len())?;
        for run in text.glyph_runs() {
            glyph_count = checked_unbounded_add(glyph_count, run.glyphs().len())?;
        }
    }
    Ok((run_count, glyph_count))
}

fn validate_resolved_metrics(
    pending: &PendingTextRun,
    metrics: SceneTextMetrics,
    limits: SceneLimits,
) -> Result<(), SceneBuildError> {
    let source = pending.source_box();
    let values = [
        (
            GeometryField::Width,
            pending.rect().width(),
            metrics.full_width(),
        ),
        (
            GeometryField::Height,
            pending.rect().height(),
            metrics.height(),
        ),
        (
            GeometryField::Baseline,
            pending.baseline(),
            metrics.first_baseline(),
        ),
        (
            GeometryField::AboveBaseline,
            pending.baseline(),
            metrics.above_baseline(),
        ),
        (
            GeometryField::FontSize,
            pending.font_size(),
            metrics.font_size(),
        ),
        (
            GeometryField::LineHeight,
            pending.line_height(),
            metrics.line_height(),
        ),
    ];
    for (field, expected, value) in values {
        let actual = resolved_metric_app_units(value, source, field, limits)?;
        if actual != expected {
            return Err(SceneBuildError::TextMetricMismatch {
                pending_index: pending.id().index(),
                field,
                expected,
                actual,
            });
        }
    }

    let expected_below = pending
        .rect()
        .height()
        .checked_sub(pending.baseline())
        .ok_or(SceneBuildError::GeometryOverflow {
            box_index: Some(source.index() as usize),
            axis: GeometryField::BelowBaseline,
        })?;
    let actual_below = resolved_metric_app_units(
        metrics.below_baseline(),
        source,
        GeometryField::BelowBaseline,
        limits,
    )?;
    if actual_below != expected_below {
        return Err(SceneBuildError::TextMetricMismatch {
            pending_index: pending.id().index(),
            field: GeometryField::BelowBaseline,
            expected: expected_below,
            actual: actual_below,
        });
    }
    Ok(())
}

fn resolved_metric_app_units(
    value: f32,
    source: SourceBoxId,
    field: GeometryField,
    limits: SceneLimits,
) -> Result<i32, SceneBuildError> {
    if !value.is_finite() {
        return Err(SceneBuildError::NonFiniteConversion {
            box_index: Some(source.index() as usize),
            field,
        });
    }
    let scaled = f64::from(value) * f64::from(Au::PER_CSS_PX);
    if scaled < f64::from(i32::MIN) || scaled > f64::from(i32::MAX) {
        return Err(SceneBuildError::GeometryOverflow {
            box_index: Some(source.index() as usize),
            axis: field,
        });
    }
    #[allow(clippy::cast_possible_truncation)]
    let raw = scaled.round() as i32;
    if raw < 0 {
        return Err(SceneBuildError::NegativeGeometry {
            box_index: Some(source.index() as usize),
            field,
            value: raw,
        });
    }
    validate_range(
        raw,
        Some(source.index() as usize),
        field,
        limits.max_abs_app_units,
    )?;
    Ok(raw)
}

fn validate_resolved_coordinate(
    value: f32,
    source: SourceBoxId,
    field: GeometryField,
    max_abs_app_units: i32,
) -> Result<(), SceneBuildError> {
    if !value.is_finite() {
        return Err(SceneBuildError::NonFiniteConversion {
            box_index: Some(source.index() as usize),
            field,
        });
    }
    let scaled = f64::from(value) * f64::from(Au::PER_CSS_PX);
    if scaled < f64::from(i32::MIN) || scaled > f64::from(i32::MAX) {
        return Err(SceneBuildError::GeometryOverflow {
            box_index: Some(source.index() as usize),
            axis: field,
        });
    }
    #[allow(clippy::cast_possible_truncation)]
    let raw = scaled.round() as i32;
    validate_range(raw, Some(source.index() as usize), field, max_abs_app_units)
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

#[cfg(test)]
mod tests {
    use super::checked_scene_resolution_identity;
    use crate::SceneBuildError;

    #[test]
    fn scene_resolution_identity_allocation_is_checked_and_never_wraps() {
        let (first, second_value) = checked_scene_resolution_identity(1).unwrap();
        let (second, _) = checked_scene_resolution_identity(second_value).unwrap();
        assert_ne!(first, second);

        let (_, exhausted_value) = checked_scene_resolution_identity(u64::MAX - 1).unwrap();
        assert_eq!(exhausted_value, u64::MAX);
        assert!(matches!(
            checked_scene_resolution_identity(exhausted_value),
            Err(SceneBuildError::SceneResolutionIdentityExhausted)
        ));
    }
}
