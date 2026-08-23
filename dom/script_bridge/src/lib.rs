//! Concrete rooted adapter between Brimstone's bounded browser task capability and the first Rust
//! DOM nucleus.
//!
//! Each host operation is published synchronously through a one-command, exact-version
//! `ScriptMutationBatch`. This preserves the browser-visible successful prefix when a later DOM
//! call or JavaScript instruction throws. The task retains `Arc`-owned DOM roots across the
//! explicit microtask checkpoint, while JavaScript sees only exact numeric task tokens.

use std::{
    fmt, mem,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use brimstone_core::runtime::{
    BrowserHostClassicExecution, BrowserHostCommitOutcome, BrowserHostDocumentVersion,
    BrowserHostError, BrowserHostMicrotaskExecution, BrowserHostNodeToken, BrowserHostPhaseCommit,
    BrowserHostPhaseOutcome, BrowserHostTask, BrowserScriptRealm, ClassicScriptOutcome,
    ClassicScriptRequest, MicrotaskCheckpointOutcome,
};
use wild_buzzard_dom::bindings::{
    CreatedNodeToken, DomRootProvider, DomRootTrace, RootedNodeHandle, ScriptMutationBatch,
    ScriptMutationCommand, ScriptMutationError, ScriptMutationLimits, ScriptNode,
};
use wild_buzzard_dom::{Document, DocumentId, DocumentSnapshot, DocumentVersion, DomError, NodeId};
use wild_buzzard_html::{ParseOutput, ParserInsertedScript};

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
    residency: DocumentResidency,
    publication: DocumentPublication,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DocumentResidency {
    Host,
    Parser { generation: u64, sequence: u64 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DocumentPublication {
    Unmanaged,
    ParserLent { generation: u64, sequence: u64 },
    BoundaryRestored { generation: u64, sequence: u64 },
    PreCheckpointActive { generation: u64, sequence: u64 },
    Prepared { generation: u64, sequence: u64 },
    ClassicActive { generation: u64, sequence: u64 },
    Executed { generation: u64, sequence: u64 },
    PostCheckpointActive { generation: u64, sequence: u64 },
    ReadyToLend { generation: u64, sequence: u64 },
    CompletionRestored { generation: u64, sequence: u64 },
    FinalCheckpointActive { generation: u64, sequence: u64 },
    Published { generation: u64, sequence: u64 },
    Retired,
}

impl DocumentPublication {
    const fn publicly_readable(self) -> bool {
        matches!(self, Self::Unmanaged | Self::Published { .. })
    }

    const fn host_accessible(self) -> bool {
        matches!(
            self,
            Self::Unmanaged
                | Self::PreCheckpointActive { .. }
                | Self::ClassicActive { .. }
                | Self::PostCheckpointActive { .. }
                | Self::FinalCheckpointActive { .. }
                | Self::Published { .. }
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ParserLeaseMode {
    Inactive,
    Attached {
        sequence: u64,
    },
    Lent {
        sequence: u64,
        before: DocumentVersion,
    },
    Restored {
        sequence: u64,
        before: DocumentVersion,
        after: DocumentVersion,
        complete: bool,
    },
    Complete {
        sequence: u64,
    },
    Retired,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ActiveHostMode {
    General,
    ParserBoundary { sequence: u64 },
    ParserCompletion { sequence: u64 },
}

#[derive(Debug)]
struct ParserLeaseAuthority {
    generation: u64,
    mode: ParserLeaseMode,
    quiescent_version: DocumentVersion,
    phase_open: bool,
    rooted_nodes: Vec<NodeId>,
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
                residency: DocumentResidency::Host,
                publication: DocumentPublication::Unmanaged,
            })),
        }
    }

    pub fn current_version(&self) -> Result<DocumentVersion, BrowserHostError> {
        let state = self.inner.lock().map_err(|_| BrowserHostError::Internal)?;
        if !state.current {
            return Err(BrowserHostError::StaleDocument);
        }
        if state.residency != DocumentResidency::Host || !state.publication.host_accessible() {
            return Err(BrowserHostError::StaleTask);
        }
        if !state.publication.publicly_readable() {
            return Err(BrowserHostError::StaleTask);
        }
        Ok(state.document.version())
    }

    pub fn snapshot(&self) -> Result<DocumentSnapshot, BrowserHostError> {
        let state = self.inner.lock().map_err(|_| BrowserHostError::Internal)?;
        if !state.current {
            return Err(BrowserHostError::StaleDocument);
        }
        if state.residency != DocumentResidency::Host || !state.publication.host_accessible() {
            return Err(BrowserHostError::StaleTask);
        }
        if !state.publication.publicly_readable() {
            return Err(BrowserHostError::StaleTask);
        }
        state.document.snapshot().map_err(map_dom_read_error)
    }

    fn transition_parser_publication(
        &self,
        expected: DocumentPublication,
        next: DocumentPublication,
    ) -> Result<(), BrowserHostError> {
        let mut state = self.inner.lock().map_err(|_| BrowserHostError::Internal)?;
        if !state.current {
            return Err(BrowserHostError::StaleDocument);
        }
        if state.residency != DocumentResidency::Host || state.publication != expected {
            return Err(BrowserHostError::StaleTask);
        }
        state.publication = next;
        Ok(())
    }

    fn snapshot_at_parser_publication(
        &self,
        expected: DocumentPublication,
    ) -> Result<DocumentSnapshot, BrowserHostError> {
        let state = self.inner.lock().map_err(|_| BrowserHostError::Internal)?;
        if !state.current {
            return Err(BrowserHostError::StaleDocument);
        }
        if state.residency != DocumentResidency::Host || state.publication != expected {
            return Err(BrowserHostError::StaleTask);
        }
        state.document.snapshot().map_err(map_dom_read_error)
    }

    fn version_at_parser_publication(
        &self,
        expected: DocumentPublication,
    ) -> Result<DocumentVersion, BrowserHostError> {
        let state = self.inner.lock().map_err(|_| BrowserHostError::Internal)?;
        if !state.current {
            return Err(BrowserHostError::StaleDocument);
        }
        if state.residency != DocumentResidency::Host || state.publication != expected {
            return Err(BrowserHostError::StaleTask);
        }
        Ok(state.document.version())
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
        if state.residency != DocumentResidency::Host {
            return Err(ScriptMutationError::Finalization(
                DomError::SnapshotInvariant("script document is leased to its parser"),
            ));
        }
        if !state.publication.publicly_readable() {
            return Err(ScriptMutationError::Finalization(
                DomError::SnapshotInvariant("script document is not published"),
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
        state.publication = DocumentPublication::Retired;
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
            parser_authority: Arc::new(Mutex::new(ParserLeaseAuthority {
                generation,
                mode: ParserLeaseMode::Inactive,
                quiescent_version: expected_version,
                phase_open: false,
                rooted_nodes: Vec::new(),
            })),
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

    /// Restores one exact parser boundary to the installed rooted host.
    ///
    /// The returned guard keeps a placeholder in the parser while the real
    /// document is attached to this host. A normal caller must advance through
    /// [`RestoredParserDocument::perform_pre_checkpoint`] and return the
    /// resulting completed boundary with
    /// [`CompletedParserDocument::lend_back_to_parser`]. Dropping any guard
    /// restores the real document to the parser and permanently retires the
    /// host task.
    pub fn restore_parser_boundary<'parser>(
        &self,
        parser_document: &'parser mut Document,
        lease: ParserDocumentLease,
        boundary: ParserInsertedScript,
    ) -> Result<RestoredParserDocument<'parser>, BrowserHostError> {
        if !Arc::ptr_eq(&self.inner, &lease.document) {
            retire_parser_authority(&lease.authority);
            return Err(BrowserHostError::StaleDocument);
        }
        let after = parser_document.version();
        if after != boundary.document_version()
            || after.document_id() != lease.before.document_id()
            || after.revision() < lease.before.revision()
            || boundary.node().document_id() != after.document_id()
            || boundary.ordinal() != lease.sequence
        {
            retire_parser_authority(&lease.authority);
            return Err(BrowserHostError::VersionMismatch);
        }

        let mut authority = lease
            .authority
            .lock()
            .map_err(|_| BrowserHostError::Internal)?;
        if authority.generation != lease.generation
            || authority.mode
                != (ParserLeaseMode::Lent {
                    sequence: lease.sequence,
                    before: lease.before,
                })
            || authority.phase_open
            || authority.quiescent_version != lease.before
        {
            authority.mode = ParserLeaseMode::Retired;
            return Err(BrowserHostError::StaleTask);
        }
        validate_parser_roots(parser_document, &authority.rooted_nodes).inspect_err(|_| {
            authority.mode = ParserLeaseMode::Retired;
        })?;

        let mut state = self.inner.lock().map_err(|_| {
            authority.mode = ParserLeaseMode::Retired;
            BrowserHostError::Internal
        })?;
        if !state.current
            || state.residency
                != (DocumentResidency::Parser {
                    generation: lease.generation,
                    sequence: lease.sequence,
                })
            || state.publication
                != (DocumentPublication::ParserLent {
                    generation: lease.generation,
                    sequence: lease.sequence,
                })
        {
            authority.mode = ParserLeaseMode::Retired;
            return Err(BrowserHostError::StaleDocument);
        }

        mem::swap(&mut state.document, parser_document);
        state.residency = DocumentResidency::Host;
        state.publication = DocumentPublication::BoundaryRestored {
            generation: lease.generation,
            sequence: lease.sequence,
        };
        authority.mode = ParserLeaseMode::Restored {
            sequence: lease.sequence,
            before: lease.before,
            after,
            complete: false,
        };
        drop(state);
        drop(authority);

        Ok(RestoredParserDocument {
            parser_document,
            document: self.inner.clone(),
            authority: lease.authority,
            generation: lease.generation,
            sequence: lease.sequence,
            active: true,
        })
    }

    /// Restores the parser's final exact document to the host owner.
    ///
    /// One final host validation or checkpoint consumes the sealed parser
    /// advance before the hosted document session closes.
    pub fn restore_parser_completion(
        &self,
        lease: ParserDocumentLease,
        parsed: ParseOutput,
    ) -> Result<RestoredParserCompletion, BrowserHostError> {
        if !Arc::ptr_eq(&self.inner, &lease.document) {
            retire_parser_authority(&lease.authority);
            return Err(BrowserHostError::StaleDocument);
        }
        let after = parsed.document.version();
        if parsed.completion_document_version() != after
            || parsed.completed_script_boundaries().checked_add(1) != Some(lease.sequence)
            || after.document_id() != lease.before.document_id()
            || after.revision() < lease.before.revision()
        {
            retire_parser_authority(&lease.authority);
            return Err(BrowserHostError::VersionMismatch);
        }
        let document = parsed.document;

        let mut authority = lease
            .authority
            .lock()
            .map_err(|_| BrowserHostError::Internal)?;
        if authority.generation != lease.generation
            || authority.mode
                != (ParserLeaseMode::Lent {
                    sequence: lease.sequence,
                    before: lease.before,
                })
            || authority.phase_open
            || authority.quiescent_version != lease.before
        {
            authority.mode = ParserLeaseMode::Retired;
            return Err(BrowserHostError::StaleTask);
        }
        validate_parser_roots(&document, &authority.rooted_nodes).inspect_err(|_| {
            authority.mode = ParserLeaseMode::Retired;
        })?;

        let mut state = self.inner.lock().map_err(|_| {
            authority.mode = ParserLeaseMode::Retired;
            BrowserHostError::Internal
        })?;
        if !state.current
            || state.residency
                != (DocumentResidency::Parser {
                    generation: lease.generation,
                    sequence: lease.sequence,
                })
            || state.publication
                != (DocumentPublication::ParserLent {
                    generation: lease.generation,
                    sequence: lease.sequence,
                })
        {
            authority.mode = ParserLeaseMode::Retired;
            return Err(BrowserHostError::StaleDocument);
        }
        state.document = document;
        state.residency = DocumentResidency::Host;
        state.publication = DocumentPublication::CompletionRestored {
            generation: lease.generation,
            sequence: lease.sequence,
        };
        authority.mode = ParserLeaseMode::Restored {
            sequence: lease.sequence,
            before: lease.before,
            after,
            complete: true,
        };
        drop(state);
        drop(authority);
        Ok(RestoredParserCompletion {
            document: self.clone(),
            authority: lease.authority,
            generation: lease.generation,
            sequence: lease.sequence,
            after,
            active: true,
        })
    }
}

/// Exact document and sealed authority initially lent from one rooted host to
/// an HTML parser.
pub struct LentParserDocument {
    document: Document,
    lease: ParserDocumentLease,
}

impl LentParserDocument {
    /// Separates parser ownership from the nonforgeable return authority.
    #[must_use]
    pub fn into_parts(self) -> (Document, ParserDocumentLease) {
        (self.document, self.lease)
    }
}

/// Nonforgeable, one-use authority to restore one exact parser advance.
pub struct ParserDocumentLease {
    document: Arc<Mutex<LiveDocument>>,
    authority: Arc<Mutex<ParserLeaseAuthority>>,
    generation: u64,
    sequence: u64,
    before: DocumentVersion,
}

impl std::fmt::Debug for ParserDocumentLease {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ParserDocumentLease")
            .field("generation", &self.generation)
            .field("sequence", &self.sequence)
            .field("before", &self.before)
            .finish_non_exhaustive()
    }
}

/// Scoped attachment of the real parser document to one rooted host.
pub struct RestoredParserDocument<'parser> {
    parser_document: &'parser mut Document,
    document: Arc<Mutex<LiveDocument>>,
    authority: Arc<Mutex<ParserLeaseAuthority>>,
    generation: u64,
    sequence: u64,
    active: bool,
}

/// A sealed parser-boundary phase failed before the document could be returned
/// to parsing or published.
#[derive(Debug)]
pub enum ParserPhaseError {
    Host(BrowserHostError),
    Checkpoint(Box<BrowserHostMicrotaskExecution>),
    Classic(Box<BrowserHostClassicExecution>),
    Skipped(ClassicScriptOutcome),
}

impl fmt::Display for ParserPhaseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Host(error) => write!(formatter, "parser host phase failed: {error:?}"),
            Self::Checkpoint(execution) => write!(
                formatter,
                "parser checkpoint did not complete: {:?}; host: {:?}",
                execution.checkpoint.outcome, execution.host
            ),
            Self::Classic(execution) => write!(
                formatter,
                "parser classic script did not complete: {:?}; host: {:?}",
                execution.script.outcome, execution.host
            ),
            Self::Skipped(outcome) => {
                write!(
                    formatter,
                    "skipped parser script could not be accounted: {outcome:?}"
                )
            }
        }
    }
}

impl std::error::Error for ParserPhaseError {}

impl<'parser> RestoredParserDocument<'parser> {
    fn capability(&self) -> ScriptDocument {
        ScriptDocument {
            inner: self.document.clone(),
        }
    }

    /// Performs the mandatory pre-script microtask checkpoint. No public
    /// snapshot can observe this document until this operation completes.
    pub fn perform_pre_checkpoint(
        self,
        realm: &mut BrowserScriptRealm<'_>,
    ) -> Result<PreparedParserDocument<'parser>, ParserPhaseError> {
        let capability = self.capability();
        capability
            .transition_parser_publication(
                DocumentPublication::BoundaryRestored {
                    generation: self.generation,
                    sequence: self.sequence,
                },
                DocumentPublication::PreCheckpointActive {
                    generation: self.generation,
                    sequence: self.sequence,
                },
            )
            .map_err(ParserPhaseError::Host)?;
        let execution = realm.perform_hosted_document_microtask_checkpoint();
        if !checkpoint_completed(&execution) {
            return Err(ParserPhaseError::Checkpoint(Box::new(execution)));
        }
        capability
            .transition_parser_publication(
                DocumentPublication::PreCheckpointActive {
                    generation: self.generation,
                    sequence: self.sequence,
                },
                DocumentPublication::Prepared {
                    generation: self.generation,
                    sequence: self.sequence,
                },
            )
            .map_err(ParserPhaseError::Host)?;
        Ok(PreparedParserDocument {
            restored: self,
            pre_checkpoint: execution,
        })
    }

    fn lend_back_to_parser_after_completion(
        mut self,
    ) -> Result<ParserDocumentLease, BrowserHostError> {
        let mut authority = self
            .authority
            .lock()
            .map_err(|_| BrowserHostError::Internal)?;
        if authority.generation != self.generation
            || authority.mode
                != (ParserLeaseMode::Attached {
                    sequence: self.sequence,
                })
            || authority.phase_open
        {
            return Err(BrowserHostError::StaleTask);
        }
        let mut state = self
            .document
            .lock()
            .map_err(|_| BrowserHostError::Internal)?;
        if !state.current
            || state.residency != DocumentResidency::Host
            || state.publication
                != (DocumentPublication::ReadyToLend {
                    generation: self.generation,
                    sequence: self.sequence,
                })
        {
            return Err(BrowserHostError::StaleDocument);
        }
        let before = state.document.version();
        if authority.quiescent_version != before {
            return Err(BrowserHostError::VersionMismatch);
        }
        validate_parser_roots(&state.document, &authority.rooted_nodes)?;
        let sequence = self
            .sequence
            .checked_add(1)
            .ok_or(BrowserHostError::Internal)?;
        mem::swap(&mut state.document, self.parser_document);
        state.residency = DocumentResidency::Parser {
            generation: self.generation,
            sequence,
        };
        state.publication = DocumentPublication::ParserLent {
            generation: self.generation,
            sequence,
        };
        authority.mode = ParserLeaseMode::Lent { sequence, before };
        self.active = false;
        Ok(ParserDocumentLease {
            document: self.document.clone(),
            authority: self.authority.clone(),
            generation: self.generation,
            sequence,
            before,
        })
    }
}

/// Parser boundary after its mandatory pre-script checkpoint. This is the only
/// state which can expose the live source snapshot used for preparation.
pub struct PreparedParserDocument<'parser> {
    restored: RestoredParserDocument<'parser>,
    pre_checkpoint: BrowserHostMicrotaskExecution,
}

impl<'parser> PreparedParserDocument<'parser> {
    #[must_use]
    pub const fn pre_checkpoint(&self) -> &BrowserHostMicrotaskExecution {
        &self.pre_checkpoint
    }

    pub fn snapshot(&self) -> Result<DocumentSnapshot, BrowserHostError> {
        self.restored
            .capability()
            .snapshot_at_parser_publication(DocumentPublication::Prepared {
                generation: self.restored.generation,
                sequence: self.restored.sequence,
            })
    }

    pub fn execute_classic(
        self,
        realm: &mut BrowserScriptRealm<'_>,
        request: ClassicScriptRequest<'_>,
    ) -> Result<ExecutedParserDocument<'parser>, ParserPhaseError> {
        let capability = self.restored.capability();
        capability
            .transition_parser_publication(
                DocumentPublication::Prepared {
                    generation: self.restored.generation,
                    sequence: self.restored.sequence,
                },
                DocumentPublication::ClassicActive {
                    generation: self.restored.generation,
                    sequence: self.restored.sequence,
                },
            )
            .map_err(ParserPhaseError::Host)?;
        let execution = realm.execute_hosted_document_classic(request);
        if !classic_completed(&execution) {
            return Err(ParserPhaseError::Classic(Box::new(execution)));
        }
        capability
            .transition_parser_publication(
                DocumentPublication::ClassicActive {
                    generation: self.restored.generation,
                    sequence: self.restored.sequence,
                },
                DocumentPublication::Executed {
                    generation: self.restored.generation,
                    sequence: self.restored.sequence,
                },
            )
            .map_err(ParserPhaseError::Host)?;
        Ok(ExecutedParserDocument {
            prepared: self,
            execution,
        })
    }

    pub fn skip(
        self,
        realm: &mut BrowserScriptRealm<'_>,
        source_bytes: usize,
    ) -> Result<CompletedParserDocument<'parser>, ParserPhaseError> {
        realm
            .account_skipped_document_script(source_bytes)
            .map_err(ParserPhaseError::Skipped)?;
        self.restored
            .capability()
            .transition_parser_publication(
                DocumentPublication::Prepared {
                    generation: self.restored.generation,
                    sequence: self.restored.sequence,
                },
                DocumentPublication::ReadyToLend {
                    generation: self.restored.generation,
                    sequence: self.restored.sequence,
                },
            )
            .map_err(ParserPhaseError::Host)?;
        Ok(CompletedParserDocument {
            restored: self.restored,
            post_checkpoint: None,
        })
    }
}

/// Parser boundary after one admitted classic-script phase. Dropping this
/// value cannot resume parsing; a successful post-script checkpoint is still
/// mandatory.
pub struct ExecutedParserDocument<'parser> {
    prepared: PreparedParserDocument<'parser>,
    execution: BrowserHostClassicExecution,
}

impl<'parser> ExecutedParserDocument<'parser> {
    #[must_use]
    pub const fn execution(&self) -> &BrowserHostClassicExecution {
        &self.execution
    }

    pub fn perform_post_checkpoint(
        self,
        realm: &mut BrowserScriptRealm<'_>,
    ) -> Result<CompletedParserDocument<'parser>, ParserPhaseError> {
        let capability = self.prepared.restored.capability();
        capability
            .transition_parser_publication(
                DocumentPublication::Executed {
                    generation: self.prepared.restored.generation,
                    sequence: self.prepared.restored.sequence,
                },
                DocumentPublication::PostCheckpointActive {
                    generation: self.prepared.restored.generation,
                    sequence: self.prepared.restored.sequence,
                },
            )
            .map_err(ParserPhaseError::Host)?;
        let checkpoint = realm.perform_hosted_document_microtask_checkpoint();
        if !checkpoint_completed(&checkpoint) {
            return Err(ParserPhaseError::Checkpoint(Box::new(checkpoint)));
        }
        capability
            .transition_parser_publication(
                DocumentPublication::PostCheckpointActive {
                    generation: self.prepared.restored.generation,
                    sequence: self.prepared.restored.sequence,
                },
                DocumentPublication::ReadyToLend {
                    generation: self.prepared.restored.generation,
                    sequence: self.prepared.restored.sequence,
                },
            )
            .map_err(ParserPhaseError::Host)?;
        Ok(CompletedParserDocument {
            restored: self.prepared.restored,
            post_checkpoint: Some(checkpoint),
        })
    }
}

/// Exact parser boundary whose required script/checkpoint work has completed.
pub struct CompletedParserDocument<'parser> {
    restored: RestoredParserDocument<'parser>,
    post_checkpoint: Option<BrowserHostMicrotaskExecution>,
}

impl CompletedParserDocument<'_> {
    #[must_use]
    pub const fn post_checkpoint(&self) -> Option<&BrowserHostMicrotaskExecution> {
        self.post_checkpoint.as_ref()
    }

    pub fn current_version(&self) -> Result<DocumentVersion, BrowserHostError> {
        self.restored
            .capability()
            .version_at_parser_publication(DocumentPublication::ReadyToLend {
                generation: self.restored.generation,
                sequence: self.restored.sequence,
            })
    }

    pub fn lend_back_to_parser(self) -> Result<ParserDocumentLease, BrowserHostError> {
        self.restored.lend_back_to_parser_after_completion()
    }
}

impl Drop for RestoredParserDocument<'_> {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let Ok(mut authority) = self.authority.lock() else {
            std::process::abort();
        };
        let Ok(mut state) = self.document.lock() else {
            std::process::abort();
        };
        let matching_mode = matches!(
            authority.mode,
            ParserLeaseMode::Restored { sequence, .. }
                | ParserLeaseMode::Attached { sequence }
                if sequence == self.sequence
        ) || authority.mode == ParserLeaseMode::Retired;
        if authority.generation != self.generation
            || !matching_mode
            || state.residency != DocumentResidency::Host
        {
            std::process::abort();
        }
        mem::swap(&mut state.document, self.parser_document);
        state.residency = DocumentResidency::Parser {
            generation: self.generation,
            sequence: self.sequence,
        };
        state.current = false;
        state.publication = DocumentPublication::Retired;
        authority.mode = ParserLeaseMode::Retired;
        authority.phase_open = false;
        self.active = false;
    }
}

/// Final parser completion attached to the hosted document but not yet
/// published. Dropping this guard retires the document.
pub struct RestoredParserCompletion {
    document: ScriptDocument,
    authority: Arc<Mutex<ParserLeaseAuthority>>,
    generation: u64,
    sequence: u64,
    after: DocumentVersion,
    active: bool,
}

impl RestoredParserCompletion {
    /// Performs the mandatory final document checkpoint and returns the only
    /// authority which makes the finished document publicly snapshotable.
    pub fn perform_final_checkpoint(
        mut self,
        realm: &mut BrowserScriptRealm<'_>,
    ) -> Result<PublishedParserDocument, ParserPhaseError> {
        self.document
            .transition_parser_publication(
                DocumentPublication::CompletionRestored {
                    generation: self.generation,
                    sequence: self.sequence,
                },
                DocumentPublication::FinalCheckpointActive {
                    generation: self.generation,
                    sequence: self.sequence,
                },
            )
            .map_err(ParserPhaseError::Host)?;
        let checkpoint = realm.perform_hosted_document_microtask_checkpoint();
        if !checkpoint_completed(&checkpoint) {
            return Err(ParserPhaseError::Checkpoint(Box::new(checkpoint)));
        }
        let published_version = self
            .document
            .version_at_parser_publication(DocumentPublication::FinalCheckpointActive {
                generation: self.generation,
                sequence: self.sequence,
            })
            .map_err(ParserPhaseError::Host)?;
        {
            let authority = self
                .authority
                .lock()
                .map_err(|_| ParserPhaseError::Host(BrowserHostError::Internal))?;
            if authority.generation != self.generation
                || authority.mode
                    != (ParserLeaseMode::Complete {
                        sequence: self.sequence,
                    })
                || authority.phase_open
                || authority.quiescent_version != published_version
            {
                return Err(ParserPhaseError::Host(BrowserHostError::StaleTask));
            }
        }
        self.document
            .transition_parser_publication(
                DocumentPublication::FinalCheckpointActive {
                    generation: self.generation,
                    sequence: self.sequence,
                },
                DocumentPublication::Published {
                    generation: self.generation,
                    sequence: self.sequence,
                },
            )
            .map_err(ParserPhaseError::Host)?;
        self.active = false;
        Ok(PublishedParserDocument {
            document: self.document.clone(),
            parser_version: self.after,
            published_version,
            final_checkpoint: checkpoint,
        })
    }
}

impl Drop for RestoredParserCompletion {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let Ok(mut authority) = self.authority.lock() else {
            std::process::abort();
        };
        let Ok(mut state) = self.document.inner.lock() else {
            std::process::abort();
        };
        let matching_mode = matches!(
            authority.mode,
            ParserLeaseMode::Restored {
                sequence,
                complete: true,
                ..
            } | ParserLeaseMode::Complete { sequence }
                if sequence == self.sequence
        ) || authority.mode == ParserLeaseMode::Retired;
        if authority.generation != self.generation
            || !matching_mode
            || state.residency != DocumentResidency::Host
        {
            std::process::abort();
        }
        state.current = false;
        state.publication = DocumentPublication::Retired;
        authority.mode = ParserLeaseMode::Retired;
        authority.phase_open = false;
        self.active = false;
    }
}

/// Successfully published parser completion and its exact final checkpoint
/// evidence.
pub struct PublishedParserDocument {
    document: ScriptDocument,
    parser_version: DocumentVersion,
    published_version: DocumentVersion,
    final_checkpoint: BrowserHostMicrotaskExecution,
}

impl PublishedParserDocument {
    #[must_use]
    pub const fn parser_version(&self) -> DocumentVersion {
        self.parser_version
    }

    #[must_use]
    pub const fn published_version(&self) -> DocumentVersion {
        self.published_version
    }

    #[must_use]
    pub const fn final_checkpoint(&self) -> &BrowserHostMicrotaskExecution {
        &self.final_checkpoint
    }

    pub fn snapshot(&self) -> Result<DocumentSnapshot, BrowserHostError> {
        self.document.snapshot()
    }
}

fn checkpoint_completed(execution: &BrowserHostMicrotaskExecution) -> bool {
    execution.checkpoint.outcome == MicrotaskCheckpointOutcome::Complete
        && matches!(execution.host, BrowserHostPhaseOutcome::Completed(_))
}

fn classic_completed(execution: &BrowserHostClassicExecution) -> bool {
    matches!(execution.host, BrowserHostPhaseOutcome::Completed(_))
        && matches!(
            execution.script.outcome,
            ClassicScriptOutcome::Success(_)
                | ClassicScriptOutcome::Thrown(_)
                | ClassicScriptOutcome::ParseError(_)
                | ClassicScriptOutcome::AnalyzeError(_)
                | ClassicScriptOutcome::CompileError(_)
        )
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
        if state.residency != DocumentResidency::Host || !state.publication.host_accessible() {
            return Err(BrowserHostError::StaleTask);
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
                && state.residency == DocumentResidency::Host
                && state.publication.host_accessible()
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
    parser_authority: Arc<Mutex<ParserLeaseAuthority>>,
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

    /// Lends this task's exact pristine document to an HTML parser.
    ///
    /// This must occur before the host is installed in Brimstone and while no
    /// host phase is open. The returned authority can be restored only to this
    /// exact task and document.
    pub fn lend_document_to_parser(&mut self) -> Result<LentParserDocument, BrowserHostError> {
        self.ensure_active()?;
        if self.phase_commands != 0
            || self.phase_created_nodes != 0
            || self.phase_before != self.expected_version
        {
            self.retire();
            return Err(BrowserHostError::StaleTask);
        }
        let placeholder = Document::new();
        let mut authority = self
            .parser_authority
            .lock()
            .map_err(|_| BrowserHostError::Internal)?;
        if authority.generation != self.generation
            || authority.mode != ParserLeaseMode::Inactive
            || authority.phase_open
            || authority.quiescent_version != self.expected_version
        {
            authority.mode = ParserLeaseMode::Retired;
            self.retired = true;
            return Err(BrowserHostError::StaleTask);
        }
        if authority.rooted_nodes.len() != self.roots.len()
            || authority
                .rooted_nodes
                .iter()
                .zip(&self.roots)
                .any(|(node, root)| *node != root.node_id)
        {
            authority.mode = ParserLeaseMode::Retired;
            self.retired = true;
            return Err(BrowserHostError::Internal);
        }

        let mut state = self
            .document
            .inner
            .lock()
            .map_err(|_| BrowserHostError::Internal)?;
        if !state.current
            || state.residency != DocumentResidency::Host
            || state.document.version() != self.expected_version
            || state.publication != DocumentPublication::Unmanaged
        {
            authority.mode = ParserLeaseMode::Retired;
            self.retired = true;
            return Err(BrowserHostError::StaleDocument);
        }
        if state.document.revision() != 0
            || !state
                .document
                .children(state.document.document_node())
                .map_err(map_dom_read_error)?
                .is_empty()
        {
            return Err(BrowserHostError::InvalidOperation);
        }
        if let Err(error) = validate_parser_roots(&state.document, &authority.rooted_nodes) {
            authority.mode = ParserLeaseMode::Retired;
            self.retired = true;
            return Err(error);
        }
        let sequence = 1;
        let document = mem::replace(&mut state.document, placeholder);
        state.residency = DocumentResidency::Parser {
            generation: self.generation,
            sequence,
        };
        state.publication = DocumentPublication::ParserLent {
            generation: self.generation,
            sequence,
        };
        authority.mode = ParserLeaseMode::Lent {
            sequence,
            before: self.expected_version,
        };
        Ok(LentParserDocument {
            document,
            lease: ParserDocumentLease {
                document: self.document.inner.clone(),
                authority: self.parser_authority.clone(),
                generation: self.generation,
                sequence,
                before: self.expected_version,
            },
        })
    }

    fn ensure_active(&mut self) -> Result<(), BrowserHostError> {
        if self.retired {
            return Err(BrowserHostError::StaleTask);
        }
        let host_mode = {
            let mut authority = self
                .parser_authority
                .lock()
                .map_err(|_| BrowserHostError::Internal)?;
            if authority.generation != self.generation {
                authority.mode = ParserLeaseMode::Retired;
                self.retired = true;
                return Err(BrowserHostError::StaleTask);
            }
            match authority.mode {
                ParserLeaseMode::Inactive => ActiveHostMode::General,
                ParserLeaseMode::Attached { sequence } => {
                    ActiveHostMode::ParserBoundary { sequence }
                }
                ParserLeaseMode::Complete { sequence } => {
                    ActiveHostMode::ParserCompletion { sequence }
                }
                ParserLeaseMode::Restored {
                    sequence,
                    before,
                    after,
                    complete,
                } => {
                    if authority.phase_open
                        || authority.quiescent_version != before
                        || self.expected_version != before
                        || self.phase_before != before
                        || self.phase_commands != 0
                        || self.phase_created_nodes != 0
                    {
                        authority.mode = ParserLeaseMode::Retired;
                        self.retired = true;
                        return Err(BrowserHostError::VersionMismatch);
                    }
                    self.expected_version = after;
                    self.phase_before = after;
                    authority.quiescent_version = after;
                    authority.mode = if complete {
                        ParserLeaseMode::Complete { sequence }
                    } else {
                        ParserLeaseMode::Attached { sequence }
                    };
                    if complete {
                        ActiveHostMode::ParserCompletion { sequence }
                    } else {
                        ActiveHostMode::ParserBoundary { sequence }
                    }
                }
                ParserLeaseMode::Lent { .. } | ParserLeaseMode::Retired => {
                    authority.mode = ParserLeaseMode::Retired;
                    self.retired = true;
                    return Err(BrowserHostError::StaleTask);
                }
            }
        };
        let actual = {
            let state = self
                .document
                .inner
                .lock()
                .map_err(|_| BrowserHostError::Internal)?;
            if !state.current || state.residency != DocumentResidency::Host {
                drop(state);
                self.retire();
                return Err(BrowserHostError::StaleDocument);
            }
            let publication_matches = match host_mode {
                ActiveHostMode::General => matches!(
                    state.publication,
                    DocumentPublication::Unmanaged | DocumentPublication::Published { .. }
                ),
                ActiveHostMode::ParserBoundary { sequence } => matches!(
                    state.publication,
                    DocumentPublication::PreCheckpointActive {
                        generation,
                        sequence: candidate
                    } | DocumentPublication::ClassicActive {
                        generation,
                        sequence: candidate
                    } | DocumentPublication::PostCheckpointActive {
                        generation,
                        sequence: candidate
                    } if generation == self.generation && candidate == sequence
                ),
                ActiveHostMode::ParserCompletion { sequence } => {
                    state.publication
                        == (DocumentPublication::FinalCheckpointActive {
                            generation: self.generation,
                            sequence,
                        })
                }
            };
            if !publication_matches {
                drop(state);
                self.retire();
                return Err(BrowserHostError::StaleTask);
            }
            state.document.version()
        };
        if actual.document_id() != self.expected_version.document_id() {
            self.retire();
            return Err(BrowserHostError::StaleDocument);
        }
        if actual != self.expected_version {
            self.retire();
            return Err(BrowserHostError::VersionMismatch);
        }
        Ok(())
    }

    fn retire(&mut self) {
        self.retired = true;
        retire_parser_authority(&self.parser_authority);
    }

    fn account_host_call(&mut self) -> Result<(), BrowserHostError> {
        self.ensure_active()?;
        let next = self
            .host_calls
            .checked_add(1)
            .ok_or(BrowserHostError::LimitExceeded)?;
        if next > self.limits.max_commands() {
            self.retire();
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
            self.retire();
            return Err(BrowserHostError::LimitExceeded);
        }
        self.created_nodes = next;
        Ok(())
    }

    fn account_strings(&mut self, values: &[&str]) -> Result<(), BrowserHostError> {
        for value in values {
            if value.len() > self.limits.max_string_bytes() {
                self.retire();
                return Err(BrowserHostError::LimitExceeded);
            }
            let next = self
                .total_string_bytes
                .checked_add(value.len())
                .ok_or(BrowserHostError::LimitExceeded)?;
            if next > self.limits.max_total_string_bytes() {
                self.retire();
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
            .map_err(|_| BrowserHostError::Allocation)?;
        self.parser_authority
            .lock()
            .map_err(|_| BrowserHostError::Internal)?
            .rooted_nodes
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
            let authority = self
                .parser_authority
                .lock()
                .map_err(|_| BrowserHostError::Internal)?;
            if authority.rooted_nodes.get(index) != Some(&root.node_id) {
                return Err(BrowserHostError::Internal);
            }
            return self.encode_root_index(index);
        }
        self.reserve_root()?;
        self.record_reserved_root(root)
    }

    fn record_reserved_root(
        &mut self,
        root: RootedDomNode,
    ) -> Result<BrowserHostNodeToken, BrowserHostError> {
        let index = self.roots.len();
        {
            let mut authority = self
                .parser_authority
                .lock()
                .map_err(|_| BrowserHostError::Internal)?;
            if authority.rooted_nodes.len() != index {
                authority.mode = ParserLeaseMode::Retired;
                return Err(BrowserHostError::Internal);
            }
            authority.rooted_nodes.push(root.node_id);
        }
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
            self.retire();
            return Err(BrowserHostError::StaleTask);
        }
        let low = (token.get() & TOKEN_SLOT_MASK) as u32;
        let index = low.checked_sub(1).ok_or(BrowserHostError::InvalidNode)? as usize;
        let root = self.roots.get(index).ok_or(BrowserHostError::InvalidNode)?;
        if !self.document.is_live(root) {
            self.retire();
            return Err(BrowserHostError::StaleDocument);
        }
        Ok(root.node_id)
    }

    fn apply_one(
        &mut self,
        command: ScriptMutationCommand,
    ) -> Result<wild_buzzard_dom::bindings::ScriptMutationCommit, BrowserHostError> {
        self.ensure_active()?;
        let next_phase_commands = self
            .phase_commands
            .checked_add(1)
            .ok_or(BrowserHostError::LimitExceeded)?;
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
        if !state.current || state.residency != DocumentResidency::Host {
            return Err(BrowserHostError::StaleDocument);
        }
        if state.document.version() != self.expected_version {
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
        self.phase_commands = next_phase_commands;
        drop(state);
        let mut authority = self
            .parser_authority
            .lock()
            .map_err(|_| BrowserHostError::Internal)?;
        if authority.generation != self.generation
            || matches!(
                authority.mode,
                ParserLeaseMode::Lent { .. }
                    | ParserLeaseMode::Restored { .. }
                    | ParserLeaseMode::Retired
            )
        {
            authority.mode = ParserLeaseMode::Retired;
            return Err(BrowserHostError::StaleTask);
        }
        authority.phase_open = true;
        Ok(commit)
    }

    fn mark_error(&mut self, error: BrowserHostError) -> BrowserHostError {
        if !matches!(
            error,
            BrowserHostError::InvalidArgument
                | BrowserHostError::InvalidNode
                | BrowserHostError::InvalidOperation
        ) {
            self.retire();
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
        let next_phase_created_nodes = self
            .phase_created_nodes
            .checked_add(1)
            .ok_or(BrowserHostError::LimitExceeded)
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
        let token = self
            .record_reserved_root(root)
            .map_err(|error| self.mark_error(error))?;
        self.phase_created_nodes = next_phase_created_nodes;
        Ok(token)
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
        let next_phase_created_nodes = self
            .phase_created_nodes
            .checked_add(1)
            .ok_or(BrowserHostError::LimitExceeded)
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
        let token = self
            .record_reserved_root(root)
            .map_err(|error| self.mark_error(error))?;
        self.phase_created_nodes = next_phase_created_nodes;
        Ok(token)
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
        let mut authority = match self.parser_authority.lock() {
            Ok(authority) => authority,
            Err(_) => {
                self.retired = true;
                return Err(BrowserHostError::Internal);
            }
        };
        if authority.generation != self.generation
            || matches!(
                authority.mode,
                ParserLeaseMode::Lent { .. }
                    | ParserLeaseMode::Restored { .. }
                    | ParserLeaseMode::Retired
            )
        {
            authority.mode = ParserLeaseMode::Retired;
            self.retired = true;
            return Err(BrowserHostError::StaleTask);
        }
        authority.quiescent_version = self.expected_version;
        authority.phase_open = false;
        Ok(outcome)
    }

    fn abort_phase(&mut self) {
        self.retire();
        self.phase_commands = 0;
        self.phase_created_nodes = 0;
    }
}

fn host_version(version: DocumentVersion) -> BrowserHostDocumentVersion {
    BrowserHostDocumentVersion::new(version.document_id().get(), version.revision())
}

fn retire_parser_authority(authority: &Arc<Mutex<ParserLeaseAuthority>>) {
    let Ok(mut authority) = authority.lock() else {
        std::process::abort();
    };
    authority.mode = ParserLeaseMode::Retired;
    authority.phase_open = false;
}

fn validate_parser_roots(document: &Document, roots: &[NodeId]) -> Result<(), BrowserHostError> {
    for root in roots {
        if root.document_id() != document.id() || document.node_kind(*root).is_err() {
            return Err(BrowserHostError::InvalidNode);
        }
    }
    Ok(())
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
