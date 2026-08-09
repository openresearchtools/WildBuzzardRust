/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Exact-revision interaction state supplied by the DOM/event owner.

use std::collections::HashMap;
use std::fmt;

use wild_buzzard_dom::{DocumentId, DocumentSnapshot, NodeId, NodeKind};
use wild_buzzard_style_platform::ElementState;

/// A selector-visible state whose truth is owned outside the immutable markup tree.
///
/// Visited state is intentionally absent. The static adapter computes all links
/// as unvisited and keeps Stylo's visited-style path disabled.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SelectorState {
    /// `:active`.
    Active,
    /// `:focus`.
    Focus,
    /// `:hover`.
    Hover,
    /// `:enabled`.
    Enabled,
    /// `:disabled`.
    Disabled,
    /// `:checked`.
    Checked,
    /// `:indeterminate`.
    Indeterminate,
    /// `:placeholder-shown`.
    PlaceholderShown,
    /// `:target`.
    Target,
    /// `:fullscreen`.
    Fullscreen,
    /// `:valid`.
    Valid,
    /// `:invalid`.
    Invalid,
    /// `:user-valid`.
    UserValid,
    /// `:user-invalid`.
    UserInvalid,
    /// `:required`.
    Required,
    /// `:optional`.
    Optional,
    /// `:defined`.
    Defined,
    /// `:in-range`.
    InRange,
    /// `:out-of-range`.
    OutOfRange,
    /// `:read-only`.
    ReadOnly,
    /// `:read-write`.
    ReadWrite,
    /// `:default`.
    Default,
    /// `:focus-visible`.
    FocusVisible,
    /// `:focus-within`.
    FocusWithin,
    /// `:autofill`.
    Autofill,
    /// `:modal`.
    Modal,
    /// `:open`.
    Open,
    /// `:popover-open`.
    PopoverOpen,
}

impl SelectorState {
    fn platform(self) -> ElementState {
        match self {
            Self::Active => ElementState::ACTIVE,
            Self::Focus => ElementState::FOCUS,
            Self::Hover => ElementState::HOVER,
            Self::Enabled => ElementState::ENABLED,
            Self::Disabled => ElementState::DISABLED,
            Self::Checked => ElementState::CHECKED,
            Self::Indeterminate => ElementState::INDETERMINATE,
            Self::PlaceholderShown => ElementState::PLACEHOLDER_SHOWN,
            Self::Target => ElementState::URLTARGET,
            Self::Fullscreen => ElementState::FULLSCREEN,
            Self::Valid => ElementState::VALID,
            Self::Invalid => ElementState::INVALID,
            Self::UserValid => ElementState::USER_VALID,
            Self::UserInvalid => ElementState::USER_INVALID,
            Self::Required => ElementState::REQUIRED,
            Self::Optional => ElementState::OPTIONAL_,
            Self::Defined => ElementState::DEFINED,
            Self::InRange => ElementState::INRANGE,
            Self::OutOfRange => ElementState::OUTOFRANGE,
            Self::ReadOnly => ElementState::READONLY,
            Self::ReadWrite => ElementState::READWRITE,
            Self::Default => ElementState::DEFAULT,
            Self::FocusVisible => ElementState::FOCUSRING,
            Self::FocusWithin => ElementState::FOCUS_WITHIN,
            Self::Autofill => ElementState::AUTOFILL,
            Self::Modal => ElementState::MODAL,
            Self::Open => ElementState::OPEN,
            Self::PopoverOpen => ElementState::POPOVER_OPEN,
        }
    }
}

/// Validated selector state for one element.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ElementSelectorState {
    state: ElementState,
}

impl Default for ElementSelectorState {
    fn default() -> Self {
        Self {
            state: ElementState::empty(),
        }
    }
}

impl ElementSelectorState {
    /// No dynamic or DOM-derived state.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Adds one state flag. Contradictory pairs are rejected when the
    /// revisioned snapshot is constructed.
    #[must_use]
    pub fn with(mut self, state: SelectorState) -> Self {
        self.state.insert(state.platform());
        self
    }

    pub(crate) const fn platform(self) -> ElementState {
        self.state
    }

    fn conflicting_pair(self) -> Option<(&'static str, &'static str)> {
        const PAIRS: [(ElementState, &str, ElementState, &str); 6] = [
            (
                ElementState::ENABLED,
                "enabled",
                ElementState::DISABLED,
                "disabled",
            ),
            (
                ElementState::REQUIRED,
                "required",
                ElementState::OPTIONAL_,
                "optional",
            ),
            (
                ElementState::VALID,
                "valid",
                ElementState::INVALID,
                "invalid",
            ),
            (
                ElementState::USER_VALID,
                "user-valid",
                ElementState::USER_INVALID,
                "user-invalid",
            ),
            (
                ElementState::INRANGE,
                "in-range",
                ElementState::OUTOFRANGE,
                "out-of-range",
            ),
            (
                ElementState::READONLY,
                "read-only",
                ElementState::READWRITE,
                "read-write",
            ),
        ];
        PAIRS
            .into_iter()
            .find_map(|(left, left_name, right, right_name)| {
                self.state
                    .contains(left | right)
                    .then_some((left_name, right_name))
            })
    }
}

/// Validation failure for an exact-revision selector-state publication.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SelectorStateSnapshotError {
    /// Sparse state-map allocation failed.
    AllocationFailed {
        /// Requested entry capacity.
        requested: usize,
    },
    /// An entry carries a node handle from another document.
    WrongDocument {
        /// Rejected node.
        node: NodeId,
        /// Snapshot document.
        expected: DocumentId,
        /// Node-handle document.
        actual: DocumentId,
    },
    /// An entry references no node in the snapshot.
    UnknownNode(NodeId),
    /// An entry references a non-element node.
    NotAnElement(NodeId),
    /// More than one entry was supplied for one element.
    DuplicateState(NodeId),
    /// Mutually exclusive state flags were supplied together.
    ConflictingStates {
        /// Rejected element.
        node: NodeId,
        /// First conflicting flag.
        first: &'static str,
        /// Second conflicting flag.
        second: &'static str,
    },
    /// State and style input belong to different documents.
    DocumentMismatch {
        /// State document.
        expected: DocumentId,
        /// Style-input document.
        actual: DocumentId,
    },
    /// State and style input represent different document revisions.
    RevisionMismatch {
        /// State revision.
        expected: u64,
        /// Style-input revision.
        actual: u64,
    },
}

impl fmt::Display for SelectorStateSnapshotError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AllocationFailed { requested } => {
                write!(
                    formatter,
                    "could not reserve {requested} selector-state entries"
                )
            }
            Self::WrongDocument {
                node,
                expected,
                actual,
            } => write!(
                formatter,
                "state for node slot {} belongs to document {}, expected {}",
                node.slot(),
                actual.get(),
                expected.get()
            ),
            Self::UnknownNode(node) => {
                write!(
                    formatter,
                    "state references unknown node slot {}",
                    node.slot()
                )
            }
            Self::NotAnElement(node) => write!(
                formatter,
                "state references non-element node slot {}",
                node.slot()
            ),
            Self::DuplicateState(node) => write!(
                formatter,
                "duplicate selector-state entry for node slot {}",
                node.slot()
            ),
            Self::ConflictingStates {
                node,
                first,
                second,
            } => write!(
                formatter,
                "node slot {} cannot be both {first} and {second}",
                node.slot()
            ),
            Self::DocumentMismatch { expected, actual } => write!(
                formatter,
                "selector state belongs to document {}, input is document {}",
                expected.get(),
                actual.get()
            ),
            Self::RevisionMismatch { expected, actual } => write!(
                formatter,
                "selector-state revision {expected} does not match input revision {actual}"
            ),
        }
    }
}

impl std::error::Error for SelectorStateSnapshotError {}

/// Sparse, immutable selector state tied to one exact DOM snapshot revision.
#[derive(Clone, Debug)]
pub struct SelectorStateSnapshot {
    document_id: DocumentId,
    document_revision: u64,
    states: HashMap<NodeId, ElementSelectorState>,
}

impl SelectorStateSnapshot {
    /// Validates all state entries before publishing the immutable map.
    ///
    /// # Errors
    ///
    /// Returns an error when an entry belongs to another document, does not
    /// identify an element in this revision, duplicates a node, contains a
    /// contradictory state pair, or the sparse map cannot be allocated.
    pub fn try_new(
        snapshot: &DocumentSnapshot,
        entries: impl IntoIterator<Item = (NodeId, ElementSelectorState)>,
    ) -> Result<Self, SelectorStateSnapshotError> {
        let entries = entries.into_iter();
        let requested = entries.size_hint().0;
        let mut states = HashMap::new();
        states
            .try_reserve(requested)
            .map_err(|_| SelectorStateSnapshotError::AllocationFailed { requested })?;
        for (node, state) in entries {
            if node.document_id() != snapshot.document_id() {
                return Err(SelectorStateSnapshotError::WrongDocument {
                    node,
                    expected: snapshot.document_id(),
                    actual: node.document_id(),
                });
            }
            let source = snapshot
                .node(node)
                .ok_or(SelectorStateSnapshotError::UnknownNode(node))?;
            if !matches!(source.kind, NodeKind::Element(_)) {
                return Err(SelectorStateSnapshotError::NotAnElement(node));
            }
            if let Some((first, second)) = state.conflicting_pair() {
                return Err(SelectorStateSnapshotError::ConflictingStates {
                    node,
                    first,
                    second,
                });
            }
            if states.insert(node, state).is_some() {
                return Err(SelectorStateSnapshotError::DuplicateState(node));
            }
        }
        Ok(Self {
            document_id: snapshot.document_id(),
            document_revision: snapshot.revision(),
            states,
        })
    }

    pub(crate) fn validate_for(
        &self,
        snapshot: &DocumentSnapshot,
    ) -> Result<(), SelectorStateSnapshotError> {
        if self.document_id != snapshot.document_id() {
            return Err(SelectorStateSnapshotError::DocumentMismatch {
                expected: self.document_id,
                actual: snapshot.document_id(),
            });
        }
        if self.document_revision != snapshot.revision() {
            return Err(SelectorStateSnapshotError::RevisionMismatch {
                expected: self.document_revision,
                actual: snapshot.revision(),
            });
        }
        Ok(())
    }

    pub(crate) fn get(&self, node: NodeId) -> ElementState {
        self.states
            .get(&node)
            .copied()
            .unwrap_or_default()
            .platform()
    }
}
