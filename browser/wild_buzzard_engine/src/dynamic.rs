use std::fmt;

use wild_buzzard_dom::bindings::{CreatedNodeToken, ScriptMutationCommit, ScriptMutationError};
use wild_buzzard_dom::{Document, DocumentVersion, DomError, NodeId};
use wild_buzzard_headless::RgbaFrame;

use crate::{PipelineError, TextEvidence};

/// Opaque allocation proof for the node mapping of one committed DOM batch.
///
/// [`ScriptMutationCommit`] has private fields and is produced only by
/// `Document::apply_script_mutations`. Consuming it here prevents a custom
/// executor from substituting pre-existing or duplicate node identities into
/// a successful worker outcome.
#[derive(Clone, Debug)]
pub struct DocumentMutationCommit {
    version: DocumentVersion,
    created_nodes: Box<[NodeId]>,
}

impl DocumentMutationCommit {
    /// Consumes an actual DOM transaction commit into worker publication proof.
    #[must_use]
    #[allow(clippy::needless_pass_by_value)]
    pub fn from_script_commit(commit: ScriptMutationCommit) -> Self {
        Self {
            version: commit.version(),
            created_nodes: commit.created_nodes().to_vec().into_boxed_slice(),
        }
    }

    /// Exact committed document version.
    #[must_use]
    pub const fn version(&self) -> DocumentVersion {
        self.version
    }

    /// Dense transaction-created mapping in token-index order.
    #[must_use]
    pub fn created_nodes(&self) -> &[NodeId] {
        &self.created_nodes
    }

    pub(crate) fn into_created_nodes(self) -> Box<[NodeId]> {
        self.created_nodes
    }
}

/// Downstream evidence for one exact live-document rendering.
///
/// Unlike [`crate::PipelineEvidence`], this type deliberately carries no HTTP
/// or parser fields: applying a mutation batch does not fetch or parse the
/// document again.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DynamicRenderEvidence {
    /// Exact document identity and revision consumed by every downstream stage.
    pub document_version: DocumentVersion,
    /// Immutable nodes in the committed DOM snapshot.
    pub dom_nodes: usize,
    /// Native imported-Stylo computed-style entries.
    pub stylo_style_entries: usize,
    /// Recoverable CSS diagnostics retained by the Stylo adapter.
    pub style_diagnostics: usize,
    /// CSS diagnostics dropped at the configured bound.
    pub dropped_style_diagnostics: usize,
    /// Layout boxes published for the exact style revision.
    pub layout_boxes: usize,
    /// Recoverable layout warnings.
    pub layout_warnings: usize,
    /// Validated renderer-independent scene items.
    pub scene_items: usize,
    /// Serialized bytes in the pending-text display list before composition.
    pub pre_composition_display_list_bytes: usize,
}

/// The engine-owned mutable document behind the synchronous dynamic-page seam.
///
/// The arena and its mutable methods remain private. Callers can recover
/// stable lookup keys needed to prepare a future rooted-host mutation batch,
/// but cannot mutate the document except through the bounded exact-version
/// transaction on [`crate::StaticPageEngine`].
pub struct LiveDocumentPage {
    pub(crate) document: Document,
    pub(crate) last_returned_frame_version: DocumentVersion,
}

impl LiveDocumentPage {
    pub(crate) fn new(document: Document, last_returned_frame_version: DocumentVersion) -> Self {
        debug_assert_eq!(document.version(), last_returned_frame_version);
        Self {
            document,
            last_returned_frame_version,
        }
    }

    /// Exact identity and revision of the mutable DOM.
    #[must_use]
    pub const fn live_version(&self) -> DocumentVersion {
        self.document.version()
    }

    /// Exact DOM revision represented by the last successfully returned frame.
    #[must_use]
    pub const fn last_returned_frame_version(&self) -> DocumentVersion {
        self.last_returned_frame_version
    }

    /// Current document element, when one exists.
    #[must_use]
    pub fn document_element(&self) -> Option<NodeId> {
        self.document.document_element()
    }

    /// Finds one live element by its current HTML `id` value.
    ///
    /// # Errors
    ///
    /// Returns a DOM invariant error if traversal encounters invalid arena
    /// state.
    pub fn element_by_id(&self, id: &str) -> Result<Option<NodeId>, DomError> {
        self.document.element_by_id(id)
    }
}

/// Successful mutation, full recomputation, and composed rendering.
#[derive(Debug)]
pub struct RenderedDocumentUpdate {
    /// Live DOM revision before the committed batch.
    pub previous_live_version: DocumentVersion,
    /// Exact revision represented by the frame returned before this update.
    pub previous_last_returned_frame_version: DocumentVersion,
    /// Downstream evidence for the newly live revision and returned frame.
    pub evidence: DynamicRenderEvidence,
    /// Text measurement and shaping evidence for the new revision.
    pub text: TextEvidence,
    /// One complete composed frame for the new revision.
    pub frame: RgbaFrame,
    pub(crate) commit: DocumentMutationCommit,
}

impl RenderedDocumentUpdate {
    /// Resolves one dense created-node token from this exact committed batch.
    #[must_use]
    pub fn created_node(&self, token: CreatedNodeToken) -> Option<NodeId> {
        self.commit
            .created_nodes()
            .get(token.index() as usize)
            .copied()
    }

    /// Dense created-node mapping in token-index order.
    #[must_use]
    pub fn created_nodes(&self) -> &[NodeId] {
        self.commit.created_nodes()
    }

    pub(crate) fn new(
        previous_live_version: DocumentVersion,
        previous_last_returned_frame_version: DocumentVersion,
        evidence: DynamicRenderEvidence,
        text: TextEvidence,
        frame: RgbaFrame,
        commit: DocumentMutationCommit,
    ) -> Self {
        Self {
            previous_live_version,
            previous_last_returned_frame_version,
            evidence,
            text,
            frame,
            commit,
        }
    }
}

/// Successful recomputation of the current exact live DOM revision.
///
/// Rerendering does not fetch, parse, mutate the DOM, create nodes, or advance
/// its revision. It only returns a fresh composed frame for the version named
/// in [`Self::evidence`].
#[derive(Debug)]
pub struct RenderedLiveDocument {
    /// Exact revision represented by the frame returned before this rerender.
    pub previous_last_returned_frame_version: DocumentVersion,
    /// Downstream evidence for the unchanged live DOM revision.
    pub evidence: DynamicRenderEvidence,
    /// Text measurement and shaping evidence for the unchanged revision.
    pub text: TextEvidence,
    /// One fresh complete composed frame for the unchanged revision.
    pub frame: RgbaFrame,
}

/// Why a dynamic update was rejected before changing the live DOM.
#[derive(Debug)]
pub enum DocumentUpdateRejection {
    /// No successfully loaded document is currently retained.
    NoLiveDocument,
    /// The renderer was already terminally unusable before this operation.
    RendererUnavailable,
    /// The caller did not name the exact current live revision for a rerender.
    LiveVersionMismatch {
        /// Version required by the caller.
        expected: DocumentVersion,
        /// Version currently retained by the engine.
        actual: DocumentVersion,
    },
    /// A pre-commit control check or no-mutation rerender pipeline stage failed.
    Pipeline(PipelineError),
    /// The exact-version bounded mutation batch was rejected atomically.
    Mutation(ScriptMutationError),
}

impl fmt::Display for DocumentUpdateRejection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoLiveDocument => formatter.write_str("no live document is retained"),
            Self::RendererUnavailable => formatter
                .write_str("the headless renderer is unusable; tear down and reload the engine"),
            Self::LiveVersionMismatch { expected, actual } => write!(
                formatter,
                "rerender expected document {} revision {}, but live document is {} revision {}",
                expected.document_id().get(),
                expected.revision(),
                actual.document_id().get(),
                actual.revision(),
            ),
            Self::Pipeline(error) => error.fmt(formatter),
            Self::Mutation(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for DocumentUpdateRejection {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NoLiveDocument | Self::RendererUnavailable | Self::LiveVersionMismatch { .. } => {
                None
            }
            Self::Pipeline(error) => Some(error),
            Self::Mutation(error) => Some(error),
        }
    }
}

/// Two-phase dynamic update failure.
///
/// `Rejected` guarantees that this call committed no DOM mutation. A rerender
/// can therefore be rejected even if a post-send renderer failure made the
/// renderer's internal surface indeterminate. `Committed` means a mutation
/// batch advanced the live document once, but this call returned no replacement
/// frame. It never claims that an internal renderer surface was rolled back.
#[derive(Debug)]
pub enum DocumentUpdateError {
    /// This call committed no DOM mutation.
    Rejected {
        /// Current live version, absent only when no document was loaded.
        live_version: Option<DocumentVersion>,
        /// Revision represented by the last returned frame, absent only when no document was loaded.
        last_returned_frame_version: Option<DocumentVersion>,
        /// Exact rejection reason.
        reason: DocumentUpdateRejection,
    },
    /// DOM committed, but this call returned no replacement frame.
    Committed {
        /// Live version before the batch.
        previous_live_version: DocumentVersion,
        /// Revision represented by the frame returned before this failed call.
        last_returned_frame_version: DocumentVersion,
        /// Unforgeable DOM-transaction allocation proof and dense token map.
        commit: DocumentMutationCommit,
        /// Failure after the irreversible DOM commit point.
        source: Box<PipelineError>,
    },
}

impl DocumentUpdateError {
    /// Resolves a created token when the DOM committed before frame return
    /// failed. Rejected updates never create nodes.
    #[must_use]
    pub fn created_node(&self, token: CreatedNodeToken) -> Option<NodeId> {
        match self {
            Self::Rejected { .. } => None,
            Self::Committed { commit, .. } => {
                commit.created_nodes().get(token.index() as usize).copied()
            }
        }
    }

    /// Dense created-node map retained after a post-commit failure.
    #[must_use]
    pub fn created_nodes(&self) -> &[NodeId] {
        match self {
            Self::Rejected { .. } => &[],
            Self::Committed { commit, .. } => commit.created_nodes(),
        }
    }
}

impl fmt::Display for DocumentUpdateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rejected { reason, .. } => {
                write!(
                    formatter,
                    "document update committed no DOM mutation: {reason}"
                )
            }
            Self::Committed {
                last_returned_frame_version,
                commit,
                source,
                ..
            } => write!(
                formatter,
                "document update committed revision {} but returned no frame after revision {}: {source}",
                commit.version().revision(),
                last_returned_frame_version.revision(),
            ),
        }
    }
}

impl std::error::Error for DocumentUpdateError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Rejected { reason, .. } => Some(reason),
            Self::Committed { source, .. } => Some(source.as_ref()),
        }
    }
}
