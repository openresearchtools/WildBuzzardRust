use std::collections::HashMap;
use std::fmt;

use wild_buzzard_dom::{
    DocumentId, DocumentSnapshot, DocumentVersion, ElementData, NodeId, NodeKind, SnapshotNode,
};

use crate::geometry::{Au, Edges};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Display {
    None,
    Block,
    Inline,
    Flex,
}

/// Main-axis selection for a supported CSS flex formatting context.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum FlexDirection {
    #[default]
    Row,
    Column,
}

/// Line-breaking policy for a supported CSS flex formatting context.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum FlexWrap {
    #[default]
    NoWrap,
    Wrap,
}

/// Layout-facing `flex-basis` after loss-checked Stylo projection.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum FlexBasis {
    #[default]
    Auto,
    Content,
    LengthPercentage(LengthPercentage),
}

/// A non-negative CSS flex factor in millionths.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct FlexFactor(u32);

impl FlexFactor {
    pub const ONE: Self = Self(1_000_000);

    pub const fn from_millionths(value: u32) -> Self {
        Self(value)
    }

    pub const fn millionths(self) -> u32 {
        self.0
    }
}

/// Supported main-axis packing values.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum JustifyContent {
    #[default]
    Start,
    End,
    Center,
    SpaceBetween,
    SpaceAround,
    SpaceEvenly,
}

/// Supported cross-axis alignment values for a flex container.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AlignItems {
    #[default]
    Stretch,
    Start,
    End,
    Center,
}

/// Supported per-item cross-axis override.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AlignSelf {
    #[default]
    Auto,
    Stretch,
    Start,
    End,
    Center,
}

/// Non-inherited values consumed by the bounded flex formatting context.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FlexStyle {
    pub direction: FlexDirection,
    pub wrap: FlexWrap,
    pub basis: FlexBasis,
    pub grow: FlexFactor,
    pub shrink: FlexFactor,
    pub justify_content: JustifyContent,
    pub align_items: AlignItems,
    pub align_self: AlignSelf,
    pub row_gap: LengthPercentage,
    pub column_gap: LengthPercentage,
    pub order: i32,
}

impl Default for FlexStyle {
    fn default() -> Self {
        Self {
            direction: FlexDirection::Row,
            wrap: FlexWrap::NoWrap,
            basis: FlexBasis::Auto,
            grow: FlexFactor::default(),
            shrink: FlexFactor::ONE,
            justify_content: JustifyContent::Start,
            align_items: AlignItems::Stretch,
            align_self: AlignSelf::Auto,
            row_gap: LengthPercentage::default(),
            column_gap: LengthPercentage::default(),
            order: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WhiteSpace {
    Normal,
    Pre,
}

/// CSS `box-sizing` interpretation for preferred and min/max sizes.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum BoxSizing {
    #[default]
    ContentBox,
    BorderBox,
}

/// Physical block-flow direction selected by CSS `writing-mode`.
///
/// The current layout nucleus implements only [`Self::HorizontalTb`]. The
/// vertical variants are retained in the computed-style contract so layout can
/// reject them explicitly instead of silently laying them out horizontally.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum WritingMode {
    #[default]
    HorizontalTb,
    VerticalRl,
    VerticalLr,
}

/// Inline base direction selected by the inherited CSS `direction` property.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum InlineDirection {
    #[default]
    Ltr,
    Rtl,
}

/// A computed non-negative `<length-percentage>`.
///
/// Percentages use the same millionths representation as [`PercentageEdges`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LengthPercentage {
    pub length: Au,
    pub percentage: i32,
}

impl LengthPercentage {
    pub const fn length(length: Au) -> Self {
        Self {
            length,
            percentage: 0,
        }
    }

    pub const fn percentage(percentage: i32) -> Self {
        Self {
            length: Au::ZERO,
            percentage,
        }
    }

    /// Resolves against a definite percentage basis.
    pub fn resolve(self, basis: Au) -> Au {
        (self.length + basis.scale(self.percentage, PercentageEdges::ONE_HUNDRED_PERCENT))
            .non_negative()
    }

    /// Resolves when a percentage basis may be indefinite. A pure length does
    /// not need a basis; a percentage component does.
    pub fn resolve_optional(self, basis: Option<Au>) -> Option<Au> {
        if self.percentage == 0 {
            Some(self.length.non_negative())
        } else {
            basis.map(|basis| self.resolve(basis))
        }
    }
}

/// Computed value for `width`, `height`, `min-width`, or `min-height`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SizeValue {
    #[default]
    Auto,
    LengthPercentage(LengthPercentage),
}

impl SizeValue {
    pub const fn length(length: Au) -> Self {
        Self::LengthPercentage(LengthPercentage::length(length))
    }

    pub const fn percentage(percentage: i32) -> Self {
        Self::LengthPercentage(LengthPercentage::percentage(percentage))
    }

    pub fn resolve_optional(self, basis: Option<Au>) -> Option<Au> {
        match self {
            Self::Auto => None,
            Self::LengthPercentage(value) => value.resolve_optional(basis),
        }
    }
}

/// Computed value for `max-width` or `max-height`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MaxSizeValue {
    #[default]
    None,
    LengthPercentage(LengthPercentage),
}

impl MaxSizeValue {
    pub const fn length(length: Au) -> Self {
        Self::LengthPercentage(LengthPercentage::length(length))
    }

    pub const fn percentage(percentage: i32) -> Self {
        Self::LengthPercentage(LengthPercentage::percentage(percentage))
    }

    pub fn resolve_optional(self, basis: Option<Au>) -> Option<Au> {
        match self {
            Self::None => None,
            Self::LengthPercentage(value) => value.resolve_optional(basis),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Color {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
    pub alpha: u8,
}

impl Color {
    pub const TRANSPARENT: Self = Self {
        red: 0,
        green: 0,
        blue: 0,
        alpha: 0,
    };
    pub const BLACK: Self = Self {
        red: 0,
        green: 0,
        blue: 0,
        alpha: 255,
    };
}

/// The part of computed `background-image` needed by CSS canvas propagation.
///
/// Firefox ESR treats a background as image-transparent only when the computed list contains
/// exactly one `none` layer. Every other list, including `none, none`, is meaningful for root/body
/// selection even though this layout contract does not retain or render the image values.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum BackgroundImageLayers {
    /// The producer did not project the computed image-list invariant.
    #[default]
    Unknown,
    /// The computed list contains exactly one `none` layer.
    SingleNone,
    /// The computed list has any other length or contains any non-`none` image.
    Meaningful,
}

/// Whether effective computed containment can block HTML-body canvas propagation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum EffectiveContainment {
    /// The producer did not project effective containment.
    #[default]
    Unknown,
    /// No effective containment bit is set.
    None,
    /// At least one effective containment bit is set.
    Any,
}

/// The exact three-way root-background test used before considering an HTML body fallback.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BackgroundTransparency {
    /// The computed image-layer fact is unavailable, so body fallback must fail closed.
    Unknown,
    /// Color alpha is zero and the image list contains exactly one `none` layer.
    Transparent,
    /// A nonzero color alpha or meaningful image-list shape makes the background meaningful.
    Meaningful,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ComputedStyle {
    pub display: Display,
    pub flex: FlexStyle,
    pub margin: Edges,
    /// Percentage components of physical margins, in millionths of the
    /// containing block's inline size.
    pub margin_percentage: PercentageEdges,
    /// Physical margins whose computed value remains `auto` until layout.
    pub automatic_margin: AutomaticMarginEdges,
    pub border: Edges,
    pub padding: Edges,
    /// Percentage components of physical padding, in millionths of the
    /// containing block's inline size.
    pub padding_percentage: PercentageEdges,
    pub width: SizeValue,
    pub height: SizeValue,
    pub min_width: SizeValue,
    pub min_height: SizeValue,
    pub max_width: MaxSizeValue,
    pub max_height: MaxSizeValue,
    pub box_sizing: BoxSizing,
    pub writing_mode: WritingMode,
    pub inline_direction: InlineDirection,
    pub font_size: Au,
    pub line_height: Au,
    pub color: Color,
    pub background_color: Color,
    /// Lossless ESR root/body classification of the computed background-image list.
    pub background_image_layers: BackgroundImageLayers,
    /// Whether any effective computed containment applies to this element.
    pub effective_containment: EffectiveContainment,
    pub white_space: WhiteSpace,
}

impl Default for ComputedStyle {
    fn default() -> Self {
        let font_size = Au::from_px(16);
        Self {
            display: Display::Inline,
            flex: FlexStyle::default(),
            margin: Edges::default(),
            margin_percentage: PercentageEdges::default(),
            automatic_margin: AutomaticMarginEdges::default(),
            border: Edges::default(),
            padding: Edges::default(),
            padding_percentage: PercentageEdges::default(),
            width: SizeValue::Auto,
            height: SizeValue::Auto,
            min_width: SizeValue::Auto,
            min_height: SizeValue::Auto,
            max_width: MaxSizeValue::None,
            max_height: MaxSizeValue::None,
            box_sizing: BoxSizing::ContentBox,
            writing_mode: WritingMode::HorizontalTb,
            inline_direction: InlineDirection::Ltr,
            font_size,
            line_height: font_size.scale(6, 5),
            color: Color::BLACK,
            background_color: Color::TRANSPARENT,
            background_image_layers: BackgroundImageLayers::Unknown,
            effective_containment: EffectiveContainment::Unknown,
            white_space: WhiteSpace::Normal,
        }
    }
}

/// Physical-edge record preserving computed `auto` margins until used-value resolution.
///
/// The absolute and percentage components in [`ComputedStyle::margin`] and
/// [`ComputedStyle::margin_percentage`] are ignored for an edge marked here.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AutomaticMarginEdges {
    pub top: bool,
    pub right: bool,
    pub bottom: bool,
    pub left: bool,
}

impl AutomaticMarginEdges {
    /// Returns whether any physical margin remains automatic.
    pub const fn any(self) -> bool {
        self.top || self.right || self.bottom || self.left
    }
}

/// Fixed-point percentage edges used until layout knows the containing block.
///
/// One hundred percent is represented by `1_000_000`. Keeping the percentage
/// separate from the absolute app-unit component prevents the style adapter
/// from resolving percentages against the viewport or another incorrect
/// containing block.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PercentageEdges {
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
    pub left: i32,
}

impl PercentageEdges {
    pub const ONE_HUNDRED_PERCENT: i32 = 1_000_000;

    pub const fn all(value: i32) -> Self {
        Self {
            top: value,
            right: value,
            bottom: value,
            left: value,
        }
    }

    pub fn resolve(self, inline_size: Au) -> Edges {
        Edges {
            top: inline_size.scale(self.top, Self::ONE_HUNDRED_PERCENT),
            right: inline_size.scale(self.right, Self::ONE_HUNDRED_PERCENT),
            bottom: inline_size.scale(self.bottom, Self::ONE_HUNDRED_PERCENT),
            left: inline_size.scale(self.left, Self::ONE_HUNDRED_PERCENT),
        }
    }
}

/// Construction limits for an immutable computed-style publication.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ComputedStyleSnapshotLimits {
    pub max_entries: usize,
}

impl Default for ComputedStyleSnapshotLimits {
    fn default() -> Self {
        Self {
            max_entries: 100_000,
        }
    }
}

/// Error returned while atomically preparing an immutable style publication.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ComputedStyleSnapshotError {
    EntryCapacityExceeded {
        limit: usize,
    },
    AllocationFailed {
        requested: usize,
    },
    WrongDocument {
        node: NodeId,
        expected: DocumentId,
        actual: DocumentId,
    },
    UnknownNode(NodeId),
    NotAnElement(NodeId),
    DuplicateStyle(NodeId),
    MissingStyle(NodeId),
}

impl fmt::Display for ComputedStyleSnapshotError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EntryCapacityExceeded { limit } => {
                write!(formatter, "computed-style entry limit {limit} exceeded")
            }
            Self::AllocationFailed { requested } => write!(
                formatter,
                "could not reserve storage for {requested} computed-style entries"
            ),
            Self::WrongDocument {
                node,
                expected,
                actual,
            } => write!(
                formatter,
                "style for node slot {} belongs to document {}, expected {}",
                node.slot(),
                actual.get(),
                expected.get()
            ),
            Self::UnknownNode(node) => {
                write!(
                    formatter,
                    "style references unknown node slot {}",
                    node.slot()
                )
            }
            Self::NotAnElement(node) => {
                write!(
                    formatter,
                    "style references non-element node slot {}",
                    node.slot()
                )
            }
            Self::DuplicateStyle(node) => {
                write!(formatter, "duplicate style for node slot {}", node.slot())
            }
            Self::MissingStyle(node) => {
                write!(
                    formatter,
                    "missing style for element node slot {}",
                    node.slot()
                )
            }
        }
    }
}

impl std::error::Error for ComputedStyleSnapshotError {}

/// Owned, immutable layout-facing styles for one exact DOM revision.
#[derive(Clone, Debug)]
pub struct ComputedStyleSnapshot {
    document_version: DocumentVersion,
    styles: HashMap<NodeId, ComputedStyle>,
}

impl ComputedStyleSnapshot {
    /// Validates every entry before publishing the completed map.
    pub fn try_new(
        snapshot: &DocumentSnapshot,
        entries: impl IntoIterator<Item = (NodeId, ComputedStyle)>,
        limits: ComputedStyleSnapshotLimits,
    ) -> Result<Self, ComputedStyleSnapshotError> {
        let expected_entries = snapshot
            .nodes_in_document_order()
            .iter()
            .filter(|node| matches!(node.kind, NodeKind::Element(_)))
            .count();
        if expected_entries > limits.max_entries {
            return Err(ComputedStyleSnapshotError::EntryCapacityExceeded {
                limit: limits.max_entries,
            });
        }
        let mut styles = HashMap::new();
        styles.try_reserve(expected_entries).map_err(|_| {
            ComputedStyleSnapshotError::AllocationFailed {
                requested: expected_entries,
            }
        })?;
        for (node, style) in entries {
            if styles.len() >= limits.max_entries {
                return Err(ComputedStyleSnapshotError::EntryCapacityExceeded {
                    limit: limits.max_entries,
                });
            }
            if node.document_id() != snapshot.document_id() {
                return Err(ComputedStyleSnapshotError::WrongDocument {
                    node,
                    expected: snapshot.document_id(),
                    actual: node.document_id(),
                });
            }
            let snapshot_node = snapshot
                .node(node)
                .ok_or(ComputedStyleSnapshotError::UnknownNode(node))?;
            if !matches!(snapshot_node.kind, NodeKind::Element(_)) {
                return Err(ComputedStyleSnapshotError::NotAnElement(node));
            }
            if styles.insert(node, style).is_some() {
                return Err(ComputedStyleSnapshotError::DuplicateStyle(node));
            }
        }
        for node in snapshot.nodes_in_document_order() {
            if matches!(node.kind, NodeKind::Element(_)) && !styles.contains_key(&node.id) {
                return Err(ComputedStyleSnapshotError::MissingStyle(node.id));
            }
        }
        Ok(Self {
            document_version: snapshot.version(),
            styles,
        })
    }

    pub const fn document_version(&self) -> DocumentVersion {
        self.document_version
    }

    pub const fn document_id(&self) -> DocumentId {
        self.document_version.document_id()
    }

    pub const fn document_revision(&self) -> u64 {
        self.document_version.revision()
    }

    pub fn get(&self, node: NodeId) -> Option<&ComputedStyle> {
        self.styles.get(&node)
    }

    pub fn len(&self) -> usize {
        self.styles.len()
    }

    pub fn is_empty(&self) -> bool {
        self.styles.is_empty()
    }
}

impl ComputedStyle {
    /// Classifies the complete background as Firefox ESR does for root/body propagation.
    pub const fn background_transparency(&self) -> BackgroundTransparency {
        if self.background_color.alpha != 0 {
            return BackgroundTransparency::Meaningful;
        }
        match self.background_image_layers {
            BackgroundImageLayers::Unknown => BackgroundTransparency::Unknown,
            BackgroundImageLayers::SingleNone => BackgroundTransparency::Transparent,
            BackgroundImageLayers::Meaningful => BackgroundTransparency::Meaningful,
        }
    }

    pub fn inherit_from(parent: Option<&Self>) -> Self {
        let Some(parent) = parent else {
            return Self::default();
        };
        Self {
            font_size: parent.font_size,
            line_height: parent.line_height,
            color: parent.color,
            white_space: parent.white_space,
            writing_mode: parent.writing_mode,
            inline_direction: parent.inline_direction,
            ..Self::default()
        }
    }
}

/// Immutable input presented to a style-system adapter.
pub struct StyleInput<'a> {
    pub node_id: NodeId,
    pub node: &'a SnapshotNode,
    pub element: &'a ElementData,
    pub parent_style: Option<&'a ComputedStyle>,
}

/// Boundary that Stylo can implement without receiving mutable DOM ownership.
pub trait StyleResolver: Send + Sync {
    fn resolve(&self, input: StyleInput<'_>) -> ComputedStyle;
}

/// Small deterministic test-only UA-style baseline.
///
/// Product integration must publish styles computed by the imported Stylo
/// engine rather than use this resolver.
#[derive(Clone, Copy, Debug, Default)]
pub struct InitialStyleResolver;

impl StyleResolver for InitialStyleResolver {
    fn resolve(&self, input: StyleInput<'_>) -> ComputedStyle {
        let mut style = ComputedStyle::inherit_from(input.parent_style);
        style.background_image_layers = BackgroundImageLayers::SingleNone;
        style.effective_containment = EffectiveContainment::None;
        let name = input.element.name.local_name.as_str();
        style.display = if input.element.html_attribute("hidden").is_some()
            || matches!(
                name,
                "head"
                    | "base"
                    | "basefont"
                    | "bgsound"
                    | "link"
                    | "meta"
                    | "title"
                    | "style"
                    | "script"
                    | "template"
            ) {
            Display::None
        } else if is_ua_block(name) {
            Display::Block
        } else {
            Display::Inline
        };

        match name {
            "body" => style.margin = Edges::all(Au::from_px(8)),
            "p" => {
                style.margin.top = Au::from_px(16);
                style.margin.bottom = Au::from_px(16);
            }
            "blockquote" => {
                style.margin = Edges {
                    top: Au::from_px(16),
                    right: Au::from_px(40),
                    bottom: Au::from_px(16),
                    left: Au::from_px(40),
                };
            }
            "pre" => {
                style.margin.top = Au::from_px(16);
                style.margin.bottom = Au::from_px(16);
                style.white_space = WhiteSpace::Pre;
            }
            "h1" => set_heading(&mut style, 2, 1, 11, 16),
            "h2" => set_heading(&mut style, 3, 2, 10, 12),
            "h3" => set_heading(&mut style, 6, 5, 8, 8),
            "h4" => set_heading(&mut style, 1, 1, 8, 8),
            "h5" => set_heading(&mut style, 5, 6, 11, 11),
            "h6" => set_heading(&mut style, 2, 3, 12, 12),
            _ => {}
        }
        style
    }
}

fn set_heading(
    style: &mut ComputedStyle,
    numerator: i32,
    denominator: i32,
    margin_top_px: i32,
    margin_bottom_px: i32,
) {
    style.font_size = Au::from_px(16).scale(numerator, denominator);
    style.line_height = style.font_size.scale(6, 5);
    style.margin.top = Au::from_px(margin_top_px);
    style.margin.bottom = Au::from_px(margin_bottom_px);
}

fn is_ua_block(name: &str) -> bool {
    matches!(
        name,
        "html"
            | "body"
            | "address"
            | "article"
            | "aside"
            | "blockquote"
            | "center"
            | "details"
            | "dialog"
            | "dir"
            | "div"
            | "dl"
            | "dt"
            | "dd"
            | "fieldset"
            | "figcaption"
            | "figure"
            | "footer"
            | "form"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "header"
            | "hgroup"
            | "hr"
            | "main"
            | "menu"
            | "nav"
            | "ol"
            | "p"
            | "pre"
            | "search"
            | "section"
            | "summary"
            | "table"
            | "ul"
            | "li"
    )
}
