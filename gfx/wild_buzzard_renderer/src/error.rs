use std::fmt;

use webrender_api::IdNamespace;
use wild_buzzard_dom::DocumentVersion;

/// A geometry field rejected during layout-output validation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GeometryField {
    /// Horizontal origin.
    X,
    /// Vertical origin.
    Y,
    /// Width.
    Width,
    /// Height.
    Height,
    /// Top edge.
    Top,
    /// Right edge.
    Right,
    /// Bottom edge.
    Bottom,
    /// Left edge.
    Left,
    /// Text baseline.
    Baseline,
    /// Text extent above the first baseline.
    AboveBaseline,
    /// Text extent below the first baseline.
    BelowBaseline,
    /// Computed font size.
    FontSize,
    /// Computed line height.
    LineHeight,
}

/// A bounded resource class.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResourceKind {
    /// Layout boxes.
    Boxes,
    /// Child references.
    ChildReferences,
    /// Box fragments.
    Fragments,
    /// Renderer scene items.
    SceneItems,
    /// Pending text-resource records.
    PendingTextRuns,
    /// Resolved font/glyph runs across one composed scene.
    ResolvedGlyphRuns,
    /// Positioned glyphs across one composed scene.
    ResolvedGlyphs,
    /// UTF-8 bytes in one text run.
    TextRunBytes,
    /// UTF-8 bytes across all text runs.
    TotalTextBytes,
    /// Traversal depth.
    TreeDepth,
    /// Serialized `WebRender` bytes.
    WebRenderBytes,
}

/// A structured failure produced before a scene can reach `WebRender`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SceneBuildError {
    /// Layout output does not match the document identity and revision requested by the caller.
    DocumentVersionMismatch {
        /// Document version expected by the navigation/document owner.
        expected: DocumentVersion,
        /// Document version carried by layout output.
        actual: DocumentVersion,
    },
    /// The requested pipeline is `WebRender`'s invalid sentinel.
    InvalidPipeline,
    /// A bounded resource exceeded its configured maximum.
    ResourceLimitExceeded {
        /// Resource class.
        resource: ResourceKind,
        /// Observed count or size.
        observed: usize,
        /// Configured maximum.
        limit: usize,
    },
    /// A fallible first-party allocation could not be reserved.
    AllocationFailed {
        /// Resource class being allocated.
        resource: ResourceKind,
        /// Exact requested capacity or byte count.
        requested: usize,
    },
    /// The root refers to a missing layout box.
    MissingRootBox {
        /// Missing zero-based box index.
        box_index: usize,
    },
    /// A layout box's identity does not match its vector slot.
    InvalidBoxIdentity {
        /// Actual vector slot.
        slot: usize,
        /// Index reported by the box identity.
        reported: usize,
    },
    /// A child reference points outside the layout-box vector.
    MissingChildBox {
        /// Referring parent slot.
        parent: usize,
        /// Missing child slot.
        child: usize,
    },
    /// A box has more than one incoming parent edge.
    MultipleParents {
        /// Multiply-parented box slot.
        box_index: usize,
    },
    /// The root unexpectedly has a parent edge.
    RootHasParent {
        /// Root box slot.
        box_index: usize,
    },
    /// A child cycle was found.
    BoxCycle {
        /// Box slot at which the cycle was detected.
        box_index: usize,
    },
    /// A box cannot be reached from the declared root.
    UnreachableBox {
        /// Unreachable box slot.
        box_index: usize,
    },
    /// A layout output without a root still contains boxes.
    BoxesWithoutRoot {
        /// Number of orphan boxes.
        boxes: usize,
    },
    /// A leaf-only layout box contains child references.
    LeafHasChildren {
        /// Offending box slot.
        box_index: usize,
    },
    /// Text content appeared on a non-text layout box.
    TextOnNonTextBox {
        /// Offending box slot.
        box_index: usize,
    },
    /// A text fragment is missing the baseline required by the text boundary.
    TextMissingBaseline {
        /// Offending box slot.
        box_index: usize,
        /// Offending fragment slot.
        fragment_index: usize,
    },
    /// A signed geometry dimension or edge is negative.
    NegativeGeometry {
        /// Optional source box slot.
        box_index: Option<usize>,
        /// Rejected field.
        field: GeometryField,
        /// Rejected app-unit value.
        value: i32,
    },
    /// Geometry lies outside the configured absolute app-unit range.
    GeometryOutOfRange {
        /// Optional source box slot.
        box_index: Option<usize>,
        /// Rejected field.
        field: GeometryField,
        /// Rejected app-unit value.
        value: i32,
        /// Maximum absolute app-unit value.
        limit: i32,
    },
    /// A rectangle edge cannot be represented without integer overflow.
    GeometryOverflow {
        /// Source box slot, if this was a fragment.
        box_index: Option<usize>,
        /// Axis whose origin and extent overflowed.
        axis: GeometryField,
    },
    /// Conversion to `WebRender`'s floating-point geometry was not finite.
    NonFiniteConversion {
        /// Optional source box slot.
        box_index: Option<usize>,
        /// Field being converted.
        field: GeometryField,
    },
    /// Font metrics are not strictly positive.
    InvalidFontMetric {
        /// Source box slot.
        box_index: usize,
        /// Font metric field.
        field: GeometryField,
        /// Rejected value in app units.
        value: i32,
    },
    /// A pending-text or item identifier exceeded its stable `u32` domain.
    IdentifierCapacityExceeded,
    /// The process-local non-reusing compiled-scene identity domain is exhausted.
    SceneResolutionIdentityExhausted,
    /// Resolved text was validated for a different compiled scene.
    TextResolutionSceneMismatch,
    /// A canonical pending-text entry was omitted from shaped or resolved input.
    MissingTextResolution {
        /// First missing canonical pending-text index.
        pending_index: u32,
    },
    /// A pending-text index occurred more than once.
    DuplicateTextResolution {
        /// Duplicated scene-local pending-text index.
        pending_index: u32,
    },
    /// An input index does not exist in the compiled scene.
    UnknownTextResolution {
        /// Rejected scene-local pending-text index.
        pending_index: u32,
        /// Number of pending records available in the scene.
        available: usize,
    },
    /// Valid entries were supplied in an order other than their canonical
    /// pending-text order.
    OutOfOrderTextResolution {
        /// Canonical index required at this position.
        expected: u32,
        /// Index actually supplied.
        actual: u32,
    },
    /// Shaped UTF-8 does not exactly match the pending scene record.
    TextContentMismatch {
        /// Scene-local pending-text index.
        pending_index: u32,
    },
    /// Quantized shaping metrics differ from the exact layout record.
    TextMetricMismatch {
        /// Scene-local pending-text index.
        pending_index: u32,
        /// Metric that differs.
        field: GeometryField,
        /// Layout value in app units.
        expected: i32,
        /// Shaped value quantized to app units.
        actual: i32,
    },
    /// A resolved font instance belongs to a different `WebRender` namespace.
    FontInstanceNamespaceMismatch {
        /// Namespace to which this resolution is explicitly bound.
        expected: IdNamespace,
        /// Namespace carried by the rejected font-instance key.
        actual: IdNamespace,
    },
    /// A resolved entry no longer refers to the scene item validated for it.
    ResolvedTextItemMismatch {
        /// Scene-local pending-text index.
        pending_index: u32,
    },
}

impl fmt::Display for SceneBuildError {
    #[allow(clippy::too_many_lines)]
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DocumentVersionMismatch { expected, actual } => {
                format_document_version_mismatch(formatter, *expected, *actual)
            }
            Self::InvalidPipeline => formatter.write_str("invalid WebRender pipeline identifier"),
            Self::ResourceLimitExceeded {
                resource,
                observed,
                limit,
            } => format_resource_limit(formatter, *resource, *observed, *limit),
            Self::AllocationFailed {
                resource,
                requested,
            } => format_allocation_failure(formatter, *resource, *requested),
            Self::MissingRootBox { box_index } => {
                write!(formatter, "root refers to missing layout box {box_index}")
            }
            Self::InvalidBoxIdentity { slot, reported } => write!(
                formatter,
                "layout box in slot {slot} reports identity {reported}"
            ),
            Self::MissingChildBox { parent, child } => {
                write!(
                    formatter,
                    "layout box {parent} refers to missing child {child}"
                )
            }
            Self::MultipleParents { box_index } => {
                write!(formatter, "layout box {box_index} has multiple parents")
            }
            Self::RootHasParent { box_index } => {
                write!(formatter, "root layout box {box_index} has a parent")
            }
            Self::BoxCycle { box_index } => {
                write!(formatter, "layout box cycle detected at {box_index}")
            }
            Self::UnreachableBox { box_index } => {
                write!(
                    formatter,
                    "layout box {box_index} is unreachable from the root"
                )
            }
            Self::BoxesWithoutRoot { boxes } => {
                write!(
                    formatter,
                    "layout output has no root but contains {boxes} boxes"
                )
            }
            Self::LeafHasChildren { box_index } => {
                write!(formatter, "leaf layout box {box_index} has children")
            }
            Self::TextOnNonTextBox { box_index } => {
                write!(formatter, "non-text layout box {box_index} contains text")
            }
            Self::TextMissingBaseline {
                box_index,
                fragment_index,
            } => write!(
                formatter,
                "text fragment {fragment_index} on layout box {box_index} has no baseline"
            ),
            Self::NegativeGeometry {
                box_index,
                field,
                value,
            } => write!(
                formatter,
                "negative {field:?} geometry {value} for layout box {box_index:?}"
            ),
            Self::GeometryOutOfRange {
                box_index,
                field,
                value,
                limit,
            } => write!(
                formatter,
                "{field:?} geometry {value} for layout box {box_index:?} exceeds ±{limit}"
            ),
            Self::GeometryOverflow { box_index, axis } => write!(
                formatter,
                "{axis:?} geometry overflow for layout box {box_index:?}"
            ),
            Self::NonFiniteConversion { box_index, field } => write!(
                formatter,
                "non-finite {field:?} conversion for layout box {box_index:?}"
            ),
            Self::InvalidFontMetric {
                box_index,
                field,
                value,
            } => write!(
                formatter,
                "invalid {field:?} font metric {value} for layout box {box_index}"
            ),
            Self::IdentifierCapacityExceeded => {
                formatter.write_str("scene identifier capacity exceeded")
            }
            Self::SceneResolutionIdentityExhausted => {
                formatter.write_str("compiled-scene resolution identity capacity exhausted")
            }
            Self::TextResolutionSceneMismatch => {
                formatter.write_str("text resolution belongs to a different compiled scene")
            }
            Self::MissingTextResolution { pending_index } => write!(
                formatter,
                "missing resolution for pending text {pending_index}"
            ),
            Self::DuplicateTextResolution { pending_index } => write!(
                formatter,
                "duplicate resolution for pending text {pending_index}"
            ),
            Self::UnknownTextResolution {
                pending_index,
                available,
            } => write!(
                formatter,
                "pending text {pending_index} does not exist in a scene with {available} entries"
            ),
            Self::OutOfOrderTextResolution { expected, actual } => write!(
                formatter,
                "out-of-order pending text resolution {actual}; expected {expected}"
            ),
            Self::TextContentMismatch { pending_index } => write!(
                formatter,
                "shaped text does not match pending text {pending_index}"
            ),
            Self::TextMetricMismatch {
                pending_index,
                field,
                expected,
                actual,
            } => write!(
                formatter,
                "{field:?} metric {actual} for pending text {pending_index} does not match layout value {expected}"
            ),
            Self::FontInstanceNamespaceMismatch { expected, actual } => write!(
                formatter,
                "font instance belongs to WebRender namespace {actual:?}, not {expected:?}"
            ),
            Self::ResolvedTextItemMismatch { pending_index } => write!(
                formatter,
                "resolved pending text {pending_index} no longer matches its scene item"
            ),
        }
    }
}

impl std::error::Error for SceneBuildError {}

fn format_resource_limit(
    formatter: &mut fmt::Formatter<'_>,
    resource: ResourceKind,
    observed: usize,
    limit: usize,
) -> fmt::Result {
    write!(
        formatter,
        "{resource:?} resource limit exceeded: observed {observed}, limit {limit}"
    )
}

fn format_document_version_mismatch(
    formatter: &mut fmt::Formatter<'_>,
    expected: DocumentVersion,
    actual: DocumentVersion,
) -> fmt::Result {
    write!(
        formatter,
        "layout document {} revision {} does not match requested document {} revision {}",
        actual.document_id().get(),
        actual.revision(),
        expected.document_id().get(),
        expected.revision()
    )
}

fn format_allocation_failure(
    formatter: &mut fmt::Formatter<'_>,
    resource: ResourceKind,
    requested: usize,
) -> fmt::Result {
    write!(
        formatter,
        "failed to reserve {requested} units for {resource:?}"
    )
}
