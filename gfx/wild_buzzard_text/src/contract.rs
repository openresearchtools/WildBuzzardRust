use std::fmt;
use std::ops::Range;
use std::sync::Arc;

use linebender_resource_handle::Blob;

/// A CSS generic font family, independent of the selected shaping library.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum GenericFamily {
    Serif,
    SansSerif,
    Monospace,
    Cursive,
    Fantasy,
    SystemUi,
    UiSerif,
    UiSansSerif,
    UiMonospace,
    UiRounded,
    Emoji,
    Math,
    FangSong,
}

/// One entry in an ordered CSS font-family list.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum FontFamily {
    Named(String),
    Generic(GenericFamily),
}

impl FontFamily {
    /// Creates a named family. Validation is performed by [`crate::TextSystem::shape`].
    #[must_use]
    pub fn named(name: impl Into<String>) -> Self {
        Self::Named(name.into())
    }
}

/// CSS font weight, normally in the inclusive range 1 through 1000.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FontWeight(f32);

impl FontWeight {
    pub const NORMAL: Self = Self(400.0);
    pub const BOLD: Self = Self(700.0);

    #[must_use]
    pub const fn new(value: f32) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn value(self) -> f32 {
        self.0
    }
}

impl Default for FontWeight {
    fn default() -> Self {
        Self::NORMAL
    }
}

/// CSS font stretch represented as a ratio where one is normal width.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FontStretch(f32);

impl FontStretch {
    pub const NORMAL: Self = Self(1.0);

    #[must_use]
    pub const fn from_ratio(ratio: f32) -> Self {
        Self(ratio)
    }

    #[must_use]
    pub const fn ratio(self) -> f32 {
        self.0
    }
}

impl Default for FontStretch {
    fn default() -> Self {
        Self::NORMAL
    }
}

/// Requested CSS font style.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum FontStyle {
    #[default]
    Normal,
    Italic,
    Oblique {
        degrees: f32,
    },
}

/// Direction requested for Unicode bidi resolution.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum TextDirection {
    #[default]
    Auto,
    LeftToRight,
    RightToLeft,
}

/// Resolved direction of a shaped paragraph or run.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum RunDirection {
    LeftToRight,
    RightToLeft,
}

/// Why a used line height differs from the `normal` keyword.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum LineHeightProvenance {
    Explicit,
    ProvisionalNormal120Percent,
}

/// The distinction between CSS `normal` and an already-resolved used value.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum LineHeight {
    #[default]
    Normal,
    Used {
        px: f32,
        provenance: LineHeightProvenance,
    },
}

/// A validated four-byte OpenType feature setting.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct FontFeature {
    tag: [u8; 4],
    value: u16,
}

impl FontFeature {
    #[must_use]
    pub const fn new(tag: [u8; 4], value: u16) -> Self {
        Self { tag, value }
    }

    #[must_use]
    pub const fn tag(self) -> [u8; 4] {
        self.tag
    }

    #[must_use]
    pub const fn value(self) -> u16 {
        self.value
    }
}

/// A four-byte OpenType variation-axis setting.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FontVariation {
    tag: [u8; 4],
    value: f32,
}

impl FontVariation {
    #[must_use]
    pub const fn new(tag: [u8; 4], value: f32) -> Self {
        Self { tag, value }
    }

    #[must_use]
    pub const fn tag(self) -> [u8; 4] {
        self.tag
    }

    #[must_use]
    pub const fn value(self) -> f32 {
        self.value
    }
}

/// A complete immutable request for shaping one unwrapped text run.
#[derive(Clone, Debug, PartialEq)]
pub struct TextRequest {
    text: String,
    families: Vec<FontFamily>,
    font_size_px: f32,
    line_height: LineHeight,
    weight: FontWeight,
    stretch: FontStretch,
    style: FontStyle,
    language: Option<String>,
    direction: TextDirection,
    features: Vec<FontFeature>,
    variations: Vec<FontVariation>,
    letter_spacing_px: f32,
    word_spacing_px: f32,
}

impl TextRequest {
    /// Creates a request with CSS initial-like font properties.
    #[must_use]
    pub fn new(text: impl Into<String>, font_size_px: f32) -> Self {
        Self {
            text: text.into(),
            families: vec![FontFamily::Generic(GenericFamily::SansSerif)],
            font_size_px,
            line_height: LineHeight::Normal,
            weight: FontWeight::NORMAL,
            stretch: FontStretch::NORMAL,
            style: FontStyle::Normal,
            language: None,
            direction: TextDirection::Auto,
            features: Vec::new(),
            variations: Vec::new(),
            letter_spacing_px: 0.0,
            word_spacing_px: 0.0,
        }
    }

    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    #[must_use]
    pub fn families(&self) -> &[FontFamily] {
        &self.families
    }

    #[must_use]
    pub const fn font_size_px(&self) -> f32 {
        self.font_size_px
    }

    #[must_use]
    pub const fn line_height(&self) -> LineHeight {
        self.line_height
    }

    #[must_use]
    pub const fn weight(&self) -> FontWeight {
        self.weight
    }

    #[must_use]
    pub const fn stretch(&self) -> FontStretch {
        self.stretch
    }

    #[must_use]
    pub const fn style(&self) -> FontStyle {
        self.style
    }

    #[must_use]
    pub fn language(&self) -> Option<&str> {
        self.language.as_deref()
    }

    #[must_use]
    pub const fn direction(&self) -> TextDirection {
        self.direction
    }

    #[must_use]
    pub fn features(&self) -> &[FontFeature] {
        &self.features
    }

    #[must_use]
    pub fn variations(&self) -> &[FontVariation] {
        &self.variations
    }

    #[must_use]
    pub const fn letter_spacing_px(&self) -> f32 {
        self.letter_spacing_px
    }

    #[must_use]
    pub const fn word_spacing_px(&self) -> f32 {
        self.word_spacing_px
    }

    #[must_use]
    pub fn with_families(mut self, families: Vec<FontFamily>) -> Self {
        self.families = families;
        self
    }

    #[must_use]
    pub const fn with_line_height(mut self, line_height: LineHeight) -> Self {
        self.line_height = line_height;
        self
    }

    #[must_use]
    pub const fn with_weight(mut self, weight: FontWeight) -> Self {
        self.weight = weight;
        self
    }

    #[must_use]
    pub const fn with_stretch(mut self, stretch: FontStretch) -> Self {
        self.stretch = stretch;
        self
    }

    #[must_use]
    pub const fn with_style(mut self, style: FontStyle) -> Self {
        self.style = style;
        self
    }

    #[must_use]
    pub fn with_language(mut self, language: Option<String>) -> Self {
        self.language = language;
        self
    }

    #[must_use]
    pub const fn with_direction(mut self, direction: TextDirection) -> Self {
        self.direction = direction;
        self
    }

    #[must_use]
    pub fn with_features(mut self, features: Vec<FontFeature>) -> Self {
        self.features = features;
        self
    }

    #[must_use]
    pub fn with_variations(mut self, variations: Vec<FontVariation>) -> Self {
        self.variations = variations;
        self
    }

    #[must_use]
    pub const fn with_letter_spacing_px(mut self, spacing: f32) -> Self {
        self.letter_spacing_px = spacing;
        self
    }

    #[must_use]
    pub const fn with_word_spacing_px(mut self, spacing: f32) -> Self {
        self.word_spacing_px = spacing;
        self
    }
}

/// Process-unique font blob identity plus a face index within a collection.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct FontFaceId {
    blob_id: u64,
    collection_index: u32,
}

impl FontFaceId {
    #[must_use]
    pub const fn blob_id(self) -> u64 {
        self.blob_id
    }

    #[must_use]
    pub const fn collection_index(self) -> u32 {
        self.collection_index
    }
}

/// Shareable validated font bytes selected by the shaping engine.
#[derive(Clone)]
pub struct FontFace {
    data: Blob<u8>,
    collection_index: u32,
}

impl FontFace {
    pub(crate) fn new(data: Blob<u8>, collection_index: u32) -> Self {
        Self {
            data,
            collection_index,
        }
    }

    #[must_use]
    pub fn id(&self) -> FontFaceId {
        FontFaceId {
            blob_id: self.data.id(),
            collection_index: self.collection_index,
        }
    }

    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        self.data.data()
    }

    #[must_use]
    pub const fn collection_index(&self) -> u32 {
        self.collection_index
    }

    /// Compares both identity and the complete bytes. Resource registries use
    /// this to fail closed if an upstream identity invariant is ever violated.
    #[must_use]
    pub fn exactly_matches(&self, other: &Self) -> bool {
        self.id() == other.id() && self.bytes() == other.bytes()
    }
}

impl fmt::Debug for FontFace {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FontFace")
            .field("id", &self.id())
            .field("byte_len", &self.bytes().len())
            .finish()
    }
}

impl PartialEq for FontFace {
    fn eq(&self, other: &Self) -> bool {
        self.id() == other.id()
    }
}

impl Eq for FontFace {}

/// Four-byte ISO 15924 script tag.
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ScriptTag([u8; 4]);

impl ScriptTag {
    pub const UNKNOWN: Self = Self(*b"Zzzz");
    pub const COMMON: Self = Self(*b"Zyyy");
    pub const INHERITED: Self = Self(*b"Zinh");

    #[must_use]
    pub const fn from_bytes(bytes: [u8; 4]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub const fn to_bytes(self) -> [u8; 4] {
        self.0
    }
}

impl fmt::Debug for ScriptTag {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("ScriptTag")
            .field(&String::from_utf8_lossy(&self.0))
            .finish()
    }
}

/// Renderer-relevant synthetic styling selected during font matching.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FontSynthesis {
    embolden: bool,
    skew_degrees: Option<f32>,
}

impl FontSynthesis {
    pub(crate) const fn new(embolden: bool, skew_degrees: Option<f32>) -> Self {
        Self {
            embolden,
            skew_degrees,
        }
    }

    #[must_use]
    pub const fn embolden(self) -> bool {
        self.embolden
    }

    #[must_use]
    pub const fn skew_degrees(self) -> Option<f32> {
        self.skew_degrees
    }
}

/// One positioned glyph in CSS-pixel coordinates relative to the text origin.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PositionedGlyph {
    id: u32,
    x: f32,
    y: f32,
    advance: f32,
    cluster_index: u32,
}

impl PositionedGlyph {
    pub(crate) const fn new(id: u32, x: f32, y: f32, advance: f32, cluster_index: u32) -> Self {
        Self {
            id,
            x,
            y,
            advance,
            cluster_index,
        }
    }

    #[must_use]
    pub const fn id(self) -> u32 {
        self.id
    }

    #[must_use]
    pub const fn x(self) -> f32 {
        self.x
    }

    #[must_use]
    pub const fn y(self) -> f32 {
        self.y
    }

    #[must_use]
    pub const fn advance(self) -> f32 {
        self.advance
    }

    #[must_use]
    pub const fn cluster_index(self) -> u32 {
        self.cluster_index
    }
}

/// UTF-8 byte and glyph ranges for one grapheme/shape cluster.
#[derive(Clone, Debug, PartialEq)]
pub struct GlyphCluster {
    text_range: Range<usize>,
    glyph_range: Range<usize>,
    x: f32,
    advance: f32,
    ligature_start: bool,
    ligature_continuation: bool,
}

impl GlyphCluster {
    pub(crate) const fn new(
        text_range: Range<usize>,
        glyph_range: Range<usize>,
        x: f32,
        advance: f32,
        ligature_start: bool,
        ligature_continuation: bool,
    ) -> Self {
        Self {
            text_range,
            glyph_range,
            x,
            advance,
            ligature_start,
            ligature_continuation,
        }
    }

    #[must_use]
    pub fn text_range(&self) -> Range<usize> {
        self.text_range.clone()
    }

    #[must_use]
    pub fn glyph_range(&self) -> Range<usize> {
        self.glyph_range.clone()
    }

    #[must_use]
    pub const fn x(&self) -> f32 {
        self.x
    }

    #[must_use]
    pub const fn advance(&self) -> f32 {
        self.advance
    }

    #[must_use]
    pub const fn is_ligature_start(&self) -> bool {
        self.ligature_start
    }

    #[must_use]
    pub const fn is_ligature_continuation(&self) -> bool {
        self.ligature_continuation
    }
}

/// Typographic metrics associated with one selected font run.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RunMetrics {
    pub ascent: f32,
    pub descent: f32,
    pub leading: f32,
    pub underline_offset: f32,
    pub underline_size: f32,
    pub strikethrough_offset: f32,
    pub strikethrough_size: f32,
    pub line_height: f32,
    pub x_height: Option<f32>,
    pub cap_height: Option<f32>,
}

/// One visual-order, single-font, single-direction shaped run.
#[derive(Clone, Debug, PartialEq)]
pub struct ShapedRun {
    face: FontFace,
    font_size_px: f32,
    normalized_variation_coordinates: Box<[i16]>,
    synthesis: FontSynthesis,
    direction: RunDirection,
    script: ScriptTag,
    text_range: Range<usize>,
    advance: f32,
    metrics: RunMetrics,
    glyphs: Box<[PositionedGlyph]>,
    clusters: Box<[GlyphCluster]>,
}

impl ShapedRun {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        face: FontFace,
        font_size_px: f32,
        normalized_variation_coordinates: Box<[i16]>,
        synthesis: FontSynthesis,
        direction: RunDirection,
        script: ScriptTag,
        text_range: Range<usize>,
        advance: f32,
        metrics: RunMetrics,
        glyphs: Box<[PositionedGlyph]>,
        clusters: Box<[GlyphCluster]>,
    ) -> Self {
        Self {
            face,
            font_size_px,
            normalized_variation_coordinates,
            synthesis,
            direction,
            script,
            text_range,
            advance,
            metrics,
            glyphs,
            clusters,
        }
    }

    #[must_use]
    pub const fn face(&self) -> &FontFace {
        &self.face
    }

    #[must_use]
    pub const fn font_size_px(&self) -> f32 {
        self.font_size_px
    }

    #[must_use]
    pub fn normalized_variation_coordinates(&self) -> &[i16] {
        &self.normalized_variation_coordinates
    }

    #[must_use]
    pub const fn synthesis(&self) -> FontSynthesis {
        self.synthesis
    }

    #[must_use]
    pub const fn direction(&self) -> RunDirection {
        self.direction
    }

    #[must_use]
    pub const fn script(&self) -> ScriptTag {
        self.script
    }

    #[must_use]
    pub fn text_range(&self) -> Range<usize> {
        self.text_range.clone()
    }

    #[must_use]
    pub const fn advance(&self) -> f32 {
        self.advance
    }

    #[must_use]
    pub const fn metrics(&self) -> RunMetrics {
        self.metrics
    }

    #[must_use]
    pub fn glyphs(&self) -> &[PositionedGlyph] {
        &self.glyphs
    }

    #[must_use]
    pub fn clusters(&self) -> &[GlyphCluster] {
        &self.clusters
    }
}

/// Aggregate metrics used by layout for the exact shaped result that is painted.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TextMetrics {
    width: f32,
    full_width: f32,
    height: f32,
    first_baseline: f32,
    ascent: f32,
    descent: f32,
    leading: f32,
    line_height: f32,
}

impl TextMetrics {
    #[allow(clippy::too_many_arguments)]
    pub(crate) const fn new(
        width: f32,
        full_width: f32,
        height: f32,
        first_baseline: f32,
        ascent: f32,
        descent: f32,
        leading: f32,
        line_height: f32,
    ) -> Self {
        Self {
            width,
            full_width,
            height,
            first_baseline,
            ascent,
            descent,
            leading,
            line_height,
        }
    }

    #[must_use]
    pub const fn width(self) -> f32 {
        self.width
    }

    #[must_use]
    pub const fn full_width(self) -> f32 {
        self.full_width
    }

    #[must_use]
    pub const fn height(self) -> f32 {
        self.height
    }

    #[must_use]
    pub const fn first_baseline(self) -> f32 {
        self.first_baseline
    }

    #[must_use]
    pub const fn ascent(self) -> f32 {
        self.ascent
    }

    #[must_use]
    pub const fn descent(self) -> f32 {
        self.descent
    }

    #[must_use]
    pub const fn leading(self) -> f32 {
        self.leading
    }

    #[must_use]
    pub const fn line_height(self) -> f32 {
        self.line_height
    }
}

/// Immutable result shared by layout, hit testing, and painting.
#[derive(Clone, Debug, PartialEq)]
pub struct ShapedText {
    text: Arc<str>,
    collection_revision: u64,
    base_direction: RunDirection,
    line_height_request: LineHeight,
    metrics: TextMetrics,
    runs: Box<[ShapedRun]>,
}

impl ShapedText {
    pub(crate) fn new(
        text: Arc<str>,
        collection_revision: u64,
        base_direction: RunDirection,
        line_height_request: LineHeight,
        metrics: TextMetrics,
        runs: Box<[ShapedRun]>,
    ) -> Self {
        Self {
            text,
            collection_revision,
            base_direction,
            line_height_request,
            metrics,
            runs,
        }
    }

    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    #[must_use]
    pub const fn collection_revision(&self) -> u64 {
        self.collection_revision
    }

    #[must_use]
    pub const fn base_direction(&self) -> RunDirection {
        self.base_direction
    }

    #[must_use]
    pub const fn line_height_request(&self) -> LineHeight {
        self.line_height_request
    }

    #[must_use]
    pub const fn metrics(&self) -> TextMetrics {
        self.metrics
    }

    #[must_use]
    pub fn runs(&self) -> &[ShapedRun] {
        &self.runs
    }

    #[must_use]
    pub fn glyph_count(&self) -> usize {
        self.runs.iter().map(|run| run.glyphs.len()).sum()
    }

    #[must_use]
    pub fn cluster_count(&self) -> usize {
        self.runs.iter().map(|run| run.clusters.len()).sum()
    }
}

/// Observable bounded-cache state, useful for diagnostics and tests.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CacheStatistics {
    pub(crate) entries: usize,
    pub(crate) accounted_bytes: usize,
    pub(crate) hits: u64,
    pub(crate) misses: u64,
}

impl CacheStatistics {
    #[must_use]
    pub const fn entries(self) -> usize {
        self.entries
    }

    #[must_use]
    pub const fn accounted_bytes(self) -> usize {
        self.accounted_bytes
    }

    #[must_use]
    pub const fn hits(self) -> u64 {
        self.hits
    }

    #[must_use]
    pub const fn misses(self) -> u64 {
        self.misses
    }
}
