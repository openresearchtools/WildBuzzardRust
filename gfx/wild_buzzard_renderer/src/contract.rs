use webrender_api::{BuiltDisplayList, PipelineId};

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

/// A validated renderer-facing scene item.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SceneItem {
    /// An opaque or translucent solid background.
    Background(BackgroundPrimitive),
    /// A provisional solid border using layout's computed text color.
    Border(BorderPrimitive),
    /// A text run awaiting font selection and glyph shaping.
    PendingText(PendingTextPrimitive),
}

impl SceneItem {
    /// Returns the stable item identifier.
    #[must_use]
    pub const fn id(&self) -> SceneItemId {
        match self {
            Self::Background(item) => item.id(),
            Self::Border(item) => item.id(),
            Self::PendingText(item) => item.id(),
        }
    }
}

/// A fully validated, immutable scene independent of `WebRender` serialization.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Scene {
    document_revision: u64,
    viewport: AppUnitSize,
    content_size: AppUnitSize,
    spatial_root: SpatialRootId,
    viewport_clip: ViewportClipId,
    items: Vec<SceneItem>,
    pending_text: Vec<PendingTextRun>,
}

impl Scene {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        document_revision: u64,
        viewport: AppUnitSize,
        content_size: AppUnitSize,
        spatial_root: SpatialRootId,
        viewport_clip: ViewportClipId,
        items: Vec<SceneItem>,
        pending_text: Vec<PendingTextRun>,
    ) -> Self {
        Self {
            document_revision,
            viewport,
            content_size,
            spatial_root,
            viewport_clip,
            items,
            pending_text,
        }
    }

    /// Returns the exact source document revision.
    #[must_use]
    pub const fn document_revision(&self) -> u64 {
        self.document_revision
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

    /// Returns an item's stable ID.
    #[must_use]
    pub const fn item_id(&self, item: &SceneItem) -> SceneItemId {
        item.id()
    }
}

/// A validated immutable scene paired with `WebRender`'s serialized display list.
pub struct CompiledScene {
    pub(crate) scene: Scene,
    pub(crate) pipeline_id: PipelineId,
    pub(crate) display_list: BuiltDisplayList,
}

impl CompiledScene {
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
