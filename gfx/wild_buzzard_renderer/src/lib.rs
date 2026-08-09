//! Validated conversion from immutable Wild Buzzard layout output to an
//! immutable scene contract and a real `WebRender` built display list.
//!
//! This crate is deliberately a one-way boundary: it borrows layout output
//! while compiling, then owns only renderer-facing values. Fresh scenes are
//! renderer-neutral; resolving text attaches renderer-namespace font instance
//! keys before one immutable display list is rebuilt. `WebRender` never
//! receives DOM nodes, layout boxes, or mutable layout state.

#![forbid(unsafe_code)]

mod compiler;
mod contract;
mod error;

pub use compiler::{CompileRequest, PipelineKey, SceneCompiler, SceneLimits};
pub use contract::{
    AppUnitEdges, AppUnitRect, AppUnitSize, BackgroundPrimitive, BorderPrimitive, Color,
    CompiledScene, PendingTextId, PendingTextPrimitive, PendingTextRun, ResolvedGlyph,
    ResolvedGlyphRun, ResolvedTextPrimitive, ResolvedTextSet, Scene, SceneItem, SceneItemId,
    SceneTextDescriptor, SceneTextMetrics, SourceBoxId, SpatialRootId, TextResolutionBuilder,
    ValidatedTextMap, ViewportClipId,
};
pub use error::{GeometryField, ResourceKind, SceneBuildError};
