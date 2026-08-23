//! Browser-owned coordination between HTML parsing, Brimstone, and the Rust DOM.
//!
//! Product admission is intentionally disabled. The first executable gate is
//! available only through the opt-in `contained_inline_classic` feature and
//! accepts deterministic numeric-loopback documents. It proves parser ordering
//! and ownership; it is not general-web JavaScript admission.

#![forbid(unsafe_code)]

/// General-web product script admission remains compile-time disabled.
pub const PRODUCT_SCRIPT_ADMISSION_ENABLED: bool = false;

#[cfg(feature = "contained_inline_classic")]
mod contained {
    use std::{
        fmt,
        rc::Rc,
        time::{Duration, Instant},
    };

    use brimstone_core::{
        common::options::OptionsBuilder,
        runtime::{
            BrowserHostError, BrowserHostPhaseOutcome, ClassicScriptLimits, ClassicScriptOutcome,
            ClassicScriptRequest, ContextBuilder, InterruptReason, MicrotaskCheckpointOutcome,
            OwnedContext, ScriptInterruptHandle, ScriptValueSummary,
        },
    };
    use wild_buzzard_dom::bindings::ScriptMutationLimits;
    use wild_buzzard_dom::{Document, DocumentSnapshot, DocumentVersion, NodeId, NodeKind};
    use wild_buzzard_dom_script_bridge::{ParserPhaseError, RootedDomTask, ScriptDocument};
    use wild_buzzard_html::{
        HtmlParser, ParserInsertedScript, ParserScriptStartTag, ParserStateError,
        ScriptHandlerError, TokenizerLimits,
    };
    use wild_buzzard_net::{CancellationSource, CancellationToken, LoopbackTarget};

    const MAX_DOCUMENT_SCRIPT_CANDIDATES: usize = 64;
    const MAX_DOCUMENT_INLINE_SOURCE_BYTES: usize = 4 * 1024 * 1024;
    const MAX_SCRIPT_ATTRIBUTE_BYTES: usize = 4 * 1024;
    const MAX_CONTEXT_HEAP_BYTES: usize = 64 * 1024 * 1024;
    const PARSER_POLL_CHUNK_BYTES: usize = 16 * 1024;
    const MAX_BRIMSTONE_DOCUMENT_TIME: Duration = Duration::from_secs(30);

    /// Owner that requests both browser-task cancellation and Brimstone interruption.
    #[derive(Clone, Debug, Default)]
    pub struct ScriptLoopCancellationSource {
        browser: CancellationSource,
        script: ScriptInterruptHandle,
    }

    impl ScriptLoopCancellationSource {
        #[must_use]
        pub fn new() -> Self {
            Self::default()
        }

        #[must_use]
        pub fn token(&self) -> ScriptLoopCancellationToken {
            ScriptLoopCancellationToken {
                browser: self.browser.token(),
                script: self.script.clone(),
            }
        }

        /// Requests both halves of cancellation. Exactly one caller observes a new request.
        #[must_use]
        pub fn cancel(&self) -> bool {
            self.script.request_interrupt();
            self.browser.cancel()
        }
    }

    /// Read-only paired cancellation authority for one parser/script task.
    #[derive(Clone, Debug)]
    pub struct ScriptLoopCancellationToken {
        browser: CancellationToken,
        script: ScriptInterruptHandle,
    }

    impl ScriptLoopCancellationToken {
        #[must_use]
        pub fn is_cancelled(&self) -> bool {
            self.browser.is_cancelled()
        }

        /// Borrows the exact browser-task cancellation half used by this
        /// parser/script operation.
        ///
        /// Network and rendering stages use this same token so cancellation
        /// cannot leave either side of one coordinated load running alone.
        #[must_use]
        pub const fn browser_cancellation(&self) -> &CancellationToken {
            &self.browser
        }

        fn checkpoint(&self, deadline: Instant) -> Result<(), ScriptLoopError> {
            if self.browser.is_cancelled() {
                return Err(ScriptLoopError::Cancelled);
            }
            if Instant::now() >= deadline {
                return Err(ScriptLoopError::Deadline);
            }
            Ok(())
        }
    }

    /// Why a parser-inserted script did not enter Brimstone.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum SkippedScriptReason {
        ExternalClassic,
        NoModule,
        Module,
        ImportMap,
        UnsupportedType,
    }

    /// Bounded observable disposition for one parser-inserted candidate.
    #[derive(Clone, Debug, PartialEq)]
    pub enum ScriptDisposition {
        Success(ScriptValueSummary),
        Thrown(ScriptValueSummary),
        ParseError { diagnostics: usize },
        AnalyzeError { diagnostics: usize },
        CompileError,
        Skipped(SkippedScriptReason),
    }

    /// Evidence for one exact parser boundary.
    #[derive(Clone, Debug, PartialEq)]
    pub struct ScriptBoundaryEvidence {
        ordinal: u32,
        node: NodeId,
        parser_version: DocumentVersion,
        completed_version: DocumentVersion,
        source_bytes: usize,
        disposition: ScriptDisposition,
    }

    impl ScriptBoundaryEvidence {
        #[must_use]
        pub const fn ordinal(&self) -> u32 {
            self.ordinal
        }

        #[must_use]
        pub const fn node(&self) -> NodeId {
            self.node
        }

        #[must_use]
        pub const fn parser_version(&self) -> DocumentVersion {
            self.parser_version
        }

        #[must_use]
        pub const fn completed_version(&self) -> DocumentVersion {
            self.completed_version
        }

        #[must_use]
        pub const fn source_bytes(&self) -> usize {
            self.source_bytes
        }

        #[must_use]
        pub const fn disposition(&self) -> &ScriptDisposition {
            &self.disposition
        }
    }

    /// Final parser/script evidence for one exact document revision.
    #[derive(Clone, Debug, PartialEq)]
    pub struct ScriptLoopReport {
        input_bytes: usize,
        html_diagnostics: usize,
        final_version: DocumentVersion,
        boundaries: Vec<ScriptBoundaryEvidence>,
    }

    impl ScriptLoopReport {
        #[must_use]
        pub const fn input_bytes(&self) -> usize {
            self.input_bytes
        }

        #[must_use]
        pub const fn html_diagnostics(&self) -> usize {
            self.html_diagnostics
        }

        #[must_use]
        pub const fn final_version(&self) -> DocumentVersion {
            self.final_version
        }

        #[must_use]
        pub fn boundaries(&self) -> &[ScriptBoundaryEvidence] {
            &self.boundaries
        }
    }

    /// Live owner retained after parser completion for later event-loop work.
    pub struct ScriptedDocument {
        context: OwnedContext,
        document: ScriptDocument,
        host: RootedDomTask,
        report: ScriptLoopReport,
    }

    impl fmt::Debug for ScriptedDocument {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter
                .debug_struct("ScriptedDocument")
                .field("report", &self.report)
                .finish_non_exhaustive()
        }
    }

    impl ScriptedDocument {
        #[must_use]
        pub const fn report(&self) -> &ScriptLoopReport {
            &self.report
        }

        /// Returns the exact live document snapshot retained after parser completion.
        ///
        /// # Errors
        ///
        /// Returns a host error if the document was retired or its lock was poisoned.
        pub fn snapshot(&self) -> Result<DocumentSnapshot, ScriptLoopError> {
            self.document.snapshot().map_err(ScriptLoopError::Host)
        }

        /// Returns the exact live document identity and revision.
        ///
        /// # Errors
        ///
        /// Returns a host error if the document was retired or its lock was poisoned.
        pub fn current_version(&self) -> Result<DocumentVersion, ScriptLoopError> {
            self.document
                .current_version()
                .map_err(ScriptLoopError::Host)
        }

        /// Keeps the owner-thread runtime and rooted host visibly retained.
        #[must_use]
        pub fn retained_owner_count(&self) -> usize {
            let _ = (&self.context, &self.host);
            1
        }
    }

    /// Terminal failure for the contained parser/script task.
    #[derive(Debug)]
    pub enum ScriptLoopError {
        NonLoopbackSource,
        Cancelled,
        Deadline,
        ContextInitialization,
        Allocation,
        Parser(ParserStateError),
        Host(BrowserHostError),
        Runtime(ClassicScriptOutcome),
        Checkpoint(MicrotaskCheckpointOutcome),
        HostDisposition(BrowserHostPhaseOutcome),
        SourceLimit { requested: usize, limit: usize },
        AttributeLimit { requested: usize, limit: usize },
        Invariant(&'static str),
    }

    /// Authoritative control terminal carried by a completed script-loop
    /// result. Later clock or cancellation-token observations must never
    /// replace this classification.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum ScriptLoopControlFailure {
        Cancelled,
        Deadline,
    }

    impl ScriptLoopError {
        /// Returns the exact cancellation/deadline terminal already stored in
        /// this error, including nested Brimstone and host outcomes.
        #[must_use]
        pub const fn control_failure(&self) -> Option<ScriptLoopControlFailure> {
            match self {
                Self::Cancelled
                | Self::Host(BrowserHostError::Cancelled)
                | Self::Runtime(ClassicScriptOutcome::Interrupted(
                    InterruptReason::ExternalRequest,
                ))
                | Self::Runtime(ClassicScriptOutcome::HostFailure(BrowserHostError::Cancelled))
                | Self::Checkpoint(MicrotaskCheckpointOutcome::Interrupted(
                    InterruptReason::ExternalRequest,
                ))
                | Self::Checkpoint(MicrotaskCheckpointOutcome::HostFailure(
                    BrowserHostError::Cancelled,
                ))
                | Self::HostDisposition(BrowserHostPhaseOutcome::Failed(
                    BrowserHostError::Cancelled,
                )) => Some(ScriptLoopControlFailure::Cancelled),
                Self::Deadline
                | Self::Runtime(ClassicScriptOutcome::Interrupted(InterruptReason::Deadline))
                | Self::Checkpoint(MicrotaskCheckpointOutcome::Interrupted(
                    InterruptReason::Deadline,
                )) => Some(ScriptLoopControlFailure::Deadline),
                _ => None,
            }
        }
    }

    impl fmt::Display for ScriptLoopError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                Self::NonLoopbackSource => {
                    formatter.write_str("contained script source is not numeric loopback HTTP")
                }
                Self::Cancelled => formatter.write_str("scripted document task was cancelled"),
                Self::Deadline => formatter.write_str("scripted document deadline elapsed"),
                Self::ContextInitialization => {
                    formatter.write_str("Brimstone context initialization failed")
                }
                Self::Allocation => formatter.write_str("script coordinator allocation failed"),
                Self::Parser(error) => write!(formatter, "HTML parser failed: {error}"),
                Self::Host(error) => write!(formatter, "DOM host failed: {error:?}"),
                Self::Runtime(outcome) => {
                    write!(formatter, "document JavaScript session failed: {outcome:?}")
                }
                Self::Checkpoint(outcome) => {
                    write!(
                        formatter,
                        "document microtask checkpoint failed: {outcome:?}"
                    )
                }
                Self::HostDisposition(outcome) => {
                    write!(formatter, "DOM host phase was not committed: {outcome:?}")
                }
                Self::SourceLimit { requested, limit } => {
                    write!(
                        formatter,
                        "inline script source {requested} exceeds {limit} bytes"
                    )
                }
                Self::AttributeLimit { requested, limit } => {
                    write!(
                        formatter,
                        "script attribute {requested} exceeds {limit} bytes"
                    )
                }
                Self::Invariant(detail) => {
                    write!(formatter, "script coordinator invariant: {detail}")
                }
            }
        }
    }

    impl std::error::Error for ScriptLoopError {}

    /// Parses and executes only contained numeric-loopback inline classic scripts.
    ///
    /// The same `OwnedContext`, initial realm, rooted DOM task, cumulative budget,
    /// and document identity span every parser pause. General-web URLs, external
    /// scripts, modules, import maps, and `nomodule` candidates never execute.
    ///
    /// # Errors
    ///
    /// Returns a typed terminal error for source policy, parser/DOM/runtime,
    /// cancellation, deadline, allocation, resource, or authority failure.
    #[allow(clippy::too_many_lines)]
    pub fn parse_contained_numeric_loopback_document(
        url: &str,
        source: &str,
        parser_limits: TokenizerLimits,
        mutation_limits: ScriptMutationLimits,
        cancellation: &ScriptLoopCancellationToken,
        deadline: Instant,
    ) -> Result<ScriptedDocument, ScriptLoopError> {
        cancellation.checkpoint(deadline)?;
        let target = LoopbackTarget::parse(url).map_err(|_| ScriptLoopError::NonLoopbackSource)?;
        let canonical_url = target.url().as_str();

        let document_limits = ClassicScriptLimits::parser_blocking_document(
            MAX_BRIMSTONE_DOCUMENT_TIME,
        )
        .map_err(|outcome| {
            ScriptLoopError::Runtime(ClassicScriptOutcome::InvalidDocumentSession(outcome))
        })?;

        let options = OptionsBuilder::new()
            .serialized_heap(None)
            .min_heap_size(MAX_CONTEXT_HEAP_BYTES)
            .max_heap_size(MAX_CONTEXT_HEAP_BYTES)
            .build()
            .map_err(|_| ScriptLoopError::ContextInitialization)?;
        let mut context = ContextBuilder::new()
            .set_options(Rc::new(options))
            .build()
            .map_err(|_| ScriptLoopError::ContextInitialization)?;

        let document = ScriptDocument::new(Document::new());
        let initial_version = document.current_version().map_err(ScriptLoopError::Host)?;
        let mut host = document
            .begin_task(mutation_limits)
            .map_err(ScriptLoopError::Host)?;
        let (parser_document, initial_lease) = host
            .lend_document_to_parser()
            .map_err(ScriptLoopError::Host)?
            .into_parts();
        if parser_document.version() != initial_version {
            return Err(ScriptLoopError::Invariant(
                "initial parser document version drifted",
            ));
        }
        let mut parser = HtmlParser::from_pristine_document(parser_limits, parser_document)
            .map_err(ScriptLoopError::Parser)?;
        let mut lease = Some(initial_lease);
        let mut boundaries = Vec::new();
        boundaries
            .try_reserve_exact(MAX_DOCUMENT_SCRIPT_CANDIDATES)
            .map_err(|_| ScriptLoopError::Allocation)?;
        let mut cumulative_source_bytes = 0usize;

        let session = context.with_browser_script_realm(|realm| {
            realm.with_hosted_document_script_budget_until(
                &mut host,
                document_limits,
                &cancellation.script,
                deadline,
                |document_session| {
                    let mut handler = |parser_document: &mut Document,
                                       script: ParserInsertedScript|
                     -> Result<(), ScriptLoopError> {
                        cancellation.checkpoint(deadline)?;
                        let script_node = script.node();
                        let parser_version = script.document_version();
                        let start_tag = script.start_tag().clone();
                        let ordinal = u32::try_from(script.ordinal())
                            .map_err(|_| ScriptLoopError::Invariant("script ordinal overflow"))?;
                        let expected_ordinal = u32::try_from(boundaries.len() + 1)
                            .map_err(|_| ScriptLoopError::Invariant("script ordinal overflow"))?;
                        if ordinal != expected_ordinal {
                            return Err(ScriptLoopError::Invariant(
                                "parser and coordinator script ordinals disagree",
                            ));
                        }
                        let current_lease = lease.take().ok_or(ScriptLoopError::Invariant(
                            "parser lease was already consumed",
                        ))?;
                        let restored = document
                            .restore_parser_boundary(parser_document, current_lease, script)
                            .map_err(ScriptLoopError::Host)?;

                        let prepared = restored
                            .perform_pre_checkpoint(document_session)
                            .map_err(map_parser_phase_error)?;
                        validate_checkpoint(prepared.pre_checkpoint())?;
                        cancellation.checkpoint(deadline)?;

                        let snapshot = prepared.snapshot().map_err(ScriptLoopError::Host)?;
                        let frozen = freeze_script_candidate(&snapshot, script_node, &start_tag)?;
                        cumulative_source_bytes = cumulative_source_bytes
                            .checked_add(frozen.source.len())
                            .ok_or(ScriptLoopError::SourceLimit {
                                requested: usize::MAX,
                                limit: MAX_DOCUMENT_INLINE_SOURCE_BYTES,
                            })?;
                        if cumulative_source_bytes > MAX_DOCUMENT_INLINE_SOURCE_BYTES {
                            return Err(ScriptLoopError::SourceLimit {
                                requested: cumulative_source_bytes,
                                limit: MAX_DOCUMENT_INLINE_SOURCE_BYTES,
                            });
                        }

                        let (disposition, completed) = match frozen.classification {
                            ScriptClassification::AdmittedClassic => {
                                let request =
                                    ClassicScriptRequest::new(&frozen.source, canonical_url)
                                        .with_base_url(canonical_url);
                                let executed = prepared
                                    .execute_classic(document_session, request)
                                    .map_err(map_parser_phase_error)?;
                                let disposition =
                                    summarize_classic_execution(executed.execution())?;
                                cancellation.checkpoint(deadline)?;
                                let completed = executed
                                    .perform_post_checkpoint(document_session)
                                    .map_err(map_parser_phase_error)?;
                                validate_checkpoint(completed.post_checkpoint().ok_or(
                                    ScriptLoopError::Invariant(
                                        "admitted classic script omitted its post checkpoint",
                                    ),
                                )?)?;
                                (disposition, completed)
                            }
                            ScriptClassification::Skipped(reason) => {
                                let completed = prepared
                                    .skip(document_session, frozen.source.len())
                                    .map_err(map_parser_phase_error)?;
                                (ScriptDisposition::Skipped(reason), completed)
                            }
                        };
                        cancellation.checkpoint(deadline)?;
                        let completed_version =
                            completed.current_version().map_err(ScriptLoopError::Host)?;
                        boundaries.push(ScriptBoundaryEvidence {
                            ordinal,
                            node: script_node,
                            parser_version,
                            completed_version,
                            source_bytes: frozen.source.len(),
                            disposition,
                        });
                        lease = Some(
                            completed
                                .lend_back_to_parser()
                                .map_err(ScriptLoopError::Host)?,
                        );
                        Ok(())
                    };

                    let mut offset = 0usize;
                    while offset < source.len() {
                        cancellation.checkpoint(deadline)?;
                        let end = next_utf8_chunk_end(source, offset);
                        parser
                            .feed_with_script_handler(&source[offset..end], &mut handler)
                            .map_err(flatten_script_handler_error)?;
                        offset = end;
                        cancellation.checkpoint(deadline)?;
                    }
                    let parsed = parser
                        .finish_with_script_handler(&mut handler)
                        .map_err(flatten_script_handler_error)?;
                    cancellation.checkpoint(deadline)?;
                    let final_lease = lease
                        .take()
                        .ok_or(ScriptLoopError::Invariant("final parser lease is missing"))?;
                    let html_diagnostics = parsed.errors.len();
                    let completion = document
                        .restore_parser_completion(final_lease, parsed)
                        .map_err(ScriptLoopError::Host)?;
                    let published = completion
                        .perform_final_checkpoint(document_session)
                        .map_err(map_parser_phase_error)?;
                    validate_checkpoint(published.final_checkpoint())?;
                    cancellation.checkpoint(deadline)?;
                    let snapshot = published.snapshot().map_err(ScriptLoopError::Host)?;
                    Ok::<_, ScriptLoopError>((snapshot, html_diagnostics))
                },
            )
        });

        let (snapshot, html_diagnostics) = match session {
            Ok(Ok(value)) => value,
            Ok(Err(error)) => {
                retire_failed_document(&mut host, &document);
                return Err(error);
            }
            Err(outcome) => {
                retire_failed_document(&mut host, &document);
                return Err(ScriptLoopError::Runtime(outcome));
            }
        };
        if let Err(error) = cancellation.checkpoint(deadline) {
            retire_failed_document(&mut host, &document);
            return Err(error);
        }
        if snapshot.document_id() != initial_version.document_id()
            || snapshot.version() != host.expected_version()
            || snapshot.version() != document.current_version().map_err(ScriptLoopError::Host)?
        {
            retire_failed_document(&mut host, &document);
            return Err(ScriptLoopError::Invariant(
                "final parser, host, and snapshot versions disagree",
            ));
        }

        let report = ScriptLoopReport {
            input_bytes: source.len(),
            html_diagnostics,
            final_version: snapshot.version(),
            boundaries,
        };
        Ok(ScriptedDocument {
            context,
            document,
            host,
            report,
        })
    }

    fn retire_failed_document(host: &mut RootedDomTask, document: &ScriptDocument) {
        use brimstone_core::runtime::BrowserHostTask;

        host.abort_phase();
        if document.retire().is_err() {
            std::process::abort();
        }
    }

    fn flatten_script_handler_error(error: ScriptHandlerError<ScriptLoopError>) -> ScriptLoopError {
        match error {
            ScriptHandlerError::Parser(error) => ScriptLoopError::Parser(error),
            ScriptHandlerError::Handler(error) => error,
        }
    }

    fn map_parser_phase_error(error: ParserPhaseError) -> ScriptLoopError {
        match error {
            ParserPhaseError::Host(error) => ScriptLoopError::Host(error),
            ParserPhaseError::Checkpoint(execution) => {
                let execution = *execution;
                if execution.checkpoint.outcome != MicrotaskCheckpointOutcome::Complete {
                    ScriptLoopError::Checkpoint(execution.checkpoint.outcome)
                } else {
                    ScriptLoopError::HostDisposition(execution.host)
                }
            }
            ParserPhaseError::Classic(execution) => {
                let execution = *execution;
                if !matches!(execution.host, BrowserHostPhaseOutcome::Completed(_)) {
                    ScriptLoopError::HostDisposition(execution.host)
                } else {
                    ScriptLoopError::Runtime(execution.script.outcome)
                }
            }
            ParserPhaseError::Skipped(outcome) => ScriptLoopError::Runtime(outcome),
        }
    }

    fn validate_checkpoint(
        execution: &brimstone_core::runtime::BrowserHostMicrotaskExecution,
    ) -> Result<(), ScriptLoopError> {
        if execution.checkpoint.outcome != MicrotaskCheckpointOutcome::Complete {
            return Err(ScriptLoopError::Checkpoint(
                execution.checkpoint.outcome.clone(),
            ));
        }
        if !matches!(execution.host, BrowserHostPhaseOutcome::Completed(_)) {
            return Err(ScriptLoopError::HostDisposition(execution.host));
        }
        if execution.checkpoint.report.jit_enabled
            || execution.checkpoint.report.jit_native_entries != 0
        {
            return Err(ScriptLoopError::Invariant(
                "contained checkpoint entered product-disabled JIT",
            ));
        }
        Ok(())
    }

    fn summarize_classic_execution(
        execution: &brimstone_core::runtime::BrowserHostClassicExecution,
    ) -> Result<ScriptDisposition, ScriptLoopError> {
        if execution.script.report.jit_enabled || execution.script.report.jit_native_entries != 0 {
            return Err(ScriptLoopError::Invariant(
                "contained classic script entered product-disabled JIT",
            ));
        }
        if !matches!(execution.host, BrowserHostPhaseOutcome::Completed(_)) {
            return Err(ScriptLoopError::HostDisposition(execution.host));
        }
        match &execution.script.outcome {
            ClassicScriptOutcome::Success(value) => Ok(ScriptDisposition::Success(*value)),
            ClassicScriptOutcome::Thrown(value) => Ok(ScriptDisposition::Thrown(*value)),
            ClassicScriptOutcome::ParseError(diagnostics) => Ok(ScriptDisposition::ParseError {
                diagnostics: diagnostics.len(),
            }),
            ClassicScriptOutcome::AnalyzeError(diagnostics) => {
                Ok(ScriptDisposition::AnalyzeError {
                    diagnostics: diagnostics.len(),
                })
            }
            ClassicScriptOutcome::CompileError(_) => Ok(ScriptDisposition::CompileError),
            terminal => Err(ScriptLoopError::Runtime(terminal.clone())),
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum ScriptClassification {
        AdmittedClassic,
        Skipped(SkippedScriptReason),
    }

    struct FrozenScriptCandidate {
        source: String,
        classification: ScriptClassification,
    }

    fn freeze_script_candidate(
        snapshot: &DocumentSnapshot,
        script: NodeId,
        start_tag: &ParserScriptStartTag,
    ) -> Result<FrozenScriptCandidate, ScriptLoopError> {
        let node = snapshot.node(script).ok_or(ScriptLoopError::Invariant(
            "script node is absent from live snapshot",
        ))?;
        let NodeKind::Element(element) = &node.kind else {
            return Err(ScriptLoopError::Invariant(
                "script boundary names a non-element",
            ));
        };
        if element.name.local_name != "script" {
            return Err(ScriptLoopError::Invariant(
                "script boundary names a non-script element",
            ));
        }
        let raw_script_type = start_tag.script_type();
        if let Some(value) = raw_script_type
            && value.len() > MAX_SCRIPT_ATTRIBUTE_BYTES
        {
            return Err(ScriptLoopError::AttributeLimit {
                requested: value.len(),
                limit: MAX_SCRIPT_ATTRIBUTE_BYTES,
            });
        }

        let source_len = node.children.iter().try_fold(0usize, |total, child| {
            let Some(child) = snapshot.node(*child) else {
                return Err(ScriptLoopError::Invariant(
                    "script child is absent from snapshot",
                ));
            };
            let bytes = match &child.kind {
                NodeKind::Text(data) => data.len(),
                _ => 0,
            };
            total
                .checked_add(bytes)
                .ok_or(ScriptLoopError::SourceLimit {
                    requested: usize::MAX,
                    limit: MAX_DOCUMENT_INLINE_SOURCE_BYTES,
                })
        })?;
        if source_len > MAX_DOCUMENT_INLINE_SOURCE_BYTES {
            return Err(ScriptLoopError::SourceLimit {
                requested: source_len,
                limit: MAX_DOCUMENT_INLINE_SOURCE_BYTES,
            });
        }
        let mut source = String::new();
        source
            .try_reserve_exact(source_len)
            .map_err(|_| ScriptLoopError::Allocation)?;
        for child in &node.children {
            if let Some(wild_buzzard_dom::SnapshotNode {
                kind: NodeKind::Text(data),
                ..
            }) = snapshot.node(*child)
            {
                source.push_str(data);
            }
        }

        let script_type = raw_script_type.map(trim_ascii_whitespace);
        let classification = if matches!(script_type, Some(value) if value.eq_ignore_ascii_case("module"))
        {
            ScriptClassification::Skipped(SkippedScriptReason::Module)
        } else if matches!(script_type, Some(value) if value.eq_ignore_ascii_case("importmap")) {
            ScriptClassification::Skipped(SkippedScriptReason::ImportMap)
        } else if !is_admitted_classic_type(script_type) {
            ScriptClassification::Skipped(SkippedScriptReason::UnsupportedType)
        } else if start_tag.no_module_present() {
            ScriptClassification::Skipped(SkippedScriptReason::NoModule)
        } else if start_tag.src().is_some() {
            ScriptClassification::Skipped(SkippedScriptReason::ExternalClassic)
        } else {
            ScriptClassification::AdmittedClassic
        };
        Ok(FrozenScriptCandidate {
            source,
            classification,
        })
    }

    fn trim_ascii_whitespace(value: &str) -> &str {
        value.trim_matches(|character| {
            matches!(
                character,
                '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | '\u{0020}'
            )
        })
    }

    fn is_admitted_classic_type(script_type: Option<&str>) -> bool {
        match script_type {
            None | Some("") => true,
            Some(value) => value.eq_ignore_ascii_case("text/javascript"),
        }
    }

    fn next_utf8_chunk_end(source: &str, offset: usize) -> usize {
        let mut end = source
            .len()
            .min(offset.saturating_add(PARSER_POLL_CHUNK_BYTES));
        while end > offset && !source.is_char_boundary(end) {
            end -= 1;
        }
        if end == offset {
            source[offset..]
                .char_indices()
                .nth(1)
                .map_or(source.len(), |(relative, _)| offset + relative)
        } else {
            end
        }
    }

    #[cfg(test)]
    mod tests {
        use std::{thread, time::Duration};

        use super::*;

        fn run(source: &str) -> Result<ScriptedDocument, ScriptLoopError> {
            let cancellation = ScriptLoopCancellationSource::new();
            parse_contained_numeric_loopback_document(
                "http://127.0.0.1:8080/index.html",
                source,
                TokenizerLimits::default(),
                ScriptMutationLimits::DEFAULT,
                &cancellation.token(),
                Instant::now() + Duration::from_secs(5),
            )
        }

        #[test]
        fn one_realm_and_rooted_task_span_parser_pauses_and_publish_final_dom() {
            let output = run("<body><script>\
                 globalThis.dom = __wildBuzzardDom;\
                 globalThis.bodyRoot = dom.lookup(3);\
                 globalThis.sectionRoot = dom.createElement('section');\
                 globalThis.textRoot = dom.createText('before');\
                 dom.append(sectionRoot, textRoot);\
                 dom.append(bodyRoot, sectionRoot);\
                 Promise.resolve().then(() => dom.setText(textRoot, 'after'));\
                 </script><p>between</p><script>\
                 if (globalThis.dom !== __wildBuzzardDom) throw 'realm reset';\
                 globalThis.paragraphRoot = dom.lookup(8);\
                 dom.setAttribute(paragraphRoot, 'data-seen', 'yes');\
                 dom.setAttribute(sectionRoot, 'data-phase', 'second');\
                 </script>")
            .unwrap();

            assert_eq!(output.retained_owner_count(), 1);
            assert_eq!(output.report().boundaries().len(), 2);
            assert_eq!(
                output.report().final_version(),
                output.current_version().unwrap()
            );
            let snapshot = output.snapshot().unwrap();
            let section = snapshot
                .nodes_in_document_order()
                .iter()
                .find(|node| {
                    matches!(&node.kind, NodeKind::Element(element) if element.name.local_name == "section")
                })
                .unwrap();
            let NodeKind::Element(section_data) = &section.kind else {
                unreachable!();
            };
            assert_eq!(section_data.html_attribute("data-phase"), Some("second"));
            assert!(section.children.iter().any(|child| {
                matches!(snapshot.node(*child).map(|node| &node.kind), Some(NodeKind::Text(data)) if data == "after")
            }));
            let paragraph = snapshot
                .nodes_in_document_order()
                .iter()
                .find(|node| {
                    matches!(&node.kind, NodeKind::Element(element) if element.name.local_name == "p")
                })
                .unwrap();
            let NodeKind::Element(paragraph) = &paragraph.kind else {
                unreachable!();
            };
            assert_eq!(paragraph.html_attribute("data-seen"), Some("yes"));
        }

        #[test]
        fn classification_is_exact_and_skipped_scripts_do_not_execute() {
            let output = run("<script>globalThis.count = 1;</script>\
                 <script type='  '>count += 1;</script>\
                 <script type='TEXT/JAVASCRIPT'>count += 1;</script>\
                 <script src=''>count = 100;</script>\
                 <script nomodule>count = 200;</script>\
                 <script type=module>count = 300;</script>\
                 <script type=application/javascript>count = 400;</script>\
                 <script>if (count !== 3) throw 'classification failure';</script>")
            .unwrap();
            let dispositions = output
                .report()
                .boundaries()
                .iter()
                .map(ScriptBoundaryEvidence::disposition)
                .collect::<Vec<_>>();
            assert_eq!(dispositions.len(), 8);
            assert!(matches!(dispositions[0], ScriptDisposition::Success(_)));
            assert!(matches!(dispositions[1], ScriptDisposition::Success(_)));
            assert!(matches!(dispositions[2], ScriptDisposition::Success(_)));
            assert_eq!(
                dispositions[3],
                &ScriptDisposition::Skipped(SkippedScriptReason::ExternalClassic)
            );
            assert_eq!(
                dispositions[4],
                &ScriptDisposition::Skipped(SkippedScriptReason::NoModule)
            );
            assert_eq!(
                dispositions[5],
                &ScriptDisposition::Skipped(SkippedScriptReason::Module)
            );
            assert_eq!(
                dispositions[6],
                &ScriptDisposition::Skipped(SkippedScriptReason::UnsupportedType)
            );
            assert!(matches!(dispositions[7], ScriptDisposition::Success(_)));
        }

        #[test]
        fn execution_classification_uses_the_start_tag_while_source_stays_live() {
            let mut parser = HtmlParser::default();
            let observed = std::cell::RefCell::new(Vec::new());
            let mut handler = |document: &mut Document, script: ParserInsertedScript| {
                let ordinal = observed.borrow().len();
                let text = document.children(script.node()).unwrap()[0];
                document
                    .set_character_data(text, format!("live-{ordinal}"))
                    .unwrap();
                match ordinal {
                    0 => {
                        document
                            .set_html_attribute(script.node(), "type", "text/javascript")
                            .unwrap();
                    }
                    1 => {
                        document
                            .remove_attribute(script.node(), None, "src")
                            .unwrap();
                    }
                    2 => {
                        document
                            .set_html_attribute(script.node(), "type", "module")
                            .unwrap();
                        document
                            .set_html_attribute(script.node(), "src", "late.js")
                            .unwrap();
                        document
                            .set_html_attribute(script.node(), "nomodule", "")
                            .unwrap();
                    }
                    _ => unreachable!(),
                }
                let snapshot = document.snapshot().unwrap();
                let frozen =
                    freeze_script_candidate(&snapshot, script.node(), script.start_tag()).unwrap();
                observed
                    .borrow_mut()
                    .push((frozen.source, frozen.classification));
                Ok::<(), &'static str>(())
            };
            parser
                .feed_with_script_handler(
                    "<script type=module>original-0</script>\
                     <script src=original.js>original-1</script>\
                     <script>original-2</script>",
                    &mut handler,
                )
                .unwrap();
            let _ = parser.finish_with_script_handler(&mut handler).unwrap();
            assert_eq!(
                *observed.borrow(),
                [
                    (
                        "live-0".to_owned(),
                        ScriptClassification::Skipped(SkippedScriptReason::Module)
                    ),
                    (
                        "live-1".to_owned(),
                        ScriptClassification::Skipped(SkippedScriptReason::ExternalClassic)
                    ),
                    ("live-2".to_owned(), ScriptClassification::AdmittedClassic),
                ]
            );
        }

        #[test]
        fn malformed_eof_script_is_retained_as_dom_text_but_never_executed() {
            let output = run("<script>globalThis.mustNotExecute = true;").unwrap();
            assert!(output.report().boundaries().is_empty());
            assert!(output.report().html_diagnostics() >= 1);
            let snapshot = output.snapshot().unwrap();
            let script = snapshot
                .nodes_in_document_order()
                .iter()
                .find(|node| {
                    matches!(
                        &node.kind,
                        NodeKind::Element(element) if element.name.local_name == "script"
                    )
                })
                .unwrap();
            let text = script
                .children
                .iter()
                .filter_map(|child| match &snapshot.node(*child)?.kind {
                    NodeKind::Text(data) => Some(data.as_str()),
                    _ => None,
                })
                .collect::<String>();
            assert_eq!(text, "globalThis.mustNotExecute = true;");
        }

        #[test]
        fn recoverable_primary_throw_runs_post_checkpoint_then_resumes_parser() {
            let output = run("<body><script>\
                 globalThis.dom = __wildBuzzardDom;\
                 globalThis.bodyRoot = dom.lookup(3);\
                 Promise.resolve().then(() => { globalThis.afterThrow = 'checkpoint'; });\
                 throw 17;\
                 </script><p>after throw</p><script>\
                 if (afterThrow !== 'checkpoint') throw 'post checkpoint missing';\
                 globalThis.paragraphRoot = dom.lookup(6);\
                 dom.setAttribute(paragraphRoot, 'data-resumed', 'yes');\
                 </script>")
            .unwrap();
            assert!(matches!(
                output.report().boundaries()[0].disposition(),
                ScriptDisposition::Thrown(ScriptValueSummary::Number(17.0))
            ));
            assert!(matches!(
                output.report().boundaries()[1].disposition(),
                ScriptDisposition::Success(_)
            ));
            let snapshot = output.snapshot().unwrap();
            let paragraph = snapshot
                .nodes_in_document_order()
                .iter()
                .find_map(|node| match &node.kind {
                    NodeKind::Element(element) if element.name.local_name == "p" => Some(element),
                    _ => None,
                })
                .unwrap();
            assert_eq!(paragraph.html_attribute("data-resumed"), Some("yes"));
        }

        #[test]
        fn cumulative_candidate_limit_is_terminal_and_no_owner_escapes() {
            let source = "<script></script>".repeat(MAX_DOCUMENT_SCRIPT_CANDIDATES + 1);
            let error = run(&source).unwrap_err();
            assert!(matches!(
                error,
                ScriptLoopError::Runtime(ClassicScriptOutcome::ResourceLimit(
                    brimstone_core::runtime::ResourceLimitKind::ScriptCandidates {
                        requested_total: 65,
                        limit: 64
                    }
                ))
            ));
        }

        #[test]
        fn parser_suspends_before_future_markup() {
            let output = run("<body><script>\
                 let sawFuture = true;\
                 try { __wildBuzzardDom.lookup(6); } catch (_) { sawFuture = false; }\
                 if (sawFuture) throw 'future markup became visible';\
                 </script><p>future</p><script>\
                 if (__wildBuzzardDom.lookup(6) === undefined) throw 'future node missing';\
                 </script>")
            .unwrap();
            assert_eq!(output.report().boundaries().len(), 2);
            assert!(
                output.report().boundaries().iter().all(|boundary| matches!(
                    boundary.disposition(),
                    ScriptDisposition::Success(_)
                ))
            );
        }

        #[test]
        fn paired_cancellation_interrupts_running_javascript() {
            let cancellation = ScriptLoopCancellationSource::new();
            let request = cancellation.clone();
            let canceller = thread::spawn(move || {
                thread::sleep(Duration::from_millis(20));
                request.cancel()
            });
            let result = parse_contained_numeric_loopback_document(
                "http://127.0.0.1:8080/cancel.html",
                "<script>while (true) {}</script>",
                TokenizerLimits::default(),
                ScriptMutationLimits::DEFAULT,
                &cancellation.token(),
                Instant::now() + Duration::from_secs(5),
            );
            assert!(canceller.join().unwrap());
            assert!(matches!(
                result,
                Err(
                    ScriptLoopError::Runtime(ClassicScriptOutcome::Interrupted(_))
                        | ScriptLoopError::Cancelled
                )
            ));
        }

        #[test]
        fn caller_absolute_deadline_is_not_rebased_after_context_setup() {
            let cancellation = ScriptLoopCancellationSource::new();
            let started = Instant::now();
            let deadline = started + Duration::from_millis(100);
            let result = parse_contained_numeric_loopback_document(
                "http://127.0.0.1:8080/deadline.html",
                "<script>while (true) {}</script>",
                TokenizerLimits::default(),
                ScriptMutationLimits::DEFAULT,
                &cancellation.token(),
                deadline,
            );
            assert!(matches!(
                result,
                Err(ScriptLoopError::Deadline
                    | ScriptLoopError::Runtime(ClassicScriptOutcome::Interrupted(_)))
            ));
            assert!(
                started.elapsed() < Duration::from_millis(160),
                "context setup must not be added back to the caller's absolute deadline"
            );
        }

        #[test]
        fn returned_control_terminal_has_one_typed_authoritative_classification() {
            assert_eq!(
                ScriptLoopError::Runtime(ClassicScriptOutcome::Interrupted(
                    InterruptReason::Deadline
                ))
                .control_failure(),
                Some(ScriptLoopControlFailure::Deadline)
            );
            assert_eq!(
                ScriptLoopError::Checkpoint(MicrotaskCheckpointOutcome::Interrupted(
                    InterruptReason::ExternalRequest
                ))
                .control_failure(),
                Some(ScriptLoopControlFailure::Cancelled)
            );
            assert_eq!(
                ScriptLoopError::HostDisposition(BrowserHostPhaseOutcome::Failed(
                    BrowserHostError::Cancelled
                ))
                .control_failure(),
                Some(ScriptLoopControlFailure::Cancelled)
            );
            assert_eq!(
                ScriptLoopError::Invariant("unrelated").control_failure(),
                None
            );
        }

        #[test]
        fn general_web_source_is_rejected_before_context_or_dom_creation() {
            let cancellation = ScriptLoopCancellationSource::new();
            assert!(matches!(
                parse_contained_numeric_loopback_document(
                    "https://www.youtube.com/",
                    "<script>globalThis.mustNotRun = true;</script>",
                    TokenizerLimits::default(),
                    ScriptMutationLimits::DEFAULT,
                    &cancellation.token(),
                    Instant::now() + Duration::from_secs(1),
                ),
                Err(ScriptLoopError::NonLoopbackSource)
            ));
        }
    }
}

#[cfg(feature = "contained_inline_classic")]
pub use contained::*;
