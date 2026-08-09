//! Host-side contract for a future JavaScript binding layer.
//!
//! `NodeId` is a stable lookup key, but it is deliberately **not** a GC root.
//! A JavaScript runtime adapter must produce an owned `RootedNodeHandle` whose
//! lifetime keeps the host document alive and whose tracing implementation
//! reports every live DOM edge. This keeps unrooted engine pointers out of the
//! public boundary without making this crate depend on a particular JS engine.

use crate::{Document, DocumentId, DocumentSnapshot, DocumentVersion, DomError, NodeId};
use std::fmt;

/// An owned, traceable reference maintained by a JS/DOM integration adapter.
///
/// Implementations must keep the referenced document alive until the last
/// clone is dropped. They must not contain an unrooted pointer into either the
/// DOM arena or a moving garbage-collected heap.
pub trait RootedNodeHandle: Clone + Send + Sync + 'static {
    /// The stable identity of the owning document.
    fn document_id(&self) -> DocumentId;

    /// The stable node lookup key within that document.
    fn node_id(&self) -> NodeId;
}

/// Capability supplied by the eventual JS/DOM integration layer.
///
/// No implementation is provided for `NodeId`: turning a lookup key into a
/// root is an ownership operation that only the embedding can perform.
pub trait DomRootProvider {
    type Root: RootedNodeHandle;
    type Error;

    /// Acquires an owned root after checking document identity and liveness.
    fn root_node(&self, node: NodeId) -> Result<Self::Root, Self::Error>;

    /// Returns whether an existing root still denotes a host-visible node.
    fn is_live(&self, root: &Self::Root) -> bool;
}

/// A visitor used by binding objects to expose their rooted host edges.
pub trait DomRootTrace {
    /// Visits every DOM root held by the binding object exactly once per edge.
    fn trace_dom_roots(&self, visitor: &mut dyn FnMut(NodeId));
}

/// Identifies one node created by a command earlier in the same script batch.
///
/// Tokens are dense, zero-based command data rather than persistent DOM
/// identities. They are never roots and cannot be used outside their batch.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CreatedNodeToken(u32);

impl CreatedNodeToken {
    /// Creates the token expected for the given zero-based creation ordinal.
    #[must_use]
    pub const fn from_index(index: u32) -> Self {
        Self(index)
    }

    /// Returns the zero-based creation ordinal encoded by this token.
    #[must_use]
    pub const fn index(self) -> u32 {
        self.0
    }
}

/// A node operand in a script mutation batch.
///
/// `Existing` is checked against the exact target document before every use.
/// `Created` must refer to a successful create command earlier in the batch.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ScriptNode {
    Existing(NodeId),
    Created(CreatedNodeToken),
}

impl From<NodeId> for ScriptNode {
    fn from(value: NodeId) -> Self {
        Self::Existing(value)
    }
}

impl From<CreatedNodeToken> for ScriptNode {
    fn from(value: CreatedNodeToken) -> Self {
        Self::Created(value)
    }
}

/// One engine-neutral DOM command issued by a future JavaScript adapter.
///
/// HTML element creation always uses the HTML namespace. HTML attributes use
/// a null namespace and ASCII-lowercased local name, matching the existing
/// `Document` HTML helpers. No generic namespace is inferred at this boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ScriptMutationCommand {
    CreateHtmlElement {
        token: CreatedNodeToken,
        local_name: String,
    },
    CreateText {
        token: CreatedNodeToken,
        data: String,
    },
    AppendChild {
        parent: ScriptNode,
        child: ScriptNode,
    },
    InsertBefore {
        parent: ScriptNode,
        child: ScriptNode,
        reference: Option<ScriptNode>,
    },
    SetHtmlAttribute {
        element: ScriptNode,
        local_name: String,
        value: String,
    },
    RemoveHtmlAttribute {
        element: ScriptNode,
        local_name: String,
    },
    SetCharacterData {
        node: ScriptNode,
        data: String,
    },
    RemoveChild {
        parent: ScriptNode,
        child: ScriptNode,
    },
}

/// An owned mutation request tied to one exact document state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScriptMutationBatch {
    expected_version: DocumentVersion,
    commands: Vec<ScriptMutationCommand>,
}

impl ScriptMutationBatch {
    #[must_use]
    pub fn new(expected_version: DocumentVersion, commands: Vec<ScriptMutationCommand>) -> Self {
        Self {
            expected_version,
            commands,
        }
    }

    #[must_use]
    pub const fn expected_version(&self) -> DocumentVersion {
        self.expected_version
    }

    #[must_use]
    pub fn commands(&self) -> &[ScriptMutationCommand] {
        &self.commands
    }
}

/// Resource dimension constrained by script mutation limits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScriptMutationLimitKind {
    Commands,
    CreatedNodes,
    StringBytes,
    TotalStringBytes,
}

impl fmt::Display for ScriptMutationLimitKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Commands => formatter.write_str("commands"),
            Self::CreatedNodes => formatter.write_str("created nodes"),
            Self::StringBytes => formatter.write_str("bytes in one string"),
            Self::TotalStringBytes => formatter.write_str("total string bytes"),
        }
    }
}

/// Rejection returned when caller-selected limits exceed process hard caps.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScriptMutationLimitConfigurationError {
    pub kind: ScriptMutationLimitKind,
    pub requested: usize,
    pub hard_maximum: usize,
}

impl fmt::Display for ScriptMutationLimitConfigurationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "requested {} limit {} exceeds hard maximum {}",
            self.kind, self.requested, self.hard_maximum
        )
    }
}

impl std::error::Error for ScriptMutationLimitConfigurationError {}

/// Checked per-batch limits which can only narrow process hard caps.
///
/// Zero is valid for every field and forbids that resource. An empty batch is
/// still rejected as `ScriptMutationError::EmptyBatch`, independently of the
/// command limit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScriptMutationLimits {
    max_commands: usize,
    max_created_nodes: usize,
    max_string_bytes: usize,
    max_total_string_bytes: usize,
}

impl ScriptMutationLimits {
    pub const HARD_MAX_COMMANDS: usize = 4_096;
    pub const HARD_MAX_CREATED_NODES: usize = 2_048;
    pub const HARD_MAX_STRING_BYTES: usize = 1024 * 1024;
    pub const HARD_MAX_TOTAL_STRING_BYTES: usize = 4 * 1024 * 1024;

    pub const DEFAULT: Self = Self {
        max_commands: Self::HARD_MAX_COMMANDS,
        max_created_nodes: Self::HARD_MAX_CREATED_NODES,
        max_string_bytes: Self::HARD_MAX_STRING_BYTES,
        max_total_string_bytes: Self::HARD_MAX_TOTAL_STRING_BYTES,
    };

    /// Constructs limits no greater than the fixed process hard caps.
    pub const fn try_new(
        max_commands: usize,
        max_created_nodes: usize,
        max_string_bytes: usize,
        max_total_string_bytes: usize,
    ) -> Result<Self, ScriptMutationLimitConfigurationError> {
        if max_commands > Self::HARD_MAX_COMMANDS {
            return Err(ScriptMutationLimitConfigurationError {
                kind: ScriptMutationLimitKind::Commands,
                requested: max_commands,
                hard_maximum: Self::HARD_MAX_COMMANDS,
            });
        }
        if max_created_nodes > Self::HARD_MAX_CREATED_NODES {
            return Err(ScriptMutationLimitConfigurationError {
                kind: ScriptMutationLimitKind::CreatedNodes,
                requested: max_created_nodes,
                hard_maximum: Self::HARD_MAX_CREATED_NODES,
            });
        }
        if max_string_bytes > Self::HARD_MAX_STRING_BYTES {
            return Err(ScriptMutationLimitConfigurationError {
                kind: ScriptMutationLimitKind::StringBytes,
                requested: max_string_bytes,
                hard_maximum: Self::HARD_MAX_STRING_BYTES,
            });
        }
        if max_total_string_bytes > Self::HARD_MAX_TOTAL_STRING_BYTES {
            return Err(ScriptMutationLimitConfigurationError {
                kind: ScriptMutationLimitKind::TotalStringBytes,
                requested: max_total_string_bytes,
                hard_maximum: Self::HARD_MAX_TOTAL_STRING_BYTES,
            });
        }
        Ok(Self {
            max_commands,
            max_created_nodes,
            max_string_bytes,
            max_total_string_bytes,
        })
    }

    #[must_use]
    pub const fn max_commands(self) -> usize {
        self.max_commands
    }

    #[must_use]
    pub const fn max_created_nodes(self) -> usize {
        self.max_created_nodes
    }

    #[must_use]
    pub const fn max_string_bytes(self) -> usize {
        self.max_string_bytes
    }

    #[must_use]
    pub const fn max_total_string_bytes(self) -> usize {
        self.max_total_string_bytes
    }
}

impl Default for ScriptMutationLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Batch-local token misuse.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScriptMutationTokenError {
    CreationOrder {
        expected: CreatedNodeToken,
        actual: CreatedNodeToken,
    },
    Unavailable {
        token: CreatedNodeToken,
        available_created_nodes: usize,
    },
}

impl fmt::Display for ScriptMutationTokenError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CreationOrder { expected, actual } => write!(
                formatter,
                "creation token {} must be the next dense token {}",
                actual.index(),
                expected.index()
            ),
            Self::Unavailable {
                token,
                available_created_nodes,
            } => write!(
                formatter,
                "created-node token {} is unavailable; {} nodes have been created",
                token.index(),
                available_created_nodes
            ),
        }
    }
}

/// Atomic script mutation failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ScriptMutationError {
    EmptyBatch,
    VersionMismatch {
        expected: DocumentVersion,
        actual: DocumentVersion,
    },
    RevisionExhausted {
        version: DocumentVersion,
    },
    LimitExceeded {
        command_index: usize,
        kind: ScriptMutationLimitKind,
        limit: usize,
        actual: usize,
    },
    Token {
        command_index: usize,
        error: ScriptMutationTokenError,
    },
    Command {
        command_index: usize,
        error: DomError,
    },
    Finalization(DomError),
}

impl fmt::Display for ScriptMutationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyBatch => formatter.write_str("script mutation batch is empty"),
            Self::VersionMismatch { expected, actual } => write!(
                formatter,
                "expected document {} revision {}, found document {} revision {}",
                expected.document_id().get(),
                expected.revision(),
                actual.document_id().get(),
                actual.revision()
            ),
            Self::RevisionExhausted { version } => write!(
                formatter,
                "document {} revision space is exhausted at {}",
                version.document_id().get(),
                version.revision()
            ),
            Self::LimitExceeded {
                command_index,
                kind,
                limit,
                actual,
            } => write!(
                formatter,
                "command {command_index} exceeds {kind} limit {limit} with {actual}"
            ),
            Self::Token {
                command_index,
                error,
            } => write!(formatter, "command {command_index}: {error}"),
            Self::Command {
                command_index,
                error,
            } => write!(formatter, "command {command_index}: {error}"),
            Self::Finalization(error) => {
                write!(formatter, "cannot finalize mutation batch: {error}")
            }
        }
    }
}

impl std::error::Error for ScriptMutationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Command { error, .. } | Self::Finalization(error) => Some(error),
            _ => None,
        }
    }
}

/// The one immutable state published by a successful script mutation batch.
#[derive(Clone, Debug)]
pub struct ScriptMutationCommit {
    snapshot: DocumentSnapshot,
    created_nodes: Vec<NodeId>,
}

impl ScriptMutationCommit {
    #[must_use]
    pub const fn version(&self) -> DocumentVersion {
        self.snapshot.version()
    }

    #[must_use]
    pub fn snapshot(&self) -> &DocumentSnapshot {
        &self.snapshot
    }

    #[must_use]
    pub fn into_snapshot(self) -> DocumentSnapshot {
        self.snapshot
    }

    /// Resolves a token from this commit. Tokens from another batch are not
    /// distinguishable by value, so callers must retain them only with their
    /// owning commit/batch protocol state.
    #[must_use]
    pub fn created_node(&self, token: CreatedNodeToken) -> Option<NodeId> {
        // The supported product target is Linux x86-64, where this widens.
        let index = token.index() as usize;
        self.created_nodes.get(index).copied()
    }

    #[must_use]
    pub fn created_nodes(&self) -> &[NodeId] {
        &self.created_nodes
    }
}

impl Document {
    /// Applies a bounded script batch atomically and publishes one snapshot.
    ///
    /// Every command executes against a private arena copy with the same
    /// document and node identities. Any error discards that copy, leaving the
    /// original tree and version unchanged. A successful nonempty batch
    /// advances the externally visible document revision exactly once.
    pub fn apply_script_mutations(
        &mut self,
        batch: ScriptMutationBatch,
        limits: ScriptMutationLimits,
    ) -> Result<ScriptMutationCommit, ScriptMutationError> {
        let ScriptMutationBatch {
            expected_version,
            commands,
        } = batch;

        let actual_version = self.version();
        if expected_version != actual_version {
            return Err(ScriptMutationError::VersionMismatch {
                expected: expected_version,
                actual: actual_version,
            });
        }

        if commands.is_empty() {
            return Err(ScriptMutationError::EmptyBatch);
        }

        let committed_revision = actual_version.revision().checked_add(1).ok_or(
            ScriptMutationError::RevisionExhausted {
                version: actual_version,
            },
        )?;

        if commands.len() > limits.max_commands {
            return Err(ScriptMutationError::LimitExceeded {
                command_index: limits.max_commands,
                kind: ScriptMutationLimitKind::Commands,
                limit: limits.max_commands,
                actual: commands.len(),
            });
        }

        // This is intentionally not exposed as `Clone` for `Document`: two
        // live mutable arenas with one identity must never escape this call.
        // Starting the private revision at zero keeps its bounded internal
        // per-command bumps away from the caller's possibly near-max revision.
        let mut working = Self {
            id: self.id,
            document_node: self.document_node,
            nodes: self.nodes.clone(),
            revision: 0,
        };
        let mut created_nodes = Vec::new();
        let mut total_string_bytes = 0usize;

        for (command_index, command) in commands.into_iter().enumerate() {
            apply_script_command(
                &mut working,
                &mut created_nodes,
                &mut total_string_bytes,
                limits,
                command_index,
                command,
            )?;
        }

        working.revision = committed_revision;
        let snapshot = working
            .snapshot()
            .map_err(ScriptMutationError::Finalization)?;
        *self = working;

        Ok(ScriptMutationCommit {
            snapshot,
            created_nodes,
        })
    }
}

fn apply_script_command(
    document: &mut Document,
    created_nodes: &mut Vec<NodeId>,
    total_string_bytes: &mut usize,
    limits: ScriptMutationLimits,
    command_index: usize,
    command: ScriptMutationCommand,
) -> Result<(), ScriptMutationError> {
    match command {
        ScriptMutationCommand::CreateHtmlElement { token, local_name } => {
            account_string(&local_name, total_string_bytes, limits, command_index)?;
            prepare_creation_token(token, created_nodes, limits, command_index)?;
            let node = document
                .create_html_element(&local_name)
                .map_err(|error| command_error(command_index, error))?;
            created_nodes.push(node);
        }
        ScriptMutationCommand::CreateText { token, data } => {
            account_string(&data, total_string_bytes, limits, command_index)?;
            prepare_creation_token(token, created_nodes, limits, command_index)?;
            let node = document
                .create_text(data)
                .map_err(|error| command_error(command_index, error))?;
            created_nodes.push(node);
        }
        ScriptMutationCommand::AppendChild { parent, child } => {
            let parent = resolve_script_node(document, created_nodes, parent, command_index)?;
            let child = resolve_script_node(document, created_nodes, child, command_index)?;
            document
                .append_child(parent, child)
                .map_err(|error| command_error(command_index, error))?;
        }
        ScriptMutationCommand::InsertBefore {
            parent,
            child,
            reference,
        } => {
            let parent = resolve_script_node(document, created_nodes, parent, command_index)?;
            let child = resolve_script_node(document, created_nodes, child, command_index)?;
            let reference = reference
                .map(|reference| {
                    resolve_script_node(document, created_nodes, reference, command_index)
                })
                .transpose()?;
            document
                .insert_before(parent, child, reference)
                .map_err(|error| command_error(command_index, error))?;
        }
        ScriptMutationCommand::SetHtmlAttribute {
            element,
            local_name,
            value,
        } => {
            account_string(&local_name, total_string_bytes, limits, command_index)?;
            account_string(&value, total_string_bytes, limits, command_index)?;
            let element = resolve_script_node(document, created_nodes, element, command_index)?;
            document
                .set_html_attribute(element, &local_name, value)
                .map_err(|error| command_error(command_index, error))?;
        }
        ScriptMutationCommand::RemoveHtmlAttribute {
            element,
            local_name,
        } => {
            account_string(&local_name, total_string_bytes, limits, command_index)?;
            let element = resolve_script_node(document, created_nodes, element, command_index)?;
            document
                .remove_attribute(element, None, &local_name.to_ascii_lowercase())
                .map_err(|error| command_error(command_index, error))?;
        }
        ScriptMutationCommand::SetCharacterData { node, data } => {
            account_string(&data, total_string_bytes, limits, command_index)?;
            let node = resolve_script_node(document, created_nodes, node, command_index)?;
            document
                .set_character_data(node, data)
                .map_err(|error| command_error(command_index, error))?;
        }
        ScriptMutationCommand::RemoveChild { parent, child } => {
            let parent = resolve_script_node(document, created_nodes, parent, command_index)?;
            let child = resolve_script_node(document, created_nodes, child, command_index)?;
            document
                .remove_child(parent, child)
                .map_err(|error| command_error(command_index, error))?;
        }
    }
    Ok(())
}

fn prepare_creation_token(
    token: CreatedNodeToken,
    created_nodes: &[NodeId],
    limits: ScriptMutationLimits,
    command_index: usize,
) -> Result<(), ScriptMutationError> {
    if created_nodes.len() >= limits.max_created_nodes {
        return Err(ScriptMutationError::LimitExceeded {
            command_index,
            kind: ScriptMutationLimitKind::CreatedNodes,
            limit: limits.max_created_nodes,
            actual: created_nodes.len().saturating_add(1),
        });
    }

    // The hard cap proves this conversion exactly represents the ordinal.
    let expected = CreatedNodeToken::from_index(created_nodes.len() as u32);
    if token != expected {
        return Err(ScriptMutationError::Token {
            command_index,
            error: ScriptMutationTokenError::CreationOrder {
                expected,
                actual: token,
            },
        });
    }
    Ok(())
}

fn resolve_script_node(
    document: &Document,
    created_nodes: &[NodeId],
    node: ScriptNode,
    command_index: usize,
) -> Result<NodeId, ScriptMutationError> {
    let node = match node {
        ScriptNode::Existing(node) => node,
        ScriptNode::Created(token) => {
            // The supported product target is Linux x86-64, where this widens.
            let index = token.index() as usize;
            created_nodes
                .get(index)
                .copied()
                .ok_or(ScriptMutationError::Token {
                    command_index,
                    error: ScriptMutationTokenError::Unavailable {
                        token,
                        available_created_nodes: created_nodes.len(),
                    },
                })?
        }
    };

    // Resolution checks both the embedded DocumentId and current arena slot.
    document
        .node_kind(node)
        .map_err(|error| command_error(command_index, error))?;
    Ok(node)
}

fn account_string(
    value: &str,
    total_string_bytes: &mut usize,
    limits: ScriptMutationLimits,
    command_index: usize,
) -> Result<(), ScriptMutationError> {
    let bytes = value.len();
    if bytes > limits.max_string_bytes {
        return Err(ScriptMutationError::LimitExceeded {
            command_index,
            kind: ScriptMutationLimitKind::StringBytes,
            limit: limits.max_string_bytes,
            actual: bytes,
        });
    }
    let next_total =
        total_string_bytes
            .checked_add(bytes)
            .ok_or(ScriptMutationError::LimitExceeded {
                command_index,
                kind: ScriptMutationLimitKind::TotalStringBytes,
                limit: limits.max_total_string_bytes,
                actual: usize::MAX,
            })?;
    if next_total > limits.max_total_string_bytes {
        return Err(ScriptMutationError::LimitExceeded {
            command_index,
            kind: ScriptMutationLimitKind::TotalStringBytes,
            limit: limits.max_total_string_bytes,
            actual: next_total,
        });
    }
    *total_string_bytes = next_total;
    Ok(())
}

fn command_error(command_index: usize, error: DomError) -> ScriptMutationError {
    ScriptMutationError::Command {
        command_index,
        error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NodeKind;

    fn create_zero() -> ScriptMutationCommand {
        ScriptMutationCommand::CreateHtmlElement {
            token: CreatedNodeToken::from_index(0),
            local_name: "main".into(),
        }
    }

    #[test]
    fn revision_exhaustion_is_fallible_and_preserves_the_arena() {
        let mut document = Document::new();
        document.revision = u64::MAX;
        let version = document.version();
        let node_count = document.nodes.len();

        assert!(matches!(
            document.apply_script_mutations(
                ScriptMutationBatch::new(version, vec![create_zero()]),
                ScriptMutationLimits::DEFAULT,
            ),
            Err(ScriptMutationError::RevisionExhausted {
                version: error_version,
            }) if error_version == version
        ));
        assert_eq!(document.version(), version);
        assert_eq!(document.nodes.len(), node_count);
    }

    #[test]
    fn near_exhaustion_uses_a_bounded_private_revision_then_commits_max() {
        let mut document = Document::new();
        document.revision = u64::MAX - 1;
        let version = document.version();
        let document_node = document.document_node();
        let commit = document
            .apply_script_mutations(
                ScriptMutationBatch::new(
                    version,
                    vec![
                        create_zero(),
                        ScriptMutationCommand::SetHtmlAttribute {
                            element: ScriptNode::Created(CreatedNodeToken::from_index(0)),
                            local_name: "id".into(),
                            value: "last".into(),
                        },
                        ScriptMutationCommand::AppendChild {
                            parent: ScriptNode::Existing(document_node),
                            child: ScriptNode::Created(CreatedNodeToken::from_index(0)),
                        },
                    ],
                ),
                ScriptMutationLimits::DEFAULT,
            )
            .unwrap();

        assert_eq!(commit.version().revision(), u64::MAX);
        assert_eq!(document.version(), commit.version());
        assert_eq!(document.nodes.len(), 2);
    }

    #[test]
    fn unknown_same_document_node_is_an_indexed_command_error() {
        let mut document = Document::new();
        let version = document.version();
        let unknown = NodeId {
            document: document.id,
            slot: u32::MAX,
        };

        assert!(matches!(
            document.apply_script_mutations(
                ScriptMutationBatch::new(
                    version,
                    vec![ScriptMutationCommand::SetCharacterData {
                        node: ScriptNode::Existing(unknown),
                        data: "unreachable".into(),
                    }],
                ),
                ScriptMutationLimits::DEFAULT,
            ),
            Err(ScriptMutationError::Command {
                command_index: 0,
                error: DomError::UnknownNode(node),
            }) if node == unknown
        ));
        assert_eq!(document.version(), version);
        assert_eq!(document.nodes.len(), 1);
    }

    #[test]
    fn final_snapshot_failure_discards_the_private_copy_without_panicking() {
        let mut document = Document::new();
        let child = document.create_html_element("html").unwrap();
        document.nodes[0].children.push(child);
        let version = document.version();
        let original_parent = document.nodes[child.slot() as usize].parent;

        assert!(matches!(
            document.apply_script_mutations(
                ScriptMutationBatch::new(
                    version,
                    vec![ScriptMutationCommand::SetHtmlAttribute {
                        element: ScriptNode::Existing(child),
                        local_name: "id".into(),
                        value: "private".into(),
                    }],
                ),
                ScriptMutationLimits::DEFAULT,
            ),
            Err(ScriptMutationError::Finalization(
                DomError::SnapshotInvariant("parent/child links disagree")
            ))
        ));
        assert_eq!(document.version(), version);
        assert_eq!(
            document.nodes[child.slot() as usize].parent,
            original_parent
        );
        assert!(matches!(
            document.node_kind(child),
            Ok(NodeKind::Element(data)) if data.html_attribute("id").is_none()
        ));
    }
}
