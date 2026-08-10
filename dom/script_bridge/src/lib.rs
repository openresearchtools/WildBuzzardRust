//! Concrete rooted adapter between Brimstone's bounded browser task capability and the first Rust
//! DOM nucleus.
//!
//! Each host operation is published synchronously through a one-command, exact-version
//! `ScriptMutationBatch`. This preserves the browser-visible successful prefix when a later DOM
//! call or JavaScript instruction throws. The task retains `Arc`-owned DOM roots across the
//! explicit microtask checkpoint, while JavaScript sees only exact numeric task tokens.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};

use brimstone_core::runtime::{
    BrowserHostCommitOutcome, BrowserHostDocumentVersion, BrowserHostError, BrowserHostNodeToken,
    BrowserHostPhaseCommit, BrowserHostTask,
};
use wild_buzzard_dom::bindings::{
    CreatedNodeToken, DomRootProvider, DomRootTrace, RootedNodeHandle, ScriptMutationBatch,
    ScriptMutationCommand, ScriptMutationError, ScriptMutationLimits, ScriptNode,
};
use wild_buzzard_dom::{Document, DocumentId, DocumentSnapshot, DocumentVersion, DomError, NodeId};

// At most 4,096 host calls can add roots, so thirteen low bits cover every admitted root index.
// The remaining forty exact-Number bits provide a process-wide, never-reused task generation.
const TOKEN_SLOT_BITS: u32 = 13;
const TOKEN_SLOT_MASK: u64 = (1 << TOKEN_SLOT_BITS) - 1;
const MAX_TASK_GENERATION: u64 = (1 << (53 - TOKEN_SLOT_BITS)) - 1;
static NEXT_TASK_GENERATION: AtomicU64 = AtomicU64::new(1);

#[derive(Debug)]
struct LiveDocument {
    current: bool,
    document: Document,
}

/// Browser-owned reference to one current document arena.
#[derive(Clone, Debug)]
pub struct ScriptDocument {
    inner: Arc<Mutex<LiveDocument>>,
}

impl ScriptDocument {
    pub fn new(document: Document) -> Self {
        Self {
            inner: Arc::new(Mutex::new(LiveDocument {
                current: true,
                document,
            })),
        }
    }

    pub fn current_version(&self) -> Result<DocumentVersion, BrowserHostError> {
        let state = self.inner.lock().map_err(|_| BrowserHostError::Internal)?;
        if !state.current {
            return Err(BrowserHostError::StaleDocument);
        }
        Ok(state.document.version())
    }

    pub fn snapshot(&self) -> Result<DocumentSnapshot, BrowserHostError> {
        let state = self.inner.lock().map_err(|_| BrowserHostError::Internal)?;
        if !state.current {
            return Err(BrowserHostError::StaleDocument);
        }
        state.document.snapshot().map_err(map_dom_read_error)
    }

    /// Apply a non-script mutation transaction, for example parser work, against the same live
    /// document. An already-started script task will detect the exact-version drift before its next
    /// host operation and fail closed.
    pub fn apply_external_mutations(
        &self,
        batch: ScriptMutationBatch,
        limits: ScriptMutationLimits,
    ) -> Result<DocumentVersion, ScriptMutationError> {
        let mut state = self.inner.lock().map_err(|_| {
            ScriptMutationError::Finalization(DomError::SnapshotInvariant(
                "script document lock is poisoned",
            ))
        })?;
        if !state.current {
            return Err(ScriptMutationError::Finalization(
                DomError::SnapshotInvariant("script document is no longer current"),
            ));
        }
        Ok(state
            .document
            .apply_script_mutations(batch, limits)?
            .version())
    }

    /// Retire the responsible document/navigation. Existing roots keep memory alive but no later
    /// browser task may mutate it through this capability.
    pub fn retire(&self) -> Result<(), BrowserHostError> {
        let mut state = self.inner.lock().map_err(|_| BrowserHostError::Internal)?;
        state.current = false;
        Ok(())
    }

    pub fn begin_task(
        &self,
        limits: ScriptMutationLimits,
    ) -> Result<RootedDomTask, BrowserHostError> {
        let expected_version = self.current_version()?;
        let generation = NEXT_TASK_GENERATION
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                (current <= MAX_TASK_GENERATION).then_some(current + 1)
            })
            .map_err(|_| BrowserHostError::Internal)?;

        Ok(RootedDomTask {
            document: self.clone(),
            expected_version,
            phase_before: expected_version,
            generation,
            roots: Vec::new(),
            limits,
            host_calls: 0,
            created_nodes: 0,
            total_string_bytes: 0,
            phase_commands: 0,
            phase_created_nodes: 0,
            retired: false,
        })
    }
}

/// Concrete host root. The `Arc` keeps the arena allocation alive; `current` and exact document
/// identity are rechecked before every use.
#[derive(Clone, Debug)]
pub struct RootedDomNode {
    document: Arc<Mutex<LiveDocument>>,
    document_id: DocumentId,
    node_id: NodeId,
}

impl RootedNodeHandle for RootedDomNode {
    fn document_id(&self) -> DocumentId {
        self.document_id
    }

    fn node_id(&self) -> NodeId {
        self.node_id
    }
}

impl DomRootTrace for RootedDomNode {
    fn trace_dom_roots(&self, visitor: &mut dyn FnMut(NodeId)) {
        visitor(self.node_id);
    }
}

impl DomRootProvider for ScriptDocument {
    type Root = RootedDomNode;
    type Error = BrowserHostError;

    fn root_node(&self, node: NodeId) -> Result<Self::Root, Self::Error> {
        let state = self.inner.lock().map_err(|_| BrowserHostError::Internal)?;
        if !state.current {
            return Err(BrowserHostError::StaleDocument);
        }
        if node.document_id() != state.document.id() {
            return Err(BrowserHostError::InvalidNode);
        }
        state
            .document
            .node_kind(node)
            .map_err(|_| BrowserHostError::InvalidNode)?;
        Ok(RootedDomNode {
            document: self.inner.clone(),
            document_id: node.document_id(),
            node_id: node,
        })
    }

    fn is_live(&self, root: &Self::Root) -> bool {
        if !Arc::ptr_eq(&self.inner, &root.document) {
            return false;
        }
        self.inner.lock().is_ok_and(|state| {
            state.current
                && state.document.id() == root.document_id
                && state.document.node_kind(root.node_id).is_ok()
        })
    }
}

/// One bounded event task retained across classic-script completion, error reporting, and the
/// caller's later explicit microtask checkpoint.
pub struct RootedDomTask {
    document: ScriptDocument,
    expected_version: DocumentVersion,
    phase_before: DocumentVersion,
    generation: u64,
    roots: Vec<RootedDomNode>,
    limits: ScriptMutationLimits,
    host_calls: usize,
    created_nodes: usize,
    total_string_bytes: usize,
    phase_commands: u32,
    phase_created_nodes: u32,
    retired: bool,
}

impl RootedDomTask {
    pub fn expected_version(&self) -> DocumentVersion {
        self.expected_version
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn rooted_node_count(&self) -> usize {
        self.roots.len()
    }

    fn ensure_active(&mut self) -> Result<(), BrowserHostError> {
        if self.retired {
            return Err(BrowserHostError::StaleTask);
        }
        let actual = self.document.current_version()?;
        if actual.document_id() != self.expected_version.document_id() {
            self.retired = true;
            return Err(BrowserHostError::StaleDocument);
        }
        if actual != self.expected_version {
            self.retired = true;
            return Err(BrowserHostError::VersionMismatch);
        }
        Ok(())
    }

    fn account_host_call(&mut self) -> Result<(), BrowserHostError> {
        self.ensure_active()?;
        let next = self
            .host_calls
            .checked_add(1)
            .ok_or(BrowserHostError::LimitExceeded)?;
        if next > self.limits.max_commands() {
            self.retired = true;
            return Err(BrowserHostError::LimitExceeded);
        }
        self.host_calls = next;
        Ok(())
    }

    fn account_creation(&mut self) -> Result<(), BrowserHostError> {
        let next = self
            .created_nodes
            .checked_add(1)
            .ok_or(BrowserHostError::LimitExceeded)?;
        if next > self.limits.max_created_nodes() {
            self.retired = true;
            return Err(BrowserHostError::LimitExceeded);
        }
        self.created_nodes = next;
        Ok(())
    }

    fn account_strings(&mut self, values: &[&str]) -> Result<(), BrowserHostError> {
        for value in values {
            if value.len() > self.limits.max_string_bytes() {
                self.retired = true;
                return Err(BrowserHostError::LimitExceeded);
            }
            let next = self
                .total_string_bytes
                .checked_add(value.len())
                .ok_or(BrowserHostError::LimitExceeded)?;
            if next > self.limits.max_total_string_bytes() {
                self.retired = true;
                return Err(BrowserHostError::LimitExceeded);
            }
            self.total_string_bytes = next;
        }
        Ok(())
    }

    fn owned_string(value: &str) -> Result<String, BrowserHostError> {
        let mut owned = String::new();
        owned
            .try_reserve_exact(value.len())
            .map_err(|_| BrowserHostError::Allocation)?;
        owned.push_str(value);
        Ok(owned)
    }

    fn reserve_root(&mut self) -> Result<(), BrowserHostError> {
        self.roots
            .try_reserve(1)
            .map_err(|_| BrowserHostError::Allocation)
    }

    fn token_for_root(
        &mut self,
        root: RootedDomNode,
    ) -> Result<BrowserHostNodeToken, BrowserHostError> {
        if let Some(index) = self.roots.iter().position(|candidate| {
            candidate.document_id == root.document_id && candidate.node_id == root.node_id
        }) {
            return self.encode_root_index(index);
        }
        self.reserve_root()?;
        let index = self.roots.len();
        self.roots.push(root);
        self.encode_root_index(index)
    }

    fn encode_root_index(&self, index: usize) -> Result<BrowserHostNodeToken, BrowserHostError> {
        let index = u64::try_from(index).map_err(|_| BrowserHostError::LimitExceeded)?;
        // Zero is reserved, so the largest encodable zero-based index is MASK - 1. Reject before
        // OR-ing: otherwise a future root budget above thirteen bits could spill into generation.
        if index >= TOKEN_SLOT_MASK {
            return Err(BrowserHostError::LimitExceeded);
        }
        if self.generation == 0 || self.generation > MAX_TASK_GENERATION {
            return Err(BrowserHostError::Internal);
        }
        let low = index + 1;
        let value = (self.generation << TOKEN_SLOT_BITS) | low;
        BrowserHostNodeToken::new(value).ok_or(BrowserHostError::Internal)
    }

    fn resolve_token(&mut self, token: BrowserHostNodeToken) -> Result<NodeId, BrowserHostError> {
        self.ensure_active()?;
        let generation = token.get() >> TOKEN_SLOT_BITS;
        if generation != self.generation {
            self.retired = true;
            return Err(BrowserHostError::StaleTask);
        }
        let low = (token.get() & TOKEN_SLOT_MASK) as u32;
        let index = low.checked_sub(1).ok_or(BrowserHostError::InvalidNode)? as usize;
        let root = self.roots.get(index).ok_or(BrowserHostError::InvalidNode)?;
        if !self.document.is_live(root) {
            self.retired = true;
            return Err(BrowserHostError::StaleDocument);
        }
        Ok(root.node_id)
    }

    fn apply_one(
        &mut self,
        command: ScriptMutationCommand,
    ) -> Result<wild_buzzard_dom::bindings::ScriptMutationCommit, BrowserHostError> {
        self.ensure_active()?;
        let mut commands = Vec::new();
        commands
            .try_reserve_exact(1)
            .map_err(|_| BrowserHostError::Allocation)?;
        commands.push(command);

        let mut state = self
            .document
            .inner
            .lock()
            .map_err(|_| BrowserHostError::Internal)?;
        if !state.current {
            self.retired = true;
            return Err(BrowserHostError::StaleDocument);
        }
        if state.document.version() != self.expected_version {
            self.retired = true;
            return Err(BrowserHostError::VersionMismatch);
        }
        let result = state.document.apply_script_mutations(
            ScriptMutationBatch::new(self.expected_version, commands),
            self.limits,
        );
        let commit = match result {
            Ok(commit) => commit,
            Err(error) => return Err(map_mutation_error(error)),
        };
        self.expected_version = commit.version();
        self.phase_commands = self
            .phase_commands
            .checked_add(1)
            .ok_or(BrowserHostError::LimitExceeded)?;
        Ok(commit)
    }

    fn mark_error(&mut self, error: BrowserHostError) -> BrowserHostError {
        if !matches!(
            error,
            BrowserHostError::InvalidArgument
                | BrowserHostError::InvalidNode
                | BrowserHostError::InvalidOperation
        ) {
            self.retired = true;
        }
        error
    }
}

impl BrowserHostTask for RootedDomTask {
    fn validate_phase(&mut self) -> Result<(), BrowserHostError> {
        self.ensure_active().map_err(|error| self.mark_error(error))
    }

    fn document_node(&mut self) -> Result<BrowserHostNodeToken, BrowserHostError> {
        self.account_host_call()
            .map_err(|error| self.mark_error(error))?;
        let node = {
            let state = self
                .document
                .inner
                .lock()
                .map_err(|_| BrowserHostError::Internal)?;
            state.document.document_node()
        };
        let root = self
            .document
            .root_node(node)
            .map_err(|error| self.mark_error(error))?;
        self.token_for_root(root)
            .map_err(|error| self.mark_error(error))
    }

    fn lookup_node(&mut self, slot: u32) -> Result<BrowserHostNodeToken, BrowserHostError> {
        self.account_host_call()
            .map_err(|error| self.mark_error(error))?;
        let lookup = {
            let state = self
                .document
                .inner
                .lock()
                .map_err(|_| BrowserHostError::Internal)?;
            if !state.current {
                Err(BrowserHostError::StaleDocument)
            } else if state.document.version() != self.expected_version {
                Err(BrowserHostError::VersionMismatch)
            } else {
                state
                    .document
                    .lookup_script_node(slot)
                    .map_err(|_| BrowserHostError::InvalidNode)
            }
        };
        let node = lookup.map_err(|error| self.mark_error(error))?;
        let root = self
            .document
            .root_node(node)
            .map_err(|error| self.mark_error(error))?;
        self.token_for_root(root)
            .map_err(|error| self.mark_error(error))
    }

    fn create_html_element(
        &mut self,
        local_name: &str,
    ) -> Result<BrowserHostNodeToken, BrowserHostError> {
        self.account_host_call()
            .map_err(|error| self.mark_error(error))?;
        self.account_creation()
            .map_err(|error| self.mark_error(error))?;
        self.account_strings(&[local_name])
            .map_err(|error| self.mark_error(error))?;
        if local_name.is_empty() {
            return Err(BrowserHostError::InvalidArgument);
        }
        self.reserve_root()
            .map_err(|error| self.mark_error(error))?;
        let local_name = Self::owned_string(local_name).map_err(|error| self.mark_error(error))?;
        let commit = self
            .apply_one(ScriptMutationCommand::CreateHtmlElement {
                token: CreatedNodeToken::from_index(0),
                local_name,
            })
            .map_err(|error| self.mark_error(error))?;
        let node = commit
            .created_node(CreatedNodeToken::from_index(0))
            .ok_or(BrowserHostError::Internal)?;
        let root = self
            .document
            .root_node(node)
            .map_err(|error| self.mark_error(error))?;
        let index = self.roots.len();
        self.roots.push(root);
        self.phase_created_nodes = self
            .phase_created_nodes
            .checked_add(1)
            .ok_or(BrowserHostError::LimitExceeded)?;
        self.encode_root_index(index)
    }

    fn create_text(&mut self, data: &str) -> Result<BrowserHostNodeToken, BrowserHostError> {
        self.account_host_call()
            .map_err(|error| self.mark_error(error))?;
        self.account_creation()
            .map_err(|error| self.mark_error(error))?;
        self.account_strings(&[data])
            .map_err(|error| self.mark_error(error))?;
        self.reserve_root()
            .map_err(|error| self.mark_error(error))?;
        let data = Self::owned_string(data).map_err(|error| self.mark_error(error))?;
        let commit = self
            .apply_one(ScriptMutationCommand::CreateText {
                token: CreatedNodeToken::from_index(0),
                data,
            })
            .map_err(|error| self.mark_error(error))?;
        let node = commit
            .created_node(CreatedNodeToken::from_index(0))
            .ok_or(BrowserHostError::Internal)?;
        let root = self
            .document
            .root_node(node)
            .map_err(|error| self.mark_error(error))?;
        let index = self.roots.len();
        self.roots.push(root);
        self.phase_created_nodes = self
            .phase_created_nodes
            .checked_add(1)
            .ok_or(BrowserHostError::LimitExceeded)?;
        self.encode_root_index(index)
    }

    fn append_child(
        &mut self,
        parent: BrowserHostNodeToken,
        child: BrowserHostNodeToken,
    ) -> Result<(), BrowserHostError> {
        self.account_host_call()
            .map_err(|error| self.mark_error(error))?;
        let parent = self
            .resolve_token(parent)
            .map_err(|error| self.mark_error(error))?;
        let child = self
            .resolve_token(child)
            .map_err(|error| self.mark_error(error))?;
        self.apply_one(ScriptMutationCommand::AppendChild {
            parent: ScriptNode::Existing(parent),
            child: ScriptNode::Existing(child),
        })
        .map(|_| ())
        .map_err(|error| self.mark_error(error))
    }

    fn set_html_attribute(
        &mut self,
        element: BrowserHostNodeToken,
        local_name: &str,
        value: &str,
    ) -> Result<(), BrowserHostError> {
        self.account_host_call()
            .map_err(|error| self.mark_error(error))?;
        self.account_strings(&[local_name, value])
            .map_err(|error| self.mark_error(error))?;
        let element = self
            .resolve_token(element)
            .map_err(|error| self.mark_error(error))?;
        let local_name = Self::owned_string(local_name).map_err(|error| self.mark_error(error))?;
        let value = Self::owned_string(value).map_err(|error| self.mark_error(error))?;
        self.apply_one(ScriptMutationCommand::SetHtmlAttribute {
            element: ScriptNode::Existing(element),
            local_name,
            value,
        })
        .map(|_| ())
        .map_err(|error| self.mark_error(error))
    }

    fn set_character_data(
        &mut self,
        node: BrowserHostNodeToken,
        data: &str,
    ) -> Result<(), BrowserHostError> {
        self.account_host_call()
            .map_err(|error| self.mark_error(error))?;
        self.account_strings(&[data])
            .map_err(|error| self.mark_error(error))?;
        let node = self
            .resolve_token(node)
            .map_err(|error| self.mark_error(error))?;
        let data = Self::owned_string(data).map_err(|error| self.mark_error(error))?;
        self.apply_one(ScriptMutationCommand::SetCharacterData {
            node: ScriptNode::Existing(node),
            data,
        })
        .map(|_| ())
        .map_err(|error| self.mark_error(error))
    }

    fn finish_phase(&mut self) -> Result<BrowserHostCommitOutcome, BrowserHostError> {
        self.ensure_active()
            .map_err(|error| self.mark_error(error))?;
        let before = host_version(self.phase_before);
        let after = host_version(self.expected_version);
        let outcome = if self.phase_commands == 0 {
            BrowserHostCommitOutcome::NoChanges(after)
        } else {
            BrowserHostCommitOutcome::Committed(BrowserHostPhaseCommit::new(
                before,
                after,
                self.phase_commands,
                self.phase_created_nodes,
            ))
        };
        self.phase_before = self.expected_version;
        self.phase_commands = 0;
        self.phase_created_nodes = 0;
        Ok(outcome)
    }

    fn abort_phase(&mut self) {
        self.retired = true;
        self.phase_commands = 0;
        self.phase_created_nodes = 0;
    }
}

fn host_version(version: DocumentVersion) -> BrowserHostDocumentVersion {
    BrowserHostDocumentVersion::new(version.document_id().get(), version.revision())
}

fn map_dom_read_error(error: DomError) -> BrowserHostError {
    match error {
        DomError::WrongDocument { .. } | DomError::UnknownNode(_) => BrowserHostError::InvalidNode,
        _ => BrowserHostError::InvalidOperation,
    }
}

fn map_mutation_error(error: ScriptMutationError) -> BrowserHostError {
    match error {
        ScriptMutationError::VersionMismatch { .. } => BrowserHostError::VersionMismatch,
        ScriptMutationError::LimitExceeded { .. } => BrowserHostError::LimitExceeded,
        ScriptMutationError::Command {
            error: DomError::InvalidName,
            ..
        } => BrowserHostError::InvalidArgument,
        ScriptMutationError::Command { .. } | ScriptMutationError::Token { .. } => {
            BrowserHostError::InvalidOperation
        }
        ScriptMutationError::EmptyBatch
        | ScriptMutationError::RevisionExhausted { .. }
        | ScriptMutationError::Finalization(_) => BrowserHostError::Internal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_token_slot_boundary_cannot_spill_into_generation_bits() {
        let mut task = ScriptDocument::new(Document::new())
            .begin_task(ScriptMutationLimits::DEFAULT)
            .unwrap();
        task.generation = MAX_TASK_GENERATION;

        let last = task
            .encode_root_index((TOKEN_SLOT_MASK - 1) as usize)
            .unwrap();
        assert_eq!(last.get() & TOKEN_SLOT_MASK, TOKEN_SLOT_MASK);
        assert_eq!(last.get() >> TOKEN_SLOT_BITS, MAX_TASK_GENERATION);
        assert_eq!(
            task.encode_root_index(TOKEN_SLOT_MASK as usize),
            Err(BrowserHostError::LimitExceeded)
        );

        task.generation = MAX_TASK_GENERATION + 1;
        assert_eq!(task.encode_root_index(0), Err(BrowserHostError::Internal));
    }
}
