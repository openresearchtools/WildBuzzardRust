//! Deterministic wave-one block/inline layout over immutable DOM snapshots.
//!
//! Styling and text measurement are capabilities supplied through traits. The
//! built-in implementations are intentionally small UA defaults; imported
//! Stylo and the graphics font system can implement the same contracts without
//! layout gaining access to mutable DOM nodes.

mod flex;
mod geometry;
mod style;
mod tree;

pub use geometry::{Au, Edges, Point, Rect, Size, Viewport};
pub use style::{
    AlignItems, AlignSelf, AutomaticMarginEdges, BackgroundImageLayers, BackgroundTransparency,
    BoxSizing, Color, ComputedStyle, ComputedStyleSnapshot, ComputedStyleSnapshotError,
    ComputedStyleSnapshotLimits, Display, EffectiveContainment, FlexBasis, FlexDirection,
    FlexFactor, FlexStyle, FlexWrap, InitialStyleResolver, InlineDirection, JustifyContent,
    LengthPercentage, MaxSizeValue, PercentageEdges, SizeValue, StyleInput, StyleResolver,
    WhiteSpace, WritingMode,
};
pub use tree::{
    AutomaticMarginContext, BoxId, BoxKind, CanvasBackground, CanvasBackgroundDecision,
    CanvasBackgroundSource, CanvasBackgroundStyleFacts, CanvasBodyLayoutRelation,
    CanvasBodyProvenance, Fragment, GeneratedCanvasBody, LayoutBox, LayoutBoxIdentity, LayoutError,
    LayoutLimits, LayoutOutput, LayoutPhase, LayoutWarning, LayoutWarningCode,
    MonospaceTextMeasurer, TextMeasurer, TextMetrics, layout_document, layout_document_with_limits,
    layout_document_with_style_snapshot, layout_document_with_style_snapshot_and_limits,
};
