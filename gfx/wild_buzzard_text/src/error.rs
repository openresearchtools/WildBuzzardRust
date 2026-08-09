use std::fmt;

use crate::TextDirection;

/// A bounded resource checked before or while shaping.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TextResource {
    TextBytes,
    FontFamilies,
    FamilyNameBytes,
    LanguageBytes,
    FeatureSettings,
    VariationSettings,
    Runs,
    Clusters,
    Glyphs,
    Fonts,
    FontBytes,
    TotalFontBytes,
    NormalizedCoordinates,
    CacheBytes,
}

/// A scalar request or output field rejected by validation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvalidTextField {
    FontSize,
    FontWeight,
    FontStretch,
    ObliqueAngle,
    LineHeight,
    LetterSpacing,
    WordSpacing,
    VariationValue,
    OpenTypeTag,
    OutputCoordinate,
    OutputMetric,
    ClusterRange,
}

/// Structured failure at the text-engine boundary.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum TextError {
    ResourceLimitExceeded {
        resource: TextResource,
        observed: usize,
        limit: usize,
    },
    AllocationFailed {
        resource: TextResource,
        requested: usize,
    },
    InvalidValue {
        field: InvalidTextField,
    },
    EmptyFontFamily,
    InvalidLanguageTag {
        language: String,
    },
    UnsupportedDirection {
        direction: TextDirection,
    },
    UnsupportedMultilineText,
    EmbeddedFontRejected,
    EmbeddedFontFamilyMissing,
    NoUsableFont,
    BackendInvariant {
        detail: &'static str,
    },
}

impl fmt::Display for TextError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ResourceLimitExceeded {
                resource,
                observed,
                limit,
            } => write!(
                formatter,
                "text resource {resource:?} exceeded its limit: observed {observed}, limit {limit}"
            ),
            Self::AllocationFailed {
                resource,
                requested,
            } => write!(
                formatter,
                "could not reserve {requested} entries or bytes for text resource {resource:?}"
            ),
            Self::InvalidValue { field } => {
                write!(formatter, "invalid text value for {field:?}")
            }
            Self::EmptyFontFamily => formatter.write_str("font family names may not be empty"),
            Self::InvalidLanguageTag { language } => {
                write!(formatter, "invalid BCP 47 language tag: {language}")
            }
            Self::UnsupportedDirection { direction } => write!(
                formatter,
                "explicit {direction:?} base direction is not supported by this bounded adapter"
            ),
            Self::UnsupportedMultilineText => {
                formatter.write_str("the bounded text-run adapter does not accept line separators")
            }
            Self::EmbeddedFontRejected => {
                formatter.write_str("Fontique rejected the embedded fallback font")
            }
            Self::EmbeddedFontFamilyMissing => {
                formatter.write_str("embedded Fira Code did not register its expected family")
            }
            Self::NoUsableFont => formatter.write_str("no usable font produced glyph runs"),
            Self::BackendInvariant { detail } => {
                write!(formatter, "text backend violated an invariant: {detail}")
            }
        }
    }
}

impl std::error::Error for TextError {}
