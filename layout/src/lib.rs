//! Deterministic wave-one block/inline layout over immutable DOM snapshots.
//!
//! Styling and text measurement are capabilities supplied through traits. The
//! built-in implementations are intentionally small UA defaults; imported
//! Stylo and the graphics font system can implement the same contracts without
//! layout gaining access to mutable DOM nodes.

mod geometry;
mod style;
mod tree;

pub use geometry::{Au, Edges, Point, Rect, Size, Viewport};
pub use style::{
    Color, ComputedStyle, Display, InitialStyleResolver, StyleInput, StyleResolver, WhiteSpace,
};
pub use tree::{
    BoxId, BoxKind, Fragment, LayoutBox, LayoutError, LayoutLimits, LayoutOutput, LayoutPhase,
    LayoutWarning, LayoutWarningCode, MonospaceTextMeasurer, TextMeasurer, TextMetrics,
    layout_document, layout_document_with_limits,
};
