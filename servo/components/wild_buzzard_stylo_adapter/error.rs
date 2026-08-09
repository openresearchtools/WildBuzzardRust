/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::fmt;

use wild_buzzard_dom::NodeId;
use wild_buzzard_layout::ComputedStyleSnapshotError;

use crate::state::SelectorStateSnapshotError;

/// A Stylo computed value that the bounded wave-two layout model cannot yet consume.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UnsupportedComputedValue {
    /// A display type other than none, flow block, or flow inline.
    Display(String),
    /// Automatic physical margins are not represented by the current layout model.
    AutomaticMargin(&'static str),
    /// A non-finite or out-of-range length/percentage was produced.
    LengthPercentage(&'static str),
    /// An intrinsic, anchor-dependent, or otherwise unsupported sizing value.
    Sizing(&'static str, String),
    /// The current layout model cannot represent this white-space behavior.
    WhiteSpace(String),
    /// The current layout model cannot flatten this color into one sRGB color.
    Color(&'static str),
}

/// Structured failure from snapshot construction, Stylo preparation, or translation.
#[derive(Debug)]
pub enum StyleAdapterError {
    /// The immutable input contains more nodes than configured.
    NodeLimitExceeded {
        /// Configured maximum.
        limit: usize,
        /// Observed count.
        actual: usize,
    },
    /// The immutable input contains more elements than configured side-table entries.
    ElementLimitExceeded {
        /// Configured maximum.
        limit: usize,
        /// Observed count.
        actual: usize,
    },
    /// A node exceeds the configured immutable traversal depth.
    TreeDepthLimitExceeded {
        /// Configured maximum.
        limit: usize,
        /// First node beyond the maximum.
        node: NodeId,
    },
    /// A bounded snapshot resource exceeds its configured maximum.
    SnapshotResourceLimitExceeded {
        /// Stable resource label.
        resource: &'static str,
        /// Configured maximum.
        limit: usize,
        /// Observed amount.
        actual: usize,
    },
    /// A first-party collection could not reserve its bounded capacity.
    AllocationFailed {
        /// Collection being reserved.
        resource: &'static str,
        /// Requested capacity.
        requested: usize,
    },
    /// More author style elements were found than configured.
    StylesheetLimitExceeded {
        /// Configured maximum.
        limit: usize,
    },
    /// Author stylesheet text exceeds the configured aggregate byte limit.
    StylesheetByteLimitExceeded {
        /// Configured maximum.
        limit: usize,
    },
    /// Inline declaration text exceeds the configured aggregate byte limit.
    InlineStyleByteLimitExceeded {
        /// Configured maximum.
        limit: usize,
    },
    /// Stylo produced more selectors than configured.
    SelectorLimitExceeded {
        /// Configured maximum.
        limit: usize,
        /// Parsed selector count.
        actual: usize,
    },
    /// Stylo produced more declarations than configured.
    DeclarationLimitExceeded {
        /// Configured maximum.
        limit: usize,
        /// Parsed declaration count.
        actual: usize,
    },
    /// The conservative bounded selector-work budget was exceeded.
    SelectorWorkLimitExceeded {
        /// Configured maximum.
        limit: usize,
        /// Conservative estimated work.
        estimated: usize,
    },
    /// Arithmetic needed to evaluate a resource bound overflowed.
    ResourceBoundOverflow,
    /// The caller thread was initialized for a non-layout Stylo role.
    IncompatibleThreadState {
        /// Raw imported `ThreadState` bits for diagnostics.
        current: u32,
    },
    /// `@import` is deliberately unavailable because no loader/network seam is installed.
    ImportRuleProhibited {
        /// Owning style element.
        node: NodeId,
    },
    /// A node relationship references an absent source node.
    MissingRelation {
        /// Node whose relation is malformed.
        node: NodeId,
        /// Relationship kind.
        relation: &'static str,
        /// Referenced node that is absent.
        target: NodeId,
    },
    /// An internal snapshot relationship did not resolve.
    SnapshotInvariant(&'static str),
    /// The computed value cannot be represented without fabricating layout behavior.
    UnsupportedComputedValue {
        /// Element whose computed value could not be translated.
        node: NodeId,
        /// Unsupported value category.
        value: UnsupportedComputedValue,
    },
    /// Publication validation rejected the produced layout-facing map.
    Publication(ComputedStyleSnapshotError),
    /// Selector-state validation rejected the supplied exact-revision map.
    SelectorState(SelectorStateSnapshotError),
}

impl fmt::Display for StyleAdapterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NodeLimitExceeded { limit, actual } => {
                write!(
                    formatter,
                    "DOM snapshot has {actual} nodes; limit is {limit}"
                )
            }
            Self::ElementLimitExceeded { limit, actual } => {
                write!(
                    formatter,
                    "DOM snapshot has {actual} elements; limit is {limit}"
                )
            }
            Self::TreeDepthLimitExceeded { limit, node } => write!(
                formatter,
                "node slot {} exceeds style tree depth limit {limit}",
                node.slot()
            ),
            Self::SnapshotResourceLimitExceeded {
                resource,
                limit,
                actual,
            } => write!(
                formatter,
                "snapshot {resource} amount {actual} exceeds limit {limit}"
            ),
            Self::AllocationFailed {
                resource,
                requested,
            } => write!(
                formatter,
                "could not reserve {requested} entries for style {resource}"
            ),
            Self::StylesheetLimitExceeded { limit } => {
                write!(formatter, "author stylesheet count exceeds limit {limit}")
            }
            Self::StylesheetByteLimitExceeded { limit } => {
                write!(formatter, "author stylesheet bytes exceed limit {limit}")
            }
            Self::InlineStyleByteLimitExceeded { limit } => {
                write!(formatter, "inline style bytes exceed limit {limit}")
            }
            Self::SelectorLimitExceeded { limit, actual } => {
                write!(
                    formatter,
                    "Stylo produced {actual} selectors; limit is {limit}"
                )
            }
            Self::DeclarationLimitExceeded { limit, actual } => {
                write!(
                    formatter,
                    "Stylo produced {actual} declarations; limit is {limit}"
                )
            }
            Self::SelectorWorkLimitExceeded { limit, estimated } => write!(
                formatter,
                "estimated selector work {estimated} exceeds limit {limit}"
            ),
            Self::ResourceBoundOverflow => {
                formatter.write_str("style resource-bound arithmetic overflowed")
            }
            Self::IncompatibleThreadState { current } => write!(
                formatter,
                "Stylo adapter requires a layout-capable thread; current state bits are {current:#x}"
            ),
            Self::ImportRuleProhibited { node } => write!(
                formatter,
                "style element at node slot {} contains prohibited @import",
                node.slot()
            ),
            Self::MissingRelation {
                node,
                relation,
                target,
            } => write!(
                formatter,
                "node slot {} has {relation} relation to absent node slot {}",
                node.slot(),
                target.slot()
            ),
            Self::SnapshotInvariant(message) => formatter.write_str(message),
            Self::UnsupportedComputedValue { node, value } => write!(
                formatter,
                "node slot {} has unsupported computed value {value:?}",
                node.slot()
            ),
            Self::Publication(error) => error.fmt(formatter),
            Self::SelectorState(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for StyleAdapterError {}

impl From<ComputedStyleSnapshotError> for StyleAdapterError {
    fn from(error: ComputedStyleSnapshotError) -> Self {
        Self::Publication(error)
    }
}

impl From<SelectorStateSnapshotError> for StyleAdapterError {
    fn from(error: SelectorStateSnapshotError) -> Self {
        Self::SelectorState(error)
    }
}
