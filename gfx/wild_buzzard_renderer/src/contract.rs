use webrender_api::{BuiltDisplayList, FontInstanceKey, IdNamespace, PipelineId};
use wild_buzzard_dom::DocumentVersion;

/// Process-local identity binding text resolution to exactly one compiled scene.
///
/// The scalar is deliberately private: callers can carry the opaque contracts
/// which contain it, but cannot manufacture or substitute an identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SceneResolutionIdentity(u64);

impl SceneResolutionIdentity {
    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }
}

/// A stable sequential item identifier within one scene.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SceneItemId(pub(crate) u32);

impl SceneItemId {
    /// Returns the zero-based item index.
    #[must_use]
    pub const fn index(self) -> u32 {
        self.0
    }
}

/// A stable sequential pending-text identifier within one scene.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PendingTextId(pub(crate) u32);

impl PendingTextId {
    /// Returns the zero-based pending-text index.
    #[must_use]
    pub const fn index(self) -> u32 {
        self.0
    }
}

/// Shaping metrics used to prove that glyph output belongs to one pending
/// scene-text record.
///
/// Values are CSS pixels. The baseline split is deliberately defined from
/// `first_baseline`: the extent above the baseline is `first_baseline`, and the
/// extent below it is `height - first_baseline`. Font ascent is not a placement
/// coordinate and is therefore absent from this contract.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SceneTextMetrics {
    full_width: f32,
    height: f32,
    first_baseline: f32,
    font_size: f32,
    line_height: f32,
}

impl SceneTextMetrics {
    /// Creates metrics for the exact shaped allocation that will be painted.
    #[must_use]
    pub const fn new(
        full_width: f32,
        height: f32,
        first_baseline: f32,
        font_size: f32,
        line_height: f32,
    ) -> Self {
        Self {
            full_width,
            height,
            first_baseline,
            font_size,
            line_height,
        }
    }

    /// Returns the full shaped advance.
    #[must_use]
    pub const fn full_width(self) -> f32 {
        self.full_width
    }

    /// Returns the shaped line-box height.
    #[must_use]
    pub const fn height(self) -> f32 {
        self.height
    }

    /// Returns the first baseline measured from the line-box top.
    #[must_use]
    pub const fn first_baseline(self) -> f32 {
        self.first_baseline
    }

    /// Returns the extent above the baseline.
    #[must_use]
    pub const fn above_baseline(self) -> f32 {
        self.first_baseline
    }

    /// Returns the extent below the baseline.
    #[must_use]
    pub const fn below_baseline(self) -> f32 {
        self.height - self.first_baseline
    }

    /// Returns the computed font size.
    #[must_use]
    pub const fn font_size(self) -> f32 {
        self.font_size
    }

    /// Returns the used line height.
    #[must_use]
    pub const fn line_height(self) -> f32 {
        self.line_height
    }
}

/// Borrowed identity and metrics for one shaped pending scene-text record.
///
/// This is the narrow renderer-neutral bridge used by the headless compositor.
/// It carries no font key, glyph storage, DOM pointer, layout reference, or
/// authority to construct a resolved scene.
#[derive(Clone, Copy, Debug)]
pub struct SceneTextDescriptor<'text> {
    document_version: DocumentVersion,
    pending_index: u32,
    text: &'text str,
    metrics: SceneTextMetrics,
}

impl<'text> SceneTextDescriptor<'text> {
    /// Creates a descriptor that must still be matched against a compiled
    /// scene by [`CompiledScene::validate_text_map`](crate::CompiledScene::validate_text_map).
    #[must_use]
    pub const fn new(
        document_version: DocumentVersion,
        pending_index: u32,
        text: &'text str,
        metrics: SceneTextMetrics,
    ) -> Self {
        Self {
            document_version,
            pending_index,
            text,
            metrics,
        }
    }

    /// Returns the exact source document identity and revision.
    #[must_use]
    pub const fn document_version(self) -> DocumentVersion {
        self.document_version
    }

    /// Returns the bounded scene-local pending-text index.
    #[must_use]
    pub const fn pending_index(self) -> u32 {
        self.pending_index
    }

    /// Returns the exact UTF-8 string that was shaped.
    #[must_use]
    pub const fn text(self) -> &'text str {
        self.text
    }

    /// Returns the exact shaped metrics.
    #[must_use]
    pub const fn metrics(self) -> SceneTextMetrics {
        self.metrics
    }
}

/// The layout-box index retained solely for diagnostics and hit-test metadata.
///
/// This is an integer identity, not a reference to a live layout box.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SourceBoxId(pub(crate) u32);

impl SourceBoxId {
    /// Returns the source layout-box index.
    #[must_use]
    pub const fn index(self) -> u32 {
        self.0
    }
}

/// The only spatial root in the first display-list contract.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SpatialRootId(pub(crate) u32);

impl SpatialRootId {
    /// Returns the stable scene-local spatial-root index.
    #[must_use]
    pub const fn index(self) -> u32 {
        self.0
    }
}

/// The viewport clip applied to every first-wave primitive.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ViewportClipId(pub(crate) u32);

impl ViewportClipId {
    /// Returns the stable scene-local clip index.
    #[must_use]
    pub const fn index(self) -> u32 {
        self.0
    }
}

/// An exact size in Wild Buzzard app units (60 units per CSS pixel).
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct AppUnitSize {
    width: i32,
    height: i32,
}

impl AppUnitSize {
    pub(crate) const fn new(width: i32, height: i32) -> Self {
        Self { width, height }
    }

    /// Returns the width in app units.
    #[must_use]
    pub const fn width(self) -> i32 {
        self.width
    }

    /// Returns the height in app units.
    #[must_use]
    pub const fn height(self) -> i32 {
        self.height
    }
}

/// An exact rectangle in Wild Buzzard app units.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct AppUnitRect {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

impl AppUnitRect {
    pub(crate) const fn new(x: i32, y: i32, width: i32, height: i32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// Returns the horizontal origin in app units.
    #[must_use]
    pub const fn x(self) -> i32 {
        self.x
    }

    /// Returns the vertical origin in app units.
    #[must_use]
    pub const fn y(self) -> i32 {
        self.y
    }

    /// Returns the width in app units.
    #[must_use]
    pub const fn width(self) -> i32 {
        self.width
    }

    /// Returns the height in app units.
    #[must_use]
    pub const fn height(self) -> i32 {
        self.height
    }
}

/// Four exact edge widths in top/right/bottom/left order.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct AppUnitEdges {
    top: i32,
    right: i32,
    bottom: i32,
    left: i32,
}

impl AppUnitEdges {
    pub(crate) const fn new(top: i32, right: i32, bottom: i32, left: i32) -> Self {
        Self {
            top,
            right,
            bottom,
            left,
        }
    }

    /// Returns the top edge width in app units.
    #[must_use]
    pub const fn top(self) -> i32 {
        self.top
    }

    /// Returns the right edge width in app units.
    #[must_use]
    pub const fn right(self) -> i32 {
        self.right
    }

    /// Returns the bottom edge width in app units.
    #[must_use]
    pub const fn bottom(self) -> i32 {
        self.bottom
    }

    /// Returns the left edge width in app units.
    #[must_use]
    pub const fn left(self) -> i32 {
        self.left
    }

    pub(crate) const fn is_zero(self) -> bool {
        self.top == 0 && self.right == 0 && self.bottom == 0 && self.left == 0
    }
}

/// An eight-bit, non-premultiplied RGBA color.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Color {
    red: u8,
    green: u8,
    blue: u8,
    alpha: u8,
}

impl Color {
    pub(crate) const fn new(red: u8, green: u8, blue: u8, alpha: u8) -> Self {
        Self {
            red,
            green,
            blue,
            alpha,
        }
    }

    /// Returns the red channel.
    #[must_use]
    pub const fn red(self) -> u8 {
        self.red
    }

    /// Returns the green channel.
    #[must_use]
    pub const fn green(self) -> u8 {
        self.green
    }

    /// Returns the blue channel.
    #[must_use]
    pub const fn blue(self) -> u8 {
        self.blue
    }

    /// Returns the alpha channel.
    #[must_use]
    pub const fn alpha(self) -> u8 {
        self.alpha
    }

    pub(crate) const fn is_transparent(self) -> bool {
        self.alpha == 0
    }
}

/// A solid box-background primitive.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackgroundPrimitive {
    id: SceneItemId,
    source_box: SourceBoxId,
    rect: AppUnitRect,
    color: Color,
    spatial_root: SpatialRootId,
    clip: ViewportClipId,
}

impl BackgroundPrimitive {
    pub(crate) const fn new(
        id: SceneItemId,
        source_box: SourceBoxId,
        rect: AppUnitRect,
        color: Color,
        spatial_root: SpatialRootId,
        clip: ViewportClipId,
    ) -> Self {
        Self {
            id,
            source_box,
            rect,
            color,
            spatial_root,
            clip,
        }
    }

    /// Returns the scene item identifier.
    #[must_use]
    pub const fn id(&self) -> SceneItemId {
        self.id
    }

    /// Returns the diagnostic source-box identifier.
    #[must_use]
    pub const fn source_box(&self) -> SourceBoxId {
        self.source_box
    }

    /// Returns the primitive bounds.
    #[must_use]
    pub const fn rect(&self) -> AppUnitRect {
        self.rect
    }

    /// Returns the fill color.
    #[must_use]
    pub const fn color(&self) -> Color {
        self.color
    }

    /// Returns the scene-local spatial root.
    #[must_use]
    pub const fn spatial_root(&self) -> SpatialRootId {
        self.spatial_root
    }

    /// Returns the scene-local viewport clip.
    #[must_use]
    pub const fn clip(&self) -> ViewportClipId {
        self.clip
    }
}

/// A solid box-border primitive.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BorderPrimitive {
    id: SceneItemId,
    source_box: SourceBoxId,
    rect: AppUnitRect,
    widths: AppUnitEdges,
    color: Color,
    spatial_root: SpatialRootId,
    clip: ViewportClipId,
}

impl BorderPrimitive {
    pub(crate) const fn new(
        id: SceneItemId,
        source_box: SourceBoxId,
        rect: AppUnitRect,
        widths: AppUnitEdges,
        color: Color,
        spatial_root: SpatialRootId,
        clip: ViewportClipId,
    ) -> Self {
        Self {
            id,
            source_box,
            rect,
            widths,
            color,
            spatial_root,
            clip,
        }
    }

    /// Returns the scene item identifier.
    #[must_use]
    pub const fn id(&self) -> SceneItemId {
        self.id
    }

    /// Returns the diagnostic source-box identifier.
    #[must_use]
    pub const fn source_box(&self) -> SourceBoxId {
        self.source_box
    }

    /// Returns the border bounds.
    #[must_use]
    pub const fn rect(&self) -> AppUnitRect {
        self.rect
    }

    /// Returns top/right/bottom/left border widths.
    #[must_use]
    pub const fn widths(&self) -> AppUnitEdges {
        self.widths
    }

    /// Returns the provisional solid border color.
    #[must_use]
    pub const fn color(&self) -> Color {
        self.color
    }

    /// Returns the scene-local spatial root.
    #[must_use]
    pub const fn spatial_root(&self) -> SpatialRootId {
        self.spatial_root
    }

    /// Returns the scene-local viewport clip.
    #[must_use]
    pub const fn clip(&self) -> ViewportClipId {
        self.clip
    }
}

/// Text metadata waiting for a font selector and shaping service.
///
/// No glyph identifiers are synthesized. Until a later graphics slice resolves
/// this record, it is deliberately absent from the `WebRender` display list.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingTextRun {
    id: PendingTextId,
    item_id: SceneItemId,
    source_box: SourceBoxId,
    rect: AppUnitRect,
    baseline: i32,
    text: String,
    color: Color,
    font_size: i32,
    line_height: i32,
    spatial_root: SpatialRootId,
    clip: ViewportClipId,
}

impl PendingTextRun {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        id: PendingTextId,
        item_id: SceneItemId,
        source_box: SourceBoxId,
        rect: AppUnitRect,
        baseline: i32,
        text: String,
        color: Color,
        font_size: i32,
        line_height: i32,
        spatial_root: SpatialRootId,
        clip: ViewportClipId,
    ) -> Self {
        Self {
            id,
            item_id,
            source_box,
            rect,
            baseline,
            text,
            color,
            font_size,
            line_height,
            spatial_root,
            clip,
        }
    }

    /// Returns the pending-resource identifier.
    #[must_use]
    pub const fn id(&self) -> PendingTextId {
        self.id
    }

    /// Returns the corresponding scene item identifier.
    #[must_use]
    pub const fn item_id(&self) -> SceneItemId {
        self.item_id
    }

    /// Returns the diagnostic source-box identifier.
    #[must_use]
    pub const fn source_box(&self) -> SourceBoxId {
        self.source_box
    }

    /// Returns the measured run bounds.
    #[must_use]
    pub const fn rect(&self) -> AppUnitRect {
        self.rect
    }

    /// Returns the baseline offset from the run's top, in app units.
    #[must_use]
    pub const fn baseline(&self) -> i32 {
        self.baseline
    }

    /// Returns the exact unshaped UTF-8 text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Returns the computed text color.
    #[must_use]
    pub const fn color(&self) -> Color {
        self.color
    }

    /// Returns the computed font size in app units.
    #[must_use]
    pub const fn font_size(&self) -> i32 {
        self.font_size
    }

    /// Returns the computed line height in app units.
    #[must_use]
    pub const fn line_height(&self) -> i32 {
        self.line_height
    }

    /// Returns the scene-local spatial root.
    #[must_use]
    pub const fn spatial_root(&self) -> SpatialRootId {
        self.spatial_root
    }

    /// Returns the scene-local viewport clip.
    #[must_use]
    pub const fn clip(&self) -> ViewportClipId {
        self.clip
    }
}

/// A scene item that points to text metadata awaiting font shaping.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PendingTextPrimitive {
    id: SceneItemId,
    pending_text: PendingTextId,
}

impl PendingTextPrimitive {
    pub(crate) const fn new(id: SceneItemId, pending_text: PendingTextId) -> Self {
        Self { id, pending_text }
    }

    /// Returns the scene item identifier.
    #[must_use]
    pub const fn id(self) -> SceneItemId {
        self.id
    }

    /// Returns the pending text-resource identifier.
    #[must_use]
    pub const fn pending_text(self) -> PendingTextId {
        self.pending_text
    }
}

/// One positioned glyph stored without a floating-point equality escape.
///
/// Resolution accepts only finite bounded coordinates. Storing their exact bit
/// patterns keeps the immutable scene deterministic and `Eq` while conversion
/// back to `WebRender` remains lossless.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResolvedGlyph {
    index: u32,
    x_bits: u32,
    y_bits: u32,
}

impl ResolvedGlyph {
    pub(crate) const fn new(index: u32, x: f32, y: f32) -> Self {
        Self {
            index,
            x_bits: x.to_bits(),
            y_bits: y.to_bits(),
        }
    }

    /// Returns the exact font glyph identifier supplied by the shaper.
    #[must_use]
    pub const fn index(self) -> u32 {
        self.index
    }

    /// Returns the positioned horizontal CSS-pixel coordinate.
    #[must_use]
    pub const fn x(self) -> f32 {
        f32::from_bits(self.x_bits)
    }

    /// Returns the positioned vertical CSS-pixel coordinate.
    #[must_use]
    pub const fn y(self) -> f32 {
        f32::from_bits(self.y_bits)
    }
}

/// One immutable `WebRender` font instance and its positioned glyphs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedGlyphRun {
    font_instance: FontInstanceKey,
    glyphs: Vec<ResolvedGlyph>,
}

impl ResolvedGlyphRun {
    pub(crate) const fn new(font_instance: FontInstanceKey, glyphs: Vec<ResolvedGlyph>) -> Self {
        Self {
            font_instance,
            glyphs,
        }
    }

    /// Returns the renderer-namespace font instance used by this run.
    #[must_use]
    pub const fn font_instance(&self) -> FontInstanceKey {
        self.font_instance
    }

    /// Returns positioned glyphs in visual run order.
    #[must_use]
    pub fn glyphs(&self) -> &[ResolvedGlyph] {
        &self.glyphs
    }
}

/// A pending scene item resolved to exact font instances and glyphs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedTextPrimitive {
    id: SceneItemId,
    pending_text: PendingTextId,
    source_box: SourceBoxId,
    rect: AppUnitRect,
    color: Color,
    spatial_root: SpatialRootId,
    clip: ViewportClipId,
    glyph_runs: Vec<ResolvedGlyphRun>,
}

impl ResolvedTextPrimitive {
    #[allow(clippy::too_many_arguments)]
    pub(crate) const fn new(
        id: SceneItemId,
        pending_text: PendingTextId,
        source_box: SourceBoxId,
        rect: AppUnitRect,
        color: Color,
        spatial_root: SpatialRootId,
        clip: ViewportClipId,
        glyph_runs: Vec<ResolvedGlyphRun>,
    ) -> Self {
        Self {
            id,
            pending_text,
            source_box,
            rect,
            color,
            spatial_root,
            clip,
            glyph_runs,
        }
    }

    /// Returns the scene item identifier retained across composition.
    #[must_use]
    pub const fn id(&self) -> SceneItemId {
        self.id
    }

    /// Returns the pending-text identity this primitive resolved.
    #[must_use]
    pub const fn pending_text(&self) -> PendingTextId {
        self.pending_text
    }

    /// Returns the diagnostic source-box identifier.
    #[must_use]
    pub const fn source_box(&self) -> SourceBoxId {
        self.source_box
    }

    /// Returns the line-fragment bounds.
    #[must_use]
    pub const fn rect(&self) -> AppUnitRect {
        self.rect
    }

    /// Returns the computed text color inherited from the pending record.
    #[must_use]
    pub const fn color(&self) -> Color {
        self.color
    }

    /// Returns the exact resolved glyph runs.
    #[must_use]
    pub fn glyph_runs(&self) -> &[ResolvedGlyphRun] {
        &self.glyph_runs
    }

    /// Returns the scene-local spatial root.
    #[must_use]
    pub const fn spatial_root(&self) -> SpatialRootId {
        self.spatial_root
    }

    /// Returns the scene-local viewport clip.
    #[must_use]
    pub const fn clip(&self) -> ViewportClipId {
        self.clip
    }
}

/// A validated renderer-facing scene item.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SceneItem {
    /// An opaque or translucent solid background.
    Background(BackgroundPrimitive),
    /// A provisional solid border using layout's computed text color.
    Border(BorderPrimitive),
    /// A text run awaiting font selection and glyph shaping.
    PendingText(PendingTextPrimitive),
    /// A text run resolved to exact positioned glyphs and font instances.
    Text(ResolvedTextPrimitive),
}

impl SceneItem {
    /// Returns the stable item identifier.
    #[must_use]
    pub const fn id(&self) -> SceneItemId {
        match self {
            Self::Background(item) => item.id(),
            Self::Border(item) => item.id(),
            Self::PendingText(item) => item.id(),
            Self::Text(item) => item.id(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ValidatedTextSlot {
    pub(crate) pending_text: PendingTextId,
    pub(crate) item_id: SceneItemId,
    pub(crate) source_box: SourceBoxId,
    pub(crate) rect: AppUnitRect,
    pub(crate) color: Color,
    pub(crate) spatial_root: SpatialRootId,
    pub(crate) clip: ViewportClipId,
}

/// A completed exact mapping from shaped allocations to one compiled scene.
///
/// Values can only be created by validating [`SceneTextDescriptor`] records
/// against a [`CompiledScene`]. A private non-reusing identity prevents the map
/// and every value derived from it from being rebound to another compilation.
/// Maps still contain no renderer font keys; those are supplied transactionally
/// through [`TextResolutionBuilder`].
pub struct ValidatedTextMap {
    pub(crate) scene_resolution_identity: SceneResolutionIdentity,
    pub(crate) document_version: DocumentVersion,
    pub(crate) slots: Vec<ValidatedTextSlot>,
    pub(crate) max_glyph_runs: usize,
    pub(crate) max_glyphs: usize,
    pub(crate) max_abs_app_units: i32,
}

impl ValidatedTextMap {
    /// Returns the exact scene document identity and revision.
    #[must_use]
    pub const fn document_version(&self) -> DocumentVersion {
        self.document_version
    }

    /// Returns how many pending records were mapped exactly.
    #[must_use]
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// Returns whether the mapped scene contains no text.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }
}

/// Stateful checked construction of one namespace-bound resolved-text set.
///
/// The builder accepts entries only in the exact validated canonical order and
/// cannot produce a set while an entry is missing. It retains the originating
/// compiled-scene identity and proves that every key has the expected
/// `WebRender` namespace, not that each scalar key is registered.
pub struct TextResolutionBuilder {
    pub(crate) scene_resolution_identity: SceneResolutionIdentity,
    pub(crate) document_version: DocumentVersion,
    pub(crate) renderer_namespace: IdNamespace,
    pub(crate) slots: Vec<ValidatedTextSlot>,
    pub(crate) next_slot: usize,
    pub(crate) entries: Vec<ResolvedTextPrimitive>,
    pub(crate) glyph_runs: usize,
    pub(crate) glyphs: usize,
    pub(crate) max_glyph_runs: usize,
    pub(crate) max_glyphs: usize,
    pub(crate) max_abs_app_units: i32,
}

/// Every text item for one exact compiled scene, resolved in deterministic
/// paint order and bound independently to one `WebRender` namespace. There is
/// no public unchecked constructor.
#[derive(Clone)]
pub struct ResolvedTextSet {
    pub(crate) scene_resolution_identity: SceneResolutionIdentity,
    pub(crate) document_version: DocumentVersion,
    pub(crate) renderer_namespace: IdNamespace,
    pub(crate) entries: Vec<ResolvedTextPrimitive>,
}

impl ResolvedTextSet {
    /// Returns the exact source document identity and revision.
    #[must_use]
    pub const fn document_version(&self) -> DocumentVersion {
        self.document_version
    }

    /// Returns the `WebRender` namespace checked on every font-instance key.
    ///
    /// This proves namespace consistency, not that every scalar key is present
    /// in a particular renderer's live font registry.
    #[must_use]
    pub const fn renderer_namespace(&self) -> IdNamespace {
        self.renderer_namespace
    }

    /// Returns resolved entries in canonical pending-text order.
    #[must_use]
    pub fn entries(&self) -> &[ResolvedTextPrimitive] {
        &self.entries
    }
}

/// A fully validated immutable scene independent of `WebRender` serialization.
///
/// A freshly compiled scene is renderer-neutral and contains pending text. A
/// composed scene contains namespace-checked [`FontInstanceKey`] values in its
/// resolved text primitives and must therefore be submitted only through a
/// renderer with the same namespace. Namespace equality alone does not prove
/// that an arbitrary scalar key belongs to a live registry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Scene {
    document_version: DocumentVersion,
    viewport: AppUnitSize,
    content_size: AppUnitSize,
    spatial_root: SpatialRootId,
    viewport_clip: ViewportClipId,
    pub(crate) renderer_namespace: Option<IdNamespace>,
    pub(crate) items: Vec<SceneItem>,
    pub(crate) pending_text: Vec<PendingTextRun>,
}

impl Scene {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        document_version: DocumentVersion,
        viewport: AppUnitSize,
        content_size: AppUnitSize,
        spatial_root: SpatialRootId,
        viewport_clip: ViewportClipId,
        items: Vec<SceneItem>,
        pending_text: Vec<PendingTextRun>,
    ) -> Self {
        Self {
            document_version,
            viewport,
            content_size,
            spatial_root,
            viewport_clip,
            renderer_namespace: None,
            items,
            pending_text,
        }
    }

    /// Returns the exact source document identity and local revision.
    #[must_use]
    pub const fn document_version(&self) -> DocumentVersion {
        self.document_version
    }

    /// Returns the validated viewport size.
    #[must_use]
    pub const fn viewport(&self) -> AppUnitSize {
        self.viewport
    }

    /// Returns the validated laid-out content size.
    #[must_use]
    pub const fn content_size(&self) -> AppUnitSize {
        self.content_size
    }

    /// Returns the only scene-local spatial root.
    #[must_use]
    pub const fn spatial_root(&self) -> SpatialRootId {
        self.spatial_root
    }

    /// Returns the scene-local viewport clip.
    #[must_use]
    pub const fn viewport_clip(&self) -> ViewportClipId {
        self.viewport_clip
    }

    /// Returns the namespace checked on all resolved font-instance keys.
    ///
    /// Fresh scenes have no namespace because their text is still pending.
    #[must_use]
    pub const fn renderer_namespace(&self) -> Option<IdNamespace> {
        self.renderer_namespace
    }

    /// Returns display items in deterministic paint order.
    #[must_use]
    pub fn items(&self) -> &[SceneItem] {
        &self.items
    }

    /// Returns text resources that still require font selection and shaping.
    #[must_use]
    pub fn pending_text(&self) -> &[PendingTextRun] {
        &self.pending_text
    }

    /// Resolves a pending-text identifier without exposing mutable storage.
    #[must_use]
    pub fn pending_text_by_id(&self, id: PendingTextId) -> Option<&PendingTextRun> {
        self.pending_text.get(id.0 as usize)
    }

    /// Returns how many scene items contain fully resolved glyph data.
    #[must_use]
    pub fn resolved_text_count(&self) -> usize {
        self.items
            .iter()
            .filter(|item| matches!(item, SceneItem::Text(_)))
            .count()
    }

    /// Returns an item's stable ID.
    #[must_use]
    pub const fn item_id(&self, item: &SceneItem) -> SceneItemId {
        item.id()
    }
}

/// A validated immutable scene paired with `WebRender`'s serialized display
/// list and a private process-local text-resolution identity.
pub struct CompiledScene {
    pub(crate) scene_resolution_identity: SceneResolutionIdentity,
    pub(crate) scene: Scene,
    pub(crate) pipeline_id: PipelineId,
    pub(crate) display_list: BuiltDisplayList,
    pub(crate) limits: crate::SceneLimits,
}

impl CompiledScene {
    /// Returns the exact source document identity and local revision.
    #[must_use]
    pub const fn document_version(&self) -> DocumentVersion {
        self.scene.document_version()
    }

    /// Returns the renderer-independent immutable scene.
    #[must_use]
    pub const fn scene(&self) -> &Scene {
        &self.scene
    }

    /// Returns `WebRender`'s real serialized display list for inspection.
    #[must_use]
    pub const fn built_display_list(&self) -> &BuiltDisplayList {
        &self.display_list
    }

    /// Consumes the contract and returns values suitable for renderer submission.
    #[must_use]
    pub fn into_webrender(self) -> (PipelineId, BuiltDisplayList) {
        (self.pipeline_id, self.display_list)
    }
}
