use std::fmt;

use webrender_api::IdNamespace;
use wild_buzzard_dom::DocumentVersion;
use wild_buzzard_text::FontFaceId;

/// A bounded resource checked at the shaped-text renderer boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TextRenderResource {
    SceneTexts,
    TextBytes,
    TotalTextBytes,
    Runs,
    Clusters,
    Glyphs,
    FontTemplates,
    FontInstances,
    FontBytes,
    RegisteredFontBytes,
    DisplayListBytes,
}

/// A malformed renderer-facing field.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvalidRenderField {
    Pipeline,
    Viewport,
    Origin,
    TextMetric,
    RunMetric,
    FontSize,
    FontData,
    SyntheticSkew,
    RunRange,
    ClusterRange,
    GlyphRange,
    GlyphPosition,
    GlyphAdvance,
    RunAdvance,
}

/// Structured failures while validating, registering, or emitting shaped text.
#[derive(Debug)]
#[non_exhaustive]
pub enum TextRenderError {
    InvalidLimit {
        field: &'static str,
        value: usize,
    },
    ResourceLimitExceeded {
        resource: TextRenderResource,
        observed: usize,
        limit: usize,
    },
    InvalidValue {
        field: InvalidRenderField,
    },
    DocumentVersionMismatch {
        expected: DocumentVersion,
        actual: DocumentVersion,
    },
    MissingSceneText {
        pending_index: u32,
    },
    DuplicateSceneText {
        pending_index: u32,
    },
    UnknownSceneText {
        pending_index: u32,
        available: usize,
    },
    OutOfOrderSceneText {
        expected: u32,
        actual: u32,
    },
    RendererNamespaceMismatch {
        expected: IdNamespace,
        actual: IdNamespace,
    },
    GeneratedKeyNamespaceMismatch {
        expected: IdNamespace,
        actual: IdNamespace,
    },
    FontIdentityCollision {
        id: FontFaceId,
    },
    MissingFontInstance {
        id: FontFaceId,
    },
    UnsupportedNormalizedVariations {
        coordinate_count: usize,
    },
    AllocationFailed {
        resource: TextRenderResource,
        requested: usize,
    },
    DisplayListBuildFailed,
}

impl fmt::Display for TextRenderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLimit { field, value } => {
                write!(formatter, "invalid text-render limit {field}={value}")
            }
            Self::ResourceLimitExceeded {
                resource,
                observed,
                limit,
            } => write!(
                formatter,
                "text-render resource {resource:?} exceeded its limit: observed {observed}, limit {limit}"
            ),
            Self::InvalidValue { field } => {
                write!(
                    formatter,
                    "invalid shaped-text renderer value for {field:?}"
                )
            }
            Self::DocumentVersionMismatch { expected, actual } => write!(
                formatter,
                "shaped scene text document {} revision {} does not match expected document {} revision {}",
                actual.document_id().get(),
                actual.revision(),
                expected.document_id().get(),
                expected.revision()
            ),
            Self::MissingSceneText { pending_index } => {
                write!(formatter, "missing shaped scene text {pending_index}")
            }
            Self::DuplicateSceneText { pending_index } => {
                write!(formatter, "duplicate shaped scene text {pending_index}")
            }
            Self::UnknownSceneText {
                pending_index,
                available,
            } => write!(
                formatter,
                "shaped scene text {pending_index} does not exist in an inventory of {available}"
            ),
            Self::OutOfOrderSceneText { expected, actual } => write!(
                formatter,
                "out-of-order shaped scene text {actual}; expected {expected}"
            ),
            Self::RendererNamespaceMismatch { expected, actual } => write!(
                formatter,
                "text registry belongs to WebRender namespace {expected:?}, not {actual:?}"
            ),
            Self::GeneratedKeyNamespaceMismatch { expected, actual } => write!(
                formatter,
                "WebRender generated resource namespace {actual:?}, expected {expected:?}"
            ),
            Self::FontIdentityCollision { id } => write!(
                formatter,
                "font identity {id:?} referred to different complete bytes or face indices"
            ),
            Self::MissingFontInstance { id } => write!(
                formatter,
                "prepared text run for font identity {id:?} has no registered or staged instance"
            ),
            Self::UnsupportedNormalizedVariations { coordinate_count } => write!(
                formatter,
                "cannot map {coordinate_count} normalized font coordinates to WebRender axis tags"
            ),
            Self::AllocationFailed {
                resource,
                requested,
            } => write!(
                formatter,
                "could not reserve {requested} entries or bytes for {resource:?}"
            ),
            Self::DisplayListBuildFailed => {
                formatter.write_str("WebRender panicked while building the shaped-text list")
            }
        }
    }
}

impl std::error::Error for TextRenderError {}
