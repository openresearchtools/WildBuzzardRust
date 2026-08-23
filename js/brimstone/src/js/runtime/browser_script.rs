//! Bounded classic-script admission for a future browser embedding.
//!
//! This module deliberately exposes a much smaller surface than the legacy raw [`Context`]. A
//! caller borrows one exact [`OwnedContext`] through a lifetime-branded realm token, supplies only
//! host-owned UTF-8 metadata, and receives only copied scalar summaries. No moving-GC handle or raw
//! context token can cross the boundary.

use std::{
    fmt::{self, Display, Write},
    marker::PhantomData,
    panic::{AssertUnwindSafe, catch_unwind, panic_any, resume_unwind},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use crate::{
    common::wtf_8::Wtf8String,
    parser::{
        LocalizedParseError, LocalizedParseErrors, ParseContext, analyze::analyze, parse_script,
        source::Source,
    },
    runtime::{
        Context, Handle, OwnedContext, Value,
        bytecode::{
            generator::{BytecodeProgramGenerator, EmitError},
            instruction::OpCode,
        },
        eval_result::EvalError,
        gc::HandleScopeGuard,
    },
};

use super::browser_host::{
    BrowserHostClassicExecution, BrowserHostError, BrowserHostInstallError,
    BrowserHostMicrotaskExecution, BrowserHostPhaseOutcome, BrowserHostScopeGuard, BrowserHostTask,
    install_browser_host_bindings,
};

fn abort_host_or_process_abort(host: &mut impl BrowserHostTask) {
    if catch_unwind(AssertUnwindSafe(|| host.abort_phase())).is_err() {
        std::process::abort();
    }
}

fn abort_installed_host_or_process_abort(raw: Context) {
    if catch_unwind(AssertUnwindSafe(|| raw.abort_browser_host_phase())).is_err() {
        std::process::abort();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HostPhaseDispositionState {
    Armed,
    Finishing,
    FinishRejected,
    Aborting,
    Disposed,
}

/// One-way proof that an installed host phase receives exactly one successful irreversible
/// disposition. A rejected finish may still enter the documented abort fallback. Panic while an
/// irreversible callback is in flight is unprovable and terminates the process.
struct HostPhaseDisposition {
    state: HostPhaseDispositionState,
}

impl HostPhaseDisposition {
    fn armed() -> Self {
        Self { state: HostPhaseDispositionState::Armed }
    }

    fn begin_finish(&mut self) {
        if self.state != HostPhaseDispositionState::Armed {
            std::process::abort();
        }
        self.state = HostPhaseDispositionState::Finishing;
    }

    fn finish_returned(&mut self, accepted: bool) {
        if self.state != HostPhaseDispositionState::Finishing {
            std::process::abort();
        }
        self.state = if accepted {
            HostPhaseDispositionState::Disposed
        } else {
            HostPhaseDispositionState::FinishRejected
        };
    }

    fn begin_abort(&mut self) {
        if !matches!(
            self.state,
            HostPhaseDispositionState::Armed | HostPhaseDispositionState::FinishRejected
        ) {
            std::process::abort();
        }
        self.state = HostPhaseDispositionState::Aborting;
    }

    fn abort_returned(&mut self) {
        if self.state != HostPhaseDispositionState::Aborting {
            std::process::abort();
        }
        self.state = HostPhaseDispositionState::Disposed;
    }
}

fn finish_installed_host_once(
    raw: Context,
    disposition: &mut HostPhaseDisposition,
) -> Result<super::browser_host::BrowserHostCommitOutcome, BrowserHostError> {
    disposition.begin_finish();
    let result = raw.finish_browser_host_phase();
    disposition.finish_returned(result.is_ok());
    result
}

fn discard_installed_host_once(raw: Context, disposition: &mut HostPhaseDisposition) {
    disposition.begin_abort();
    abort_installed_host_or_process_abort(raw);
    disposition.abort_returned();
}

fn discard_direct_host_once(
    host: &mut impl BrowserHostTask,
    disposition: &mut HostPhaseDisposition,
) {
    disposition.begin_abort();
    abort_host_or_process_abort(host);
    disposition.abort_returned();
}

fn retire_direct_host_after_unwind(
    host: &mut impl BrowserHostTask,
    disposition: &mut HostPhaseDisposition,
) {
    match disposition.state {
        HostPhaseDispositionState::Armed | HostPhaseDispositionState::FinishRejected => {
            discard_direct_host_once(host, disposition);
        }
        HostPhaseDispositionState::Finishing
        | HostPhaseDispositionState::Aborting
        | HostPhaseDispositionState::Disposed => std::process::abort(),
    }
}

fn complete_direct_host_after_normal_return(
    host: &mut impl BrowserHostTask,
    disposition: &mut HostPhaseDisposition,
) {
    match disposition.state {
        // Host-scope installation rejected before erased authority existed. The phase was already
        // admitted, so retire the caller's direct authority after the temporary borrow is gone.
        HostPhaseDispositionState::Armed => discard_direct_host_once(host, disposition),
        HostPhaseDispositionState::Disposed => {}
        HostPhaseDispositionState::Finishing
        | HostPhaseDispositionState::FinishRejected
        | HostPhaseDispositionState::Aborting => std::process::abort(),
    }
}

fn complete_installed_host_after_normal_return(
    raw: Context,
    disposition: &mut HostPhaseDisposition,
) {
    match disposition.state {
        HostPhaseDispositionState::Armed | HostPhaseDispositionState::FinishRejected => {
            discard_installed_host_once(raw, disposition);
        }
        HostPhaseDispositionState::Disposed => {}
        HostPhaseDispositionState::Finishing | HostPhaseDispositionState::Aborting => {
            std::process::abort();
        }
    }
}

fn retire_installed_host_after_unwind(raw: Context, disposition: &mut HostPhaseDisposition) {
    match disposition.state {
        HostPhaseDispositionState::Armed | HostPhaseDispositionState::FinishRejected => {
            discard_installed_host_once(raw, disposition);
        }
        HostPhaseDispositionState::Finishing
        | HostPhaseDispositionState::Aborting
        | HostPhaseDispositionState::Disposed => std::process::abort(),
    }
}

/// Hard browser-admission source cap. Larger network resources must be rejected or streamed by a
/// future loader before entering this synchronous runtime boundary.
pub const MAX_CLASSIC_SCRIPT_SOURCE_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_CLASSIC_SCRIPT_FILENAME_BYTES: usize = 4 * 1024;
pub const MAX_CLASSIC_SCRIPT_BASE_BYTES: usize = 8 * 1024;

const MAX_DIAGNOSTIC_BYTES: usize = 16 * 1024;
const MAX_DIAGNOSTICS: usize = 8;
const HARD_MAX_OPCODES: u64 = 100_000_000;
const HARD_MAX_MANAGED_ALLOCATION_BYTES: usize = 256 * 1024 * 1024;
const HARD_MAX_RECURSION_DEPTH: usize = 512;
const HARD_MAX_JOBS: u64 = 1_000_000;
const HARD_MAX_WALL_TIME: Duration = Duration::from_secs(30);

const MAX_DOCUMENT_SCRIPT_CANDIDATES: u32 = 64;
const MAX_DOCUMENT_SOURCE_BYTES: usize = 4 * 1024 * 1024;
const MAX_DOCUMENT_OPCODES: u64 = 10_000_000;
const MAX_DOCUMENT_MANAGED_ALLOCATION_BYTES: usize = 64 * 1024 * 1024;
const MAX_DOCUMENT_RECURSION_DEPTH: usize = 256;
const MAX_DOCUMENT_JOBS: u64 = 10_000;
const MAX_DOCUMENT_DIAGNOSTICS: usize = 128;

// Overflow itself is reported as a policy-limit request. Keep the arithmetic checked rather than
// allowing a wrapped cumulative counter; the maximum sentinel is only copied into diagnostics.
fn checked_add_usize_or_max(left: usize, right: usize) -> usize {
    let Some(total) = left.checked_add(right) else {
        return usize::MAX;
    };
    total
}

fn checked_add_u64_or_max(left: u64, right: u64) -> u64 {
    let Some(total) = left.checked_add(right) else {
        return u64::MAX;
    };
    total
}

fn checked_add_u32_or_max(left: u32, right: u32) -> u32 {
    let Some(total) = left.checked_add(right) else {
        return u32::MAX;
    };
    total
}

/// Borrowed classic-script bytes and source identity supplied by a browser loader.
///
/// `base_url` is currently retained only as validated provenance metadata. Resolving relative URLs
/// requires the future DOM/URL loader contract and is intentionally not guessed here.
#[derive(Clone, Copy, Debug)]
pub struct ClassicScriptRequest<'source> {
    source: &'source str,
    filename: &'source str,
    base_url: Option<&'source str>,
}

impl<'source> ClassicScriptRequest<'source> {
    pub fn new(source: &'source str, filename: &'source str) -> Self {
        Self { source, filename, base_url: None }
    }

    pub fn with_base_url(mut self, base_url: &'source str) -> Self {
        self.base_url = Some(base_url);
        self
    }

    pub fn source(&self) -> &'source str {
        self.source
    }

    pub fn filename(&self) -> &'source str {
        self.filename
    }

    pub fn base_url(&self) -> Option<&'source str> {
        self.base_url
    }
}

/// Validated per-evaluation limits. All fields are capped by non-configurable defense-in-depth
/// maxima so an embedding cannot accidentally turn this into an unbounded entry point.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClassicScriptLimits {
    max_opcodes: u64,
    max_managed_allocation_bytes: usize,
    max_recursion_depth: usize,
    max_jobs: u64,
    wall_time: Duration,
}

impl ClassicScriptLimits {
    pub fn new(
        max_opcodes: u64,
        max_managed_allocation_bytes: usize,
        max_recursion_depth: usize,
        max_jobs: u64,
        wall_time: Duration,
    ) -> Result<Self, LimitConfigurationError> {
        validate_nonzero_limit("max_opcodes", max_opcodes as u128, HARD_MAX_OPCODES as u128)?;
        validate_nonzero_limit(
            "max_managed_allocation_bytes",
            max_managed_allocation_bytes as u128,
            HARD_MAX_MANAGED_ALLOCATION_BYTES as u128,
        )?;
        validate_nonzero_limit(
            "max_recursion_depth",
            max_recursion_depth as u128,
            HARD_MAX_RECURSION_DEPTH as u128,
        )?;
        validate_nonzero_limit("max_jobs", max_jobs as u128, HARD_MAX_JOBS as u128)?;
        if wall_time > HARD_MAX_WALL_TIME {
            return Err(LimitConfigurationError::ExceedsHardMaximum {
                field: "wall_time",
                hard_maximum: HARD_MAX_WALL_TIME.as_nanos(),
            });
        }

        Ok(Self {
            max_opcodes,
            max_managed_allocation_bytes,
            max_recursion_depth,
            max_jobs,
            wall_time,
        })
    }

    pub fn max_opcodes(&self) -> u64 {
        self.max_opcodes
    }

    pub fn max_managed_allocation_bytes(&self) -> usize {
        self.max_managed_allocation_bytes
    }

    pub fn max_recursion_depth(&self) -> usize {
        self.max_recursion_depth
    }

    pub fn max_jobs(&self) -> u64 {
        self.max_jobs
    }

    pub fn wall_time(&self) -> Duration {
        self.wall_time
    }

    /// Construct the fixed W9 parser-blocking document budget. The returned opcode, managed
    /// allocation, and job limits are cumulative when installed through
    /// [`BrowserScriptRealm::with_document_script_budget`]; recursion remains a per-activation
    /// peak and `wall_time` becomes one absolute document deadline.
    pub fn parser_blocking_document(wall_time: Duration) -> Result<Self, LimitConfigurationError> {
        Self::new(
            MAX_DOCUMENT_OPCODES,
            MAX_DOCUMENT_MANAGED_ALLOCATION_BYTES,
            MAX_DOCUMENT_RECURSION_DEPTH,
            MAX_DOCUMENT_JOBS,
            wall_time,
        )
    }
}

impl Default for ClassicScriptLimits {
    fn default() -> Self {
        Self {
            max_opcodes: 50_000_000,
            max_managed_allocation_bytes: 128 * 1024 * 1024,
            max_recursion_depth: 256,
            max_jobs: 100_000,
            wall_time: Duration::from_secs(10),
        }
    }
}

fn validate_nonzero_limit(
    field: &'static str,
    value: u128,
    hard_maximum: u128,
) -> Result<(), LimitConfigurationError> {
    if value == 0 {
        return Err(LimitConfigurationError::Zero { field });
    }
    if value > hard_maximum {
        return Err(LimitConfigurationError::ExceedsHardMaximum { field, hard_maximum });
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LimitConfigurationError {
    Zero { field: &'static str },
    ExceedsHardMaximum { field: &'static str, hard_maximum: u128 },
    DocumentBudgetAlreadyActive,
}

/// Thread-safe one-way cancellation flag. Execution remains owner-thread-only; another thread may
/// only request that the owner stop at the next audited poll point.
#[derive(Clone, Debug, Default)]
pub struct ScriptInterruptHandle {
    requested: Arc<AtomicBool>,
}

impl ScriptInterruptHandle {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn request_interrupt(&self) {
        self.requested.store(true, Ordering::Release);
    }

    pub fn is_interrupt_requested(&self) -> bool {
        self.requested.load(Ordering::Acquire)
    }
}

fn validate_document_limits(limits: ClassicScriptLimits) -> Result<(), LimitConfigurationError> {
    for (field, value, hard_maximum) in [
        ("document_max_opcodes", limits.max_opcodes as u128, MAX_DOCUMENT_OPCODES as u128),
        (
            "document_max_managed_allocation_bytes",
            limits.max_managed_allocation_bytes as u128,
            MAX_DOCUMENT_MANAGED_ALLOCATION_BYTES as u128,
        ),
        (
            "document_max_recursion_depth",
            limits.max_recursion_depth as u128,
            MAX_DOCUMENT_RECURSION_DEPTH as u128,
        ),
        ("document_max_jobs", limits.max_jobs as u128, MAX_DOCUMENT_JOBS as u128),
    ] {
        if value > hard_maximum {
            return Err(LimitConfigurationError::ExceedsHardMaximum { field, hard_maximum });
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct DocumentScriptCaps {
    script_candidates: u32,
    source_bytes: usize,
    diagnostics: usize,
}

impl DocumentScriptCaps {
    const W9_PARSER_BLOCKING: Self = Self {
        script_candidates: MAX_DOCUMENT_SCRIPT_CANDIDATES,
        source_bytes: MAX_DOCUMENT_SOURCE_BYTES,
        diagnostics: MAX_DOCUMENT_DIAGNOSTICS,
    };
}

#[derive(Clone)]
struct AdmissionControl {
    interrupt: Arc<AtomicBool>,
    absolute_deadline: Option<Instant>,
}

#[derive(Clone, Copy)]
enum DiagnosticPolicy {
    PerAdmission,
    Document { used: usize, limit: usize },
}

struct PhaseAdmission {
    limits: ClassicScriptLimits,
    control: AdmissionControl,
    diagnostics: DiagnosticPolicy,
}

impl PhaseAdmission {
    fn relative(limits: ClassicScriptLimits, interrupt: Arc<AtomicBool>) -> Self {
        Self {
            limits,
            control: AdmissionControl { interrupt, absolute_deadline: None },
            diagnostics: DiagnosticPolicy::PerAdmission,
        }
    }
}

#[derive(Clone, Copy)]
enum DocumentTerminal {
    Interrupted(InterruptReason),
    Resource(ResourceLimitKind),
    Host(BrowserHostError),
    JobThrown(ScriptValueSummary),
    InvalidMetadata(InvalidMetadata),
    RuntimePoisoned,
    EnginePanic,
}

impl DocumentTerminal {
    fn classic_outcome(self) -> ClassicScriptOutcome {
        match self {
            Self::Interrupted(reason) => ClassicScriptOutcome::Interrupted(reason),
            Self::Resource(limit) => ClassicScriptOutcome::ResourceLimit(limit),
            Self::Host(error) => ClassicScriptOutcome::HostFailure(error),
            Self::JobThrown(value) => ClassicScriptOutcome::JobThrown(value),
            Self::InvalidMetadata(error) => ClassicScriptOutcome::InvalidMetadata(error),
            Self::RuntimePoisoned => ClassicScriptOutcome::RuntimePoisoned,
            Self::EnginePanic => ClassicScriptOutcome::EnginePanic,
        }
    }

    fn checkpoint_outcome(self) -> MicrotaskCheckpointOutcome {
        match self {
            Self::Interrupted(reason) => MicrotaskCheckpointOutcome::Interrupted(reason),
            Self::Resource(limit) => MicrotaskCheckpointOutcome::ResourceLimit(limit),
            Self::Host(error) => MicrotaskCheckpointOutcome::HostFailure(error),
            Self::JobThrown(value) => MicrotaskCheckpointOutcome::JobThrown(value),
            Self::InvalidMetadata(error) => MicrotaskCheckpointOutcome::InvalidMetadata(error),
            Self::RuntimePoisoned => MicrotaskCheckpointOutcome::RuntimePoisoned,
            Self::EnginePanic => MicrotaskCheckpointOutcome::EnginePanic,
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum DocumentHostMode {
    None,
    Installed,
}

struct DocumentScriptBudgetState {
    limits: ClassicScriptLimits,
    caps: DocumentScriptCaps,
    interrupt: Arc<AtomicBool>,
    deadline: Instant,
    script_candidates: u32,
    source_bytes: usize,
    opcodes_executed: u64,
    managed_allocation_bytes: usize,
    jobs_executed: u64,
    diagnostics_emitted: usize,
    terminal: Option<DocumentTerminal>,
    host_mode: DocumentHostMode,
}

impl DocumentScriptBudgetState {
    fn new(
        limits: ClassicScriptLimits,
        interrupt: Arc<AtomicBool>,
        caller_absolute_deadline: Option<Instant>,
        caps: DocumentScriptCaps,
        host_mode: DocumentHostMode,
    ) -> Self {
        let started = Instant::now();
        let relative_deadline = started.checked_add(limits.wall_time).unwrap_or(started);
        let deadline = caller_absolute_deadline
            .map_or(relative_deadline, |deadline| deadline.min(relative_deadline));
        Self {
            limits,
            caps,
            interrupt,
            deadline,
            script_candidates: 0,
            source_bytes: 0,
            opcodes_executed: 0,
            managed_allocation_bytes: 0,
            jobs_executed: 0,
            diagnostics_emitted: 0,
            terminal: None,
            host_mode,
        }
    }

    fn terminal_classic_outcome(&self) -> Option<ClassicScriptOutcome> {
        self.terminal.map(DocumentTerminal::classic_outcome)
    }

    fn terminal_checkpoint_outcome(&self) -> Option<MicrotaskCheckpointOutcome> {
        self.terminal.map(DocumentTerminal::checkpoint_outcome)
    }

    fn pending_task_cap(&self) -> usize {
        usize::try_from(self.limits.max_jobs).unwrap_or_else(|_| std::process::abort())
    }

    fn latch(&mut self, terminal: DocumentTerminal) {
        if self.terminal.is_none() {
            self.terminal = Some(terminal);
        }
    }

    fn check_control(&mut self) -> Result<(), ClassicScriptOutcome> {
        if let Some(outcome) = self.terminal_classic_outcome() {
            return Err(outcome);
        }
        let terminal = if self.interrupt.load(Ordering::Acquire) {
            Some(DocumentTerminal::Interrupted(InterruptReason::ExternalRequest))
        } else if Instant::now() >= self.deadline {
            Some(DocumentTerminal::Interrupted(InterruptReason::Deadline))
        } else {
            None
        };
        if let Some(terminal) = terminal {
            self.latch(terminal);
            return Err(terminal.classic_outcome());
        }
        Ok(())
    }

    /// Observe cancellation/deadline state without interrupting bounded queue retirement. The
    /// first terminal reason remains authoritative, but cleanup always continues to empty.
    fn observe_cleanup_control(&mut self) {
        if self.terminal.is_some() {
            return;
        }
        if self.interrupt.load(Ordering::Acquire) {
            self.latch(DocumentTerminal::Interrupted(InterruptReason::ExternalRequest));
        } else if Instant::now() >= self.deadline {
            self.latch(DocumentTerminal::Interrupted(InterruptReason::Deadline));
        }
    }

    fn latch_pending_task_overflow(&mut self, limit: usize) {
        let limit = u64::try_from(limit).unwrap_or_else(|_| std::process::abort());
        self.latch(DocumentTerminal::Resource(ResourceLimitKind::PendingJobs { limit }));
    }

    fn account_candidate(&mut self, source_bytes: usize) -> Result<(), ClassicScriptOutcome> {
        self.check_control()?;

        let requested_candidates = checked_add_u32_or_max(self.script_candidates, 1);
        if requested_candidates > self.caps.script_candidates {
            let limit = ResourceLimitKind::ScriptCandidates {
                requested_total: requested_candidates,
                limit: self.caps.script_candidates,
            };
            self.latch(DocumentTerminal::Resource(limit));
            return Err(ClassicScriptOutcome::ResourceLimit(limit));
        }
        self.script_candidates = requested_candidates;

        let requested_source = checked_add_usize_or_max(self.source_bytes, source_bytes);
        if requested_source > self.caps.source_bytes {
            let limit = ResourceLimitKind::SourceBytes {
                actual: requested_source,
                limit: self.caps.source_bytes,
            };
            self.latch(DocumentTerminal::Resource(limit));
            return Err(ClassicScriptOutcome::ResourceLimit(limit));
        }
        self.source_bytes = requested_source;
        Ok(())
    }

    fn phase_admission(&self) -> PhaseAdmission {
        let remaining_wall_time = self.deadline.saturating_duration_since(Instant::now());
        PhaseAdmission {
            limits: ClassicScriptLimits {
                max_opcodes: self
                    .limits
                    .max_opcodes
                    .saturating_sub(self.opcodes_executed),
                max_managed_allocation_bytes: self
                    .limits
                    .max_managed_allocation_bytes
                    .saturating_sub(self.managed_allocation_bytes),
                max_recursion_depth: self.limits.max_recursion_depth,
                max_jobs: self.limits.max_jobs.saturating_sub(self.jobs_executed),
                wall_time: remaining_wall_time,
            },
            control: AdmissionControl {
                interrupt: self.interrupt.clone(),
                absolute_deadline: Some(self.deadline),
            },
            diagnostics: DiagnosticPolicy::Document {
                used: self.diagnostics_emitted,
                limit: self.caps.diagnostics,
            },
        }
    }

    fn finish_classic_phase(&mut self, execution: &mut ClassicScriptExecution) {
        let before_allocations = self.managed_allocation_bytes;
        let outcome = std::mem::replace(&mut execution.outcome, ClassicScriptOutcome::RuntimeBusy);
        execution.outcome = self.normalize_classic_outcome(outcome, before_allocations);
        if let Some(limit) = self.account_report(&execution.report) {
            execution.outcome = ClassicScriptOutcome::ResourceLimit(limit);
        }
        if let Some(limit) = self.account_diagnostics(&execution.outcome) {
            execution.outcome = ClassicScriptOutcome::ResourceLimit(limit);
        }
        self.latch_classic_outcome(&execution.outcome);
    }

    fn finish_checkpoint_phase(&mut self, execution: &mut MicrotaskCheckpointExecution) {
        let before_allocations = self.managed_allocation_bytes;
        let outcome =
            std::mem::replace(&mut execution.outcome, MicrotaskCheckpointOutcome::RuntimeBusy);
        execution.outcome = self.normalize_checkpoint_outcome(outcome, before_allocations);
        if let Some(limit) = self.account_report(&execution.report) {
            execution.outcome = MicrotaskCheckpointOutcome::ResourceLimit(limit);
        }
        self.latch_checkpoint_outcome(&execution.outcome);
    }

    fn normalize_classic_outcome(
        &self,
        outcome: ClassicScriptOutcome,
        before_allocations: usize,
    ) -> ClassicScriptOutcome {
        match outcome {
            ClassicScriptOutcome::ResourceLimit(ResourceLimitKind::Opcodes { .. }) => {
                ClassicScriptOutcome::ResourceLimit(ResourceLimitKind::Opcodes {
                    limit: self.limits.max_opcodes,
                })
            }
            ClassicScriptOutcome::ResourceLimit(ResourceLimitKind::ManagedAllocationBytes {
                requested_total,
                ..
            }) => ClassicScriptOutcome::ResourceLimit(ResourceLimitKind::ManagedAllocationBytes {
                requested_total: checked_add_usize_or_max(before_allocations, requested_total),
                limit: self.limits.max_managed_allocation_bytes,
            }),
            ClassicScriptOutcome::ResourceLimit(ResourceLimitKind::Jobs { .. }) => {
                ClassicScriptOutcome::ResourceLimit(ResourceLimitKind::Jobs {
                    limit: self.limits.max_jobs,
                })
            }
            outcome => outcome,
        }
    }

    fn normalize_checkpoint_outcome(
        &self,
        outcome: MicrotaskCheckpointOutcome,
        before_allocations: usize,
    ) -> MicrotaskCheckpointOutcome {
        match outcome {
            MicrotaskCheckpointOutcome::ResourceLimit(ResourceLimitKind::Opcodes { .. }) => {
                MicrotaskCheckpointOutcome::ResourceLimit(ResourceLimitKind::Opcodes {
                    limit: self.limits.max_opcodes,
                })
            }
            MicrotaskCheckpointOutcome::ResourceLimit(
                ResourceLimitKind::ManagedAllocationBytes { requested_total, .. },
            ) => MicrotaskCheckpointOutcome::ResourceLimit(
                ResourceLimitKind::ManagedAllocationBytes {
                    requested_total: checked_add_usize_or_max(before_allocations, requested_total),
                    limit: self.limits.max_managed_allocation_bytes,
                },
            ),
            MicrotaskCheckpointOutcome::ResourceLimit(ResourceLimitKind::Jobs { .. }) => {
                MicrotaskCheckpointOutcome::ResourceLimit(ResourceLimitKind::Jobs {
                    limit: self.limits.max_jobs,
                })
            }
            outcome => outcome,
        }
    }

    fn account_report(&mut self, report: &ClassicScriptReport) -> Option<ResourceLimitKind> {
        let next_opcodes = checked_add_u64_or_max(self.opcodes_executed, report.opcodes_executed);
        if next_opcodes > self.limits.max_opcodes {
            return Some(ResourceLimitKind::Opcodes { limit: self.limits.max_opcodes });
        }
        self.opcodes_executed = next_opcodes;

        let next_allocations = checked_add_usize_or_max(
            self.managed_allocation_bytes,
            report.managed_allocation_bytes,
        );
        if next_allocations > self.limits.max_managed_allocation_bytes {
            return Some(ResourceLimitKind::ManagedAllocationBytes {
                requested_total: next_allocations,
                limit: self.limits.max_managed_allocation_bytes,
            });
        }
        self.managed_allocation_bytes = next_allocations;

        let next_jobs = checked_add_u64_or_max(self.jobs_executed, report.jobs_executed);
        if next_jobs > self.limits.max_jobs {
            return Some(ResourceLimitKind::Jobs { limit: self.limits.max_jobs });
        }
        self.jobs_executed = next_jobs;
        None
    }

    fn account_diagnostics(&mut self, outcome: &ClassicScriptOutcome) -> Option<ResourceLimitKind> {
        let emitted = match outcome {
            ClassicScriptOutcome::ParseError(diagnostics)
            | ClassicScriptOutcome::AnalyzeError(diagnostics) => diagnostics.len(),
            ClassicScriptOutcome::CompileError(_) => 1,
            _ => 0,
        };
        let requested_total = checked_add_usize_or_max(self.diagnostics_emitted, emitted);
        if requested_total > self.caps.diagnostics {
            return Some(ResourceLimitKind::Diagnostics {
                requested_total,
                limit: self.caps.diagnostics,
            });
        }
        self.diagnostics_emitted = requested_total;
        None
    }

    fn latch_classic_outcome(&mut self, outcome: &ClassicScriptOutcome) {
        let terminal = match outcome {
            ClassicScriptOutcome::Success(_)
            | ClassicScriptOutcome::Thrown(_)
            | ClassicScriptOutcome::ParseError(_)
            | ClassicScriptOutcome::AnalyzeError(_)
            | ClassicScriptOutcome::CompileError(_)
            | ClassicScriptOutcome::InvalidDocumentSession(_)
            | ClassicScriptOutcome::PendingJobsAtDocumentStart { .. }
            | ClassicScriptOutcome::PendingJobsAtDocumentExit { .. }
            | ClassicScriptOutcome::RuntimeBusy => None,
            ClassicScriptOutcome::JobThrown(value) => Some(DocumentTerminal::JobThrown(*value)),
            ClassicScriptOutcome::Interrupted(reason) => {
                Some(DocumentTerminal::Interrupted(*reason))
            }
            ClassicScriptOutcome::ResourceLimit(limit) => Some(DocumentTerminal::Resource(*limit)),
            ClassicScriptOutcome::HostFailure(error) => Some(DocumentTerminal::Host(*error)),
            ClassicScriptOutcome::InvalidMetadata(error) => {
                Some(DocumentTerminal::InvalidMetadata(*error))
            }
            ClassicScriptOutcome::RuntimePoisoned => Some(DocumentTerminal::RuntimePoisoned),
            ClassicScriptOutcome::EnginePanic => Some(DocumentTerminal::EnginePanic),
        };
        if let Some(terminal) = terminal {
            self.latch(terminal);
        }
    }

    fn latch_checkpoint_outcome(&mut self, outcome: &MicrotaskCheckpointOutcome) {
        let terminal = match outcome {
            MicrotaskCheckpointOutcome::Complete => None,
            MicrotaskCheckpointOutcome::JobThrown(value) => {
                Some(DocumentTerminal::JobThrown(*value))
            }
            MicrotaskCheckpointOutcome::Interrupted(reason) => {
                Some(DocumentTerminal::Interrupted(*reason))
            }
            MicrotaskCheckpointOutcome::ResourceLimit(limit) => {
                Some(DocumentTerminal::Resource(*limit))
            }
            MicrotaskCheckpointOutcome::HostFailure(error) => Some(DocumentTerminal::Host(*error)),
            MicrotaskCheckpointOutcome::InvalidMetadata(error) => {
                Some(DocumentTerminal::InvalidMetadata(*error))
            }
            MicrotaskCheckpointOutcome::RuntimeBusy => None,
            MicrotaskCheckpointOutcome::RuntimePoisoned => Some(DocumentTerminal::RuntimePoisoned),
            MicrotaskCheckpointOutcome::EnginePanic => Some(DocumentTerminal::EnginePanic),
        };
        if let Some(terminal) = terminal {
            self.latch(terminal);
        }
    }
}

/// A lifetime-branded authority for classic scripts in one exact caller-owned context and its
/// initial realm. The higher-ranked constructor on [`OwnedContext`] prevents this token from being
/// retained after the callback.
pub struct BrowserScriptRealm<'realm> {
    raw: Context,
    document_budget: Option<DocumentScriptBudgetState>,
    _brand: PhantomData<&'realm mut OwnedContext>,
}

impl<'realm> BrowserScriptRealm<'realm> {
    pub(crate) fn new(raw: Context) -> Self {
        Self { raw, document_budget: None, _brand: PhantomData }
    }

    /// Run a host-free parser-blocking document task under one sealed cumulative budget.
    ///
    /// The callback keeps the same owner-thread context and initial realm for every phase. Entry
    /// rejects and retires any older queued work. Normal return succeeds only with an empty task
    /// queue; otherwise the queue is synchronously cleared and a typed error is returned. A panic
    /// in the callback clears queued work when the context is inspectable, permanently poisons
    /// browser admission, and resumes the original unwind.
    pub fn with_document_script_budget<R>(
        &mut self,
        limits: ClassicScriptLimits,
        interrupt: &ScriptInterruptHandle,
        f: impl FnOnce(&mut Self) -> R,
    ) -> Result<R, ClassicScriptOutcome> {
        self.with_document_script_budget_caps(
            limits,
            interrupt,
            DocumentScriptCaps::W9_PARSER_BLOCKING,
            f,
        )
    }

    /// Run a hosted parser-blocking document task while borrowing one exact host authority for
    /// the entire callback.
    ///
    /// Hosted document phase methods take no host argument: queued jobs can therefore be drained
    /// only while the original `host` borrow remains installed. Attempts to use legacy per-phase
    /// host entry while this session is active return nonterminal `RuntimeBusy` without replacing
    /// or latching the document authority.
    ///
    /// ```compile_fail
    /// use brimstone_core::runtime::{
    ///     BrowserHostTask, BrowserScriptRealm, ClassicScriptLimits, ScriptInterruptHandle,
    /// };
    ///
    /// fn cannot_replace_host<H: BrowserHostTask>(
    ///     realm: &mut BrowserScriptRealm<'_>,
    ///     host_a: &mut H,
    ///     host_b: &mut H,
    ///     limits: ClassicScriptLimits,
    ///     interrupt: &ScriptInterruptHandle,
    /// ) {
    ///     let _ = realm.with_hosted_document_script_budget(
    ///         host_a,
    ///         limits,
    ///         interrupt,
    ///         |document| {
    ///             document.perform_hosted_document_microtask_checkpoint(host_b);
    ///         },
    ///     );
    /// }
    /// ```
    pub fn with_hosted_document_script_budget<H: BrowserHostTask, R>(
        &mut self,
        host: &mut H,
        limits: ClassicScriptLimits,
        interrupt: &ScriptInterruptHandle,
        f: impl FnOnce(&mut Self) -> R,
    ) -> Result<R, ClassicScriptOutcome> {
        self.with_hosted_document_script_budget_controlled(host, limits, interrupt, None, f)
    }

    /// Run a hosted parser-blocking document task until the earlier of the relative hard limit or
    /// an exact caller-owned absolute deadline.
    ///
    /// The caller should derive `caller_absolute_deadline` before any setup whose elapsed time
    /// belongs to the document operation. Brimstone preserves that authority without rebasing it
    /// when this method is entered. The ordinary relative wall-time hard limit remains active and
    /// wins when it expires first.
    pub fn with_hosted_document_script_budget_until<H: BrowserHostTask, R>(
        &mut self,
        host: &mut H,
        limits: ClassicScriptLimits,
        interrupt: &ScriptInterruptHandle,
        caller_absolute_deadline: Instant,
        f: impl FnOnce(&mut Self) -> R,
    ) -> Result<R, ClassicScriptOutcome> {
        self.with_hosted_document_script_budget_controlled(
            host,
            limits,
            interrupt,
            Some(caller_absolute_deadline),
            f,
        )
    }

    fn with_hosted_document_script_budget_controlled<H: BrowserHostTask, R>(
        &mut self,
        host: &mut H,
        limits: ClassicScriptLimits,
        interrupt: &ScriptInterruptHandle,
        caller_absolute_deadline: Option<Instant>,
        f: impl FnOnce(&mut Self) -> R,
    ) -> Result<R, ClassicScriptOutcome> {
        self.begin_document_script_budget(
            limits,
            interrupt,
            caller_absolute_deadline,
            DocumentScriptCaps::W9_PARSER_BLOCKING,
            DocumentHostMode::Installed,
        )?;

        let host_scope = match BrowserHostScopeGuard::install(self.raw, host) {
            Ok(scope) => scope,
            Err(error) => {
                let Some(mut budget) = self.document_budget.take() else {
                    self.raw.poison_browser_script();
                    return Err(ClassicScriptOutcome::EnginePanic);
                };
                let _retirement = self.retire_active_document_tasks(&mut budget);
                return Err(budget
                    .terminal
                    .map(DocumentTerminal::classic_outcome)
                    .unwrap_or(ClassicScriptOutcome::HostFailure(error)));
            }
        };
        // Host-scope installation is part of the caller's operation budget.
        // Recheck after that setup and immediately before callback admission so
        // an elapsed absolute deadline cannot authorize host-visible effects.
        let callback_admission = self
            .document_budget
            .as_mut()
            .map_or(Err(ClassicScriptOutcome::EnginePanic), |budget| {
                budget.check_control()
            });
        if let Err(outcome) = callback_admission {
            let completion = self.finish_document_script_callback();
            drop(host_scope);
            return Err(match completion {
                Ok(()) => outcome,
                Err(authoritative) => authoritative,
            });
        }
        let result = catch_unwind(AssertUnwindSafe(|| f(self)));
        let completion = match result {
            Ok(value) => self.finish_document_script_callback().map(|()| value),
            Err(payload) => {
                self.abort_document_script_callback_for_unwind(DocumentHostMode::Installed);
                drop(host_scope);
                resume_unwind(payload)
            }
        };
        drop(host_scope);
        completion
    }

    fn with_document_script_budget_caps<R>(
        &mut self,
        limits: ClassicScriptLimits,
        interrupt: &ScriptInterruptHandle,
        caps: DocumentScriptCaps,
        f: impl FnOnce(&mut Self) -> R,
    ) -> Result<R, ClassicScriptOutcome> {
        self.begin_document_script_budget(limits, interrupt, None, caps, DocumentHostMode::None)?;

        let result = catch_unwind(AssertUnwindSafe(|| f(self)));
        match result {
            Ok(value) => self.finish_document_script_callback().map(|()| value),
            Err(payload) => {
                self.abort_document_script_callback_for_unwind(DocumentHostMode::None);
                resume_unwind(payload)
            }
        }
    }

    fn begin_document_script_budget(
        &mut self,
        limits: ClassicScriptLimits,
        interrupt: &ScriptInterruptHandle,
        caller_absolute_deadline: Option<Instant>,
        caps: DocumentScriptCaps,
        host_mode: DocumentHostMode,
    ) -> Result<(), ClassicScriptOutcome> {
        if self.document_budget.is_some() {
            return Err(ClassicScriptOutcome::InvalidDocumentSession(
                LimitConfigurationError::DocumentBudgetAlreadyActive,
            ));
        }
        if self.raw.browser_script_is_poisoned() {
            return Err(ClassicScriptOutcome::RuntimePoisoned);
        }
        validate_document_limits(limits).map_err(ClassicScriptOutcome::InvalidDocumentSession)?;
        if !self.raw.browser_script_document_session_is_idle(false) {
            return Err(ClassicScriptOutcome::RuntimeBusy);
        }

        let mut budget = DocumentScriptBudgetState::new(
            limits,
            interrupt.requested.clone(),
            caller_absolute_deadline,
            caps,
            host_mode,
        );
        let pending_jobs = self.raw.browser_script_pending_job_count();
        if pending_jobs != 0 {
            let Some(retired_jobs) = self
                .raw
                .retire_foreign_browser_tasks_bounded(MAX_DOCUMENT_JOBS as usize, || {
                    budget.observe_cleanup_control()
                })
            else {
                self.raw.poison_browser_script();
                return Err(ClassicScriptOutcome::RuntimePoisoned);
            };
            if let Some(terminal) = budget.terminal {
                return Err(terminal.classic_outcome());
            }
            return Err(ClassicScriptOutcome::PendingJobsAtDocumentStart { retired_jobs });
        }

        budget.check_control()?;
        match self
            .raw
            .install_browser_pending_task_cap(budget.pending_task_cap())
        {
            Ok(()) => {}
            Err(crate::runtime::tasks::BrowserPendingTaskCapInstallError::Allocation) => {
                return Err(ClassicScriptOutcome::ResourceLimit(
                    ResourceLimitKind::EngineAllocation,
                ));
            }
            Err(crate::runtime::tasks::BrowserPendingTaskCapInstallError::Invariant) => {
                self.raw.poison_browser_script();
                return Err(ClassicScriptOutcome::RuntimePoisoned);
            }
        }
        self.document_budget = Some(budget);
        Ok(())
    }

    fn finish_document_script_callback(&mut self) -> Result<(), ClassicScriptOutcome> {
        let Some(mut budget) = self.document_budget.take() else {
            if self.raw.browser_host_is_active_for_cleanup() {
                std::process::abort();
            }
            if !self.raw.browser_script_is_poisoned() {
                self.raw.poison_browser_script();
            }
            return Err(ClassicScriptOutcome::EnginePanic);
        };

        if self.raw.browser_script_is_poisoned() {
            return Err(budget
                .terminal
                .map(DocumentTerminal::classic_outcome)
                .unwrap_or(ClassicScriptOutcome::RuntimePoisoned));
        }

        let retirement = self.retire_active_document_tasks(&mut budget);
        let retired_jobs = retirement.retired;
        if !self.raw.browser_script_document_session_is_idle(
            budget.host_mode == DocumentHostMode::Installed,
        ) {
            if budget.host_mode == DocumentHostMode::Installed {
                std::process::abort();
            }
            self.raw.poison_browser_script();
            return Err(ClassicScriptOutcome::EnginePanic);
        }
        if let Some(terminal) = budget.terminal {
            return Err(terminal.classic_outcome());
        }
        if retired_jobs != 0 {
            return Err(ClassicScriptOutcome::PendingJobsAtDocumentExit { retired_jobs });
        }
        Ok(())
    }

    fn abort_document_script_callback_for_unwind(&mut self, host_mode: DocumentHostMode) {
        let Some(mut budget) = self.document_budget.take() else {
            if host_mode == DocumentHostMode::Installed {
                std::process::abort();
            }
            if !self.raw.browser_script_is_poisoned() {
                self.raw.poison_browser_script();
            }
            return;
        };
        if self.raw.browser_script_is_poisoned() {
            return;
        }

        let _retirement = self.retire_active_document_tasks(&mut budget);
        self.raw.poison_browser_script();
    }

    fn retire_active_document_tasks(
        &mut self,
        budget: &mut DocumentScriptBudgetState,
    ) -> crate::runtime::tasks::BrowserPendingTaskRetirement {
        let pending_cap = budget.pending_task_cap();
        let overflow = self.raw.browser_pending_task_cap_overflow();
        if let Some(limit) = overflow {
            budget.latch_pending_task_overflow(limit);
        }
        let retirement = self
            .raw
            .retire_browser_pending_task_cap(pending_cap, || budget.observe_cleanup_control());
        if retirement.overflowed != overflow.is_some() || retirement.peak_len > pending_cap {
            std::process::abort();
        }
        retirement
    }

    /// Charge one classified script candidate which intentionally does not enter Brimstone (for
    /// example an excluded external, module, or `nomodule` script). The pre-script checkpoint is a
    /// separate explicit phase and no post-script checkpoint is fabricated for this operation.
    pub fn account_skipped_document_script(
        &mut self,
        source_bytes: usize,
    ) -> Result<(), ClassicScriptOutcome> {
        let Some(budget) = self.document_budget.as_mut() else {
            return Err(ClassicScriptOutcome::RuntimeBusy);
        };
        budget.account_candidate(source_bytes)
    }

    /// Execute one admitted classic script in the active document budget without draining jobs.
    pub fn execute_document_classic(
        &mut self,
        request: ClassicScriptRequest<'_>,
    ) -> ClassicScriptExecution {
        let metadata = AdmissionMetadata::new(request);
        let Some(mut budget) = self.document_budget.take() else {
            return ClassicScriptExecution {
                outcome: ClassicScriptOutcome::RuntimeBusy,
                report: ClassicScriptReport::empty(metadata),
            };
        };
        if budget.host_mode != DocumentHostMode::None {
            self.document_budget = Some(budget);
            return ClassicScriptExecution {
                outcome: ClassicScriptOutcome::RuntimeBusy,
                report: ClassicScriptReport::empty(metadata),
            };
        }
        if let Some(outcome) = budget.terminal_classic_outcome() {
            self.document_budget = Some(budget);
            return ClassicScriptExecution {
                outcome,
                report: ClassicScriptReport::empty(metadata),
            };
        }
        if let Err(outcome) = budget.account_candidate(request.source.len()) {
            self.document_budget = Some(budget);
            return ClassicScriptExecution {
                outcome,
                report: ClassicScriptReport::empty(metadata),
            };
        }

        let phase = budget.phase_admission();
        let mut execution = self.execute_classic_controlled(request, phase);
        budget.finish_classic_phase(&mut execution);
        self.document_budget = Some(budget);
        execution
    }

    /// Execute one admitted classic script with the host authority borrowed by
    /// [`Self::with_hosted_document_script_budget`]. No replacement host can be supplied here.
    pub fn execute_hosted_document_classic(
        &mut self,
        request: ClassicScriptRequest<'_>,
    ) -> BrowserHostClassicExecution {
        let metadata = AdmissionMetadata::new(request);
        let Some(mut budget) = self.document_budget.take() else {
            return BrowserHostClassicExecution {
                script: ClassicScriptExecution {
                    outcome: ClassicScriptOutcome::RuntimeBusy,
                    report: ClassicScriptReport::empty(metadata),
                },
                host: BrowserHostPhaseOutcome::NotStarted,
            };
        };
        if budget.host_mode != DocumentHostMode::Installed || !self.raw.browser_host.is_active() {
            self.document_budget = Some(budget);
            return BrowserHostClassicExecution {
                script: ClassicScriptExecution {
                    outcome: ClassicScriptOutcome::RuntimeBusy,
                    report: ClassicScriptReport::empty(metadata),
                },
                host: BrowserHostPhaseOutcome::NotStarted,
            };
        }
        if let Some(outcome) = budget.terminal_classic_outcome() {
            self.document_budget = Some(budget);
            return BrowserHostClassicExecution {
                script: ClassicScriptExecution {
                    outcome,
                    report: ClassicScriptReport::empty(metadata),
                },
                host: BrowserHostPhaseOutcome::NotStarted,
            };
        }
        if let Err(outcome) = budget.account_candidate(request.source.len()) {
            self.document_budget = Some(budget);
            return BrowserHostClassicExecution {
                script: ClassicScriptExecution {
                    outcome,
                    report: ClassicScriptReport::empty(metadata),
                },
                host: BrowserHostPhaseOutcome::NotStarted,
            };
        }

        let phase = budget.phase_admission();
        let mut execution = self.execute_classic_with_installed_host_controlled(request, phase);
        budget.finish_classic_phase(&mut execution.script);
        self.document_budget = Some(budget);
        execution
    }

    /// Perform one explicit microtask checkpoint in the active document budget.
    pub fn perform_document_microtask_checkpoint(&mut self) -> MicrotaskCheckpointExecution {
        let metadata = AdmissionMetadata::empty();
        let Some(mut budget) = self.document_budget.take() else {
            return MicrotaskCheckpointExecution {
                outcome: MicrotaskCheckpointOutcome::RuntimeBusy,
                report: ClassicScriptReport::empty(metadata),
            };
        };
        if budget.host_mode != DocumentHostMode::None {
            self.document_budget = Some(budget);
            return MicrotaskCheckpointExecution {
                outcome: MicrotaskCheckpointOutcome::RuntimeBusy,
                report: ClassicScriptReport::empty(metadata),
            };
        }
        if let Some(outcome) = budget.terminal_checkpoint_outcome() {
            self.document_budget = Some(budget);
            return MicrotaskCheckpointExecution {
                outcome,
                report: ClassicScriptReport::empty(metadata),
            };
        }

        let phase = budget.phase_admission();
        let mut execution = self.perform_microtask_checkpoint_controlled(phase);
        budget.finish_checkpoint_phase(&mut execution);
        self.document_budget = Some(budget);
        execution
    }

    /// Perform one explicit microtask checkpoint with the host authority borrowed by
    /// [`Self::with_hosted_document_script_budget`]. No replacement host can be supplied here.
    pub fn perform_hosted_document_microtask_checkpoint(
        &mut self,
    ) -> BrowserHostMicrotaskExecution {
        let metadata = AdmissionMetadata::empty();
        let Some(mut budget) = self.document_budget.take() else {
            return BrowserHostMicrotaskExecution {
                checkpoint: MicrotaskCheckpointExecution {
                    outcome: MicrotaskCheckpointOutcome::RuntimeBusy,
                    report: ClassicScriptReport::empty(metadata),
                },
                host: BrowserHostPhaseOutcome::NotStarted,
            };
        };
        if budget.host_mode != DocumentHostMode::Installed || !self.raw.browser_host.is_active() {
            self.document_budget = Some(budget);
            return BrowserHostMicrotaskExecution {
                checkpoint: MicrotaskCheckpointExecution {
                    outcome: MicrotaskCheckpointOutcome::RuntimeBusy,
                    report: ClassicScriptReport::empty(metadata),
                },
                host: BrowserHostPhaseOutcome::NotStarted,
            };
        }
        if let Some(outcome) = budget.terminal_checkpoint_outcome() {
            self.document_budget = Some(budget);
            return BrowserHostMicrotaskExecution {
                checkpoint: MicrotaskCheckpointExecution {
                    outcome,
                    report: ClassicScriptReport::empty(metadata),
                },
                host: BrowserHostPhaseOutcome::NotStarted,
            };
        }

        let phase = budget.phase_admission();
        let mut execution = self.perform_microtask_checkpoint_with_installed_host_controlled(phase);
        budget.finish_checkpoint_phase(&mut execution.checkpoint);
        self.document_budget = Some(budget);
        execution
    }

    pub fn document_script_candidates(&self) -> Option<u32> {
        self.document_budget
            .as_ref()
            .map(|budget| budget.script_candidates)
    }

    pub fn document_source_bytes(&self) -> Option<usize> {
        self.document_budget
            .as_ref()
            .map(|budget| budget.source_bytes)
    }

    pub fn document_opcodes_executed(&self) -> Option<u64> {
        self.document_budget
            .as_ref()
            .map(|budget| budget.opcodes_executed)
    }

    pub fn document_managed_allocation_bytes(&self) -> Option<usize> {
        self.document_budget
            .as_ref()
            .map(|budget| budget.managed_allocation_bytes)
    }

    pub fn document_jobs_executed(&self) -> Option<u64> {
        self.document_budget
            .as_ref()
            .map(|budget| budget.jobs_executed)
    }

    pub fn document_diagnostics_emitted(&self) -> Option<usize> {
        self.document_budget
            .as_ref()
            .map(|budget| budget.diagnostics_emitted)
    }

    /// Parse, compile, and evaluate one classic script without draining promise jobs. The caller
    /// can report a thrown exception in HTML ordering, then invoke
    /// [`Self::perform_microtask_checkpoint`]. This is a contained integration surface, not
    /// permission to run untrusted web content; see [`ClassicScriptOutcome::EnginePanic`] and the
    /// W8-A2R handoff.
    pub fn execute_classic(
        &mut self,
        request: ClassicScriptRequest<'_>,
        limits: ClassicScriptLimits,
        interrupt: &ScriptInterruptHandle,
    ) -> ClassicScriptExecution {
        if self.document_budget.is_some() {
            return ClassicScriptExecution {
                outcome: ClassicScriptOutcome::RuntimeBusy,
                report: ClassicScriptReport::empty(AdmissionMetadata::new(request)),
            };
        }
        self.execute_classic_controlled(
            request,
            PhaseAdmission::relative(limits, interrupt.requested.clone()),
        )
    }

    fn execute_classic_controlled(
        &mut self,
        request: ClassicScriptRequest<'_>,
        phase: PhaseAdmission,
    ) -> ClassicScriptExecution {
        let PhaseAdmission { limits, control, diagnostics } = phase;
        let metadata = AdmissionMetadata::new(request);
        if self.raw.browser_script_is_poisoned() {
            return ClassicScriptExecution {
                outcome: ClassicScriptOutcome::RuntimePoisoned,
                report: ClassicScriptReport::empty(metadata),
            };
        }
        if let Some(outcome) = metadata.rejection.clone() {
            return ClassicScriptExecution {
                outcome,
                report: ClassicScriptReport::empty(metadata),
            };
        }

        let mut admission = match BrowserAdmissionGuard::install(
            self.raw,
            limits,
            control,
            metadata.clone(),
            AdmissionKind::ClassicScript,
        ) {
            Ok(admission) => admission,
            Err(AdmissionInstallError::Busy) => {
                return ClassicScriptExecution {
                    outcome: ClassicScriptOutcome::RuntimeBusy,
                    report: ClassicScriptReport::empty(metadata),
                };
            }
            Err(AdmissionInstallError::Poisoned) => {
                return ClassicScriptExecution {
                    outcome: ClassicScriptOutcome::RuntimePoisoned,
                    report: ClassicScriptReport::empty(metadata),
                };
            }
        };

        let raw = self.raw;
        let execution = catch_unwind(AssertUnwindSafe(|| {
            let _handles = HandleScopeGuard::new(raw);
            let outcome = execute_classic_inner(raw, request, diagnostics);
            raw.browser_script_poll_phase();
            outcome
        }));

        let outcome = match execution {
            Ok(outcome) => {
                if matches!(
                    outcome,
                    ClassicScriptOutcome::ResourceLimit(ResourceLimitKind::EngineAllocation)
                ) {
                    // A VM OOM can occur after Promise work was queued. No job from that
                    // partially completed script may survive into a later checkpoint.
                    raw.clear_browser_script_tasks();
                }
                outcome
            }
            Err(payload) if payload.is::<BrowserPolicyUnwind>() => match admission.termination() {
                Some(termination) => {
                    raw.clear_browser_script_tasks();
                    termination.into_outcome()
                }
                None => {
                    raw.poison_browser_script();
                    ClassicScriptOutcome::EnginePanic
                }
            },
            Err(_) => {
                // A pre-effect test panic can prove local RAII, but cannot prove arbitrary parser,
                // compiler, builtin, or moving-GC state unwind-safe. Permanently retire this
                // context from the browser seam without inspecting its VM, GC, or task queue.
                raw.poison_browser_script();
                ClassicScriptOutcome::EnginePanic
            }
        };

        if matches!(outcome, ClassicScriptOutcome::EnginePanic) {
            let report = admission.finish_without_task_queue();
            return ClassicScriptExecution { outcome, report };
        }
        raw.require_browser_script_idle_or_abort();
        let report = admission.finish();
        ClassicScriptExecution { outcome, report }
    }

    /// Execute one classic-script phase with a caller-owned, rooted DOM task capability.
    ///
    /// The capability is erased only while this synchronous call is active. Host functions publish
    /// each accepted DOM operation synchronously; `finish_phase` returns only scalar evidence.
    /// Ordinary JavaScript throws therefore preserve earlier DOM effects and are returned before
    /// the caller chooses to enter the explicit host-aware microtask checkpoint.
    pub fn execute_classic_with_host<H: BrowserHostTask>(
        &mut self,
        host: &mut H,
        request: ClassicScriptRequest<'_>,
        limits: ClassicScriptLimits,
        interrupt: &ScriptInterruptHandle,
    ) -> BrowserHostClassicExecution {
        if self.document_budget.is_some() {
            return BrowserHostClassicExecution {
                script: ClassicScriptExecution {
                    outcome: ClassicScriptOutcome::RuntimeBusy,
                    report: ClassicScriptReport::empty(AdmissionMetadata::new(request)),
                },
                host: BrowserHostPhaseOutcome::NotStarted,
            };
        }
        self.execute_classic_with_host_controlled(
            host,
            request,
            PhaseAdmission::relative(limits, interrupt.requested.clone()),
        )
    }

    fn execute_classic_with_host_controlled<H: BrowserHostTask>(
        &mut self,
        host: &mut H,
        request: ClassicScriptRequest<'_>,
        phase: PhaseAdmission,
    ) -> BrowserHostClassicExecution {
        let PhaseAdmission { limits, control, diagnostics } = phase;
        let metadata = AdmissionMetadata::new(request);
        if self.raw.browser_script_is_poisoned() {
            return BrowserHostClassicExecution {
                script: ClassicScriptExecution {
                    outcome: ClassicScriptOutcome::RuntimePoisoned,
                    report: ClassicScriptReport::empty(metadata),
                },
                host: BrowserHostPhaseOutcome::NotStarted,
            };
        }
        if let Some(outcome) = metadata.rejection.clone() {
            return BrowserHostClassicExecution {
                script: ClassicScriptExecution {
                    outcome,
                    report: ClassicScriptReport::empty(metadata),
                },
                host: BrowserHostPhaseOutcome::NotStarted,
            };
        }

        let mut admission = match BrowserAdmissionGuard::install(
            self.raw,
            limits,
            control,
            metadata.clone(),
            AdmissionKind::ClassicScript,
        ) {
            Ok(admission) => admission,
            Err(AdmissionInstallError::Busy) => {
                return BrowserHostClassicExecution {
                    script: ClassicScriptExecution {
                        outcome: ClassicScriptOutcome::RuntimeBusy,
                        report: ClassicScriptReport::empty(metadata),
                    },
                    host: BrowserHostPhaseOutcome::NotStarted,
                };
            }
            Err(AdmissionInstallError::Poisoned) => {
                return BrowserHostClassicExecution {
                    script: ClassicScriptExecution {
                        outcome: ClassicScriptOutcome::RuntimePoisoned,
                        report: ClassicScriptReport::empty(metadata),
                    },
                    host: BrowserHostPhaseOutcome::NotStarted,
                };
            }
        };

        let raw = self.raw;
        let mut host_disposition = HostPhaseDisposition::armed();
        let execution = catch_unwind(AssertUnwindSafe(|| {
            let _handles = HandleScopeGuard::new(raw);
            // Cancellation/deadline must win before installing an erased host borrow or publishing
            // the reserved binding.
            raw.browser_script_poll_phase();
            let _host_scope = match BrowserHostScopeGuard::install(raw, &mut *host) {
                Ok(scope) => scope,
                Err(error) => {
                    return (
                        ClassicScriptOutcome::HostFailure(error),
                        BrowserHostPhaseOutcome::Failed(error),
                    );
                }
            };
            if let Err(error) = raw.validate_browser_host_phase() {
                raw.clear_browser_script_tasks();
                discard_installed_host_once(raw, &mut host_disposition);
                return (
                    ClassicScriptOutcome::HostFailure(error),
                    BrowserHostPhaseOutcome::Failed(error),
                );
            }

            if let Err(error) = install_browser_host_bindings(raw) {
                raw.clear_browser_script_tasks();
                discard_installed_host_once(raw, &mut host_disposition);
                let error = match error {
                    #[cfg(feature = "alloc_error")]
                    BrowserHostInstallError::Allocation => {
                        return (
                            ClassicScriptOutcome::ResourceLimit(
                                ResourceLimitKind::EngineAllocation,
                            ),
                            BrowserHostPhaseOutcome::Discarded,
                        );
                    }
                    BrowserHostInstallError::BindingCollision => BrowserHostError::BindingCollision,
                    BrowserHostInstallError::Internal => BrowserHostError::Internal,
                };
                return (
                    ClassicScriptOutcome::HostFailure(error),
                    BrowserHostPhaseOutcome::Failed(error),
                );
            }

            let mut outcome = execute_classic_inner(raw, request, diagnostics);
            if matches!(
                outcome,
                ClassicScriptOutcome::ResourceLimit(ResourceLimitKind::EngineAllocation)
            ) {
                raw.clear_browser_script_tasks();
            }

            // This is the final fallible policy poll before the one-way host disposition.
            raw.browser_script_poll_phase();
            let host_outcome = if classic_outcome_finishes_host_phase(&outcome) {
                match finish_installed_host_once(raw, &mut host_disposition) {
                    Ok(commit) => BrowserHostPhaseOutcome::Completed(commit),
                    Err(error) => {
                        raw.clear_browser_script_tasks();
                        discard_installed_host_once(raw, &mut host_disposition);
                        outcome = ClassicScriptOutcome::HostFailure(error);
                        BrowserHostPhaseOutcome::Failed(error)
                    }
                }
            } else {
                discard_installed_host_once(raw, &mut host_disposition);
                BrowserHostPhaseOutcome::Discarded
            };
            (outcome, host_outcome)
        }));

        let (outcome, host_outcome) = match execution {
            Ok(result) => {
                complete_direct_host_after_normal_return(host, &mut host_disposition);
                result
            }
            Err(payload) if payload.is::<BrowserPolicyUnwind>() => {
                let termination = admission.termination();
                retire_direct_host_after_unwind(host, &mut host_disposition);
                if termination.is_none() {
                    raw.poison_browser_script();
                    (ClassicScriptOutcome::EnginePanic, BrowserHostPhaseOutcome::Discarded)
                } else {
                    raw.clear_browser_script_tasks();
                    (
                        termination
                            .unwrap_or_else(|| std::process::abort())
                            .into_outcome(),
                        BrowserHostPhaseOutcome::Discarded,
                    )
                }
            }
            Err(_) => {
                // The erased scope has unwound and released its exclusive borrow. Retire the host
                // task before setting the permanent runtime poison; no host, VM, GC, or queue
                // state is touched after poison becomes authoritative.
                retire_direct_host_after_unwind(host, &mut host_disposition);
                raw.poison_browser_script();
                (ClassicScriptOutcome::EnginePanic, BrowserHostPhaseOutcome::Discarded)
            }
        };

        if matches!(outcome, ClassicScriptOutcome::EnginePanic) {
            let report = admission.finish_without_task_queue();
            return BrowserHostClassicExecution {
                script: ClassicScriptExecution { outcome, report },
                host: host_outcome,
            };
        }
        raw.require_browser_script_idle_or_abort();
        let report = admission.finish();
        BrowserHostClassicExecution {
            script: ClassicScriptExecution { outcome, report },
            host: host_outcome,
        }
    }

    fn execute_classic_with_installed_host_controlled(
        &mut self,
        request: ClassicScriptRequest<'_>,
        phase: PhaseAdmission,
    ) -> BrowserHostClassicExecution {
        let PhaseAdmission { limits, control, diagnostics } = phase;
        let metadata = AdmissionMetadata::new(request);
        if self.raw.browser_script_is_poisoned() {
            return BrowserHostClassicExecution {
                script: ClassicScriptExecution {
                    outcome: ClassicScriptOutcome::RuntimePoisoned,
                    report: ClassicScriptReport::empty(metadata),
                },
                host: BrowserHostPhaseOutcome::NotStarted,
            };
        }
        if let Some(outcome) = metadata.rejection.clone() {
            return BrowserHostClassicExecution {
                script: ClassicScriptExecution {
                    outcome,
                    report: ClassicScriptReport::empty(metadata),
                },
                host: BrowserHostPhaseOutcome::NotStarted,
            };
        }

        let mut admission = match BrowserAdmissionGuard::install(
            self.raw,
            limits,
            control,
            metadata.clone(),
            AdmissionKind::ClassicScript,
        ) {
            Ok(admission) => admission,
            Err(AdmissionInstallError::Busy) => {
                return BrowserHostClassicExecution {
                    script: ClassicScriptExecution {
                        outcome: ClassicScriptOutcome::RuntimeBusy,
                        report: ClassicScriptReport::empty(metadata),
                    },
                    host: BrowserHostPhaseOutcome::NotStarted,
                };
            }
            Err(AdmissionInstallError::Poisoned) => {
                return BrowserHostClassicExecution {
                    script: ClassicScriptExecution {
                        outcome: ClassicScriptOutcome::RuntimePoisoned,
                        report: ClassicScriptReport::empty(metadata),
                    },
                    host: BrowserHostPhaseOutcome::NotStarted,
                };
            }
        };

        let raw = self.raw;
        let mut host_disposition = HostPhaseDisposition::armed();
        let execution = catch_unwind(AssertUnwindSafe(|| {
            let _handles = HandleScopeGuard::new(raw);
            raw.browser_script_poll_phase();
            if let Err(error) = raw.validate_browser_host_phase() {
                raw.clear_browser_script_tasks();
                discard_installed_host_once(raw, &mut host_disposition);
                return (
                    ClassicScriptOutcome::HostFailure(error),
                    BrowserHostPhaseOutcome::Failed(error),
                );
            }

            if let Err(error) = install_browser_host_bindings(raw) {
                raw.clear_browser_script_tasks();
                discard_installed_host_once(raw, &mut host_disposition);
                let error = match error {
                    #[cfg(feature = "alloc_error")]
                    BrowserHostInstallError::Allocation => {
                        return (
                            ClassicScriptOutcome::ResourceLimit(
                                ResourceLimitKind::EngineAllocation,
                            ),
                            BrowserHostPhaseOutcome::Discarded,
                        );
                    }
                    BrowserHostInstallError::BindingCollision => BrowserHostError::BindingCollision,
                    BrowserHostInstallError::Internal => BrowserHostError::Internal,
                };
                return (
                    ClassicScriptOutcome::HostFailure(error),
                    BrowserHostPhaseOutcome::Failed(error),
                );
            }

            let mut outcome = execute_classic_inner(raw, request, diagnostics);
            if matches!(
                outcome,
                ClassicScriptOutcome::ResourceLimit(ResourceLimitKind::EngineAllocation)
            ) {
                raw.clear_browser_script_tasks();
            }

            // This is the final fallible policy poll before the one-way host disposition.
            raw.browser_script_poll_phase();
            let host_outcome = if classic_outcome_finishes_host_phase(&outcome) {
                match finish_installed_host_once(raw, &mut host_disposition) {
                    Ok(commit) => BrowserHostPhaseOutcome::Completed(commit),
                    Err(error) => {
                        raw.clear_browser_script_tasks();
                        discard_installed_host_once(raw, &mut host_disposition);
                        outcome = ClassicScriptOutcome::HostFailure(error);
                        BrowserHostPhaseOutcome::Failed(error)
                    }
                }
            } else {
                discard_installed_host_once(raw, &mut host_disposition);
                BrowserHostPhaseOutcome::Discarded
            };
            (outcome, host_outcome)
        }));

        let (outcome, host_outcome) = match execution {
            Ok(result) => {
                complete_installed_host_after_normal_return(raw, &mut host_disposition);
                result
            }
            Err(payload) if payload.is::<BrowserPolicyUnwind>() => {
                let termination = admission.termination();
                retire_installed_host_after_unwind(raw, &mut host_disposition);
                if termination.is_none() {
                    raw.poison_browser_script();
                    (ClassicScriptOutcome::EnginePanic, BrowserHostPhaseOutcome::Discarded)
                } else {
                    raw.clear_browser_script_tasks();
                    (
                        termination
                            .unwrap_or_else(|| std::process::abort())
                            .into_outcome(),
                        BrowserHostPhaseOutcome::Discarded,
                    )
                }
            }
            Err(_) => {
                retire_installed_host_after_unwind(raw, &mut host_disposition);
                raw.poison_browser_script();
                (ClassicScriptOutcome::EnginePanic, BrowserHostPhaseOutcome::Discarded)
            }
        };

        if matches!(outcome, ClassicScriptOutcome::EnginePanic) {
            let report = admission.finish_without_task_queue();
            return BrowserHostClassicExecution {
                script: ClassicScriptExecution { outcome, report },
                host: host_outcome,
            };
        }
        raw.require_browser_script_idle_or_abort();
        let report = admission.finish();
        BrowserHostClassicExecution {
            script: ClassicScriptExecution { outcome, report },
            host: host_outcome,
        }
    }

    /// Perform one explicit, rooted, bounded promise-job checkpoint after the embedding has
    /// handled the primary script outcome. A job failure is fail-closed and discards remaining
    /// jobs because Brimstone does not yet expose the HTML host error-reporting continuation.
    pub fn perform_microtask_checkpoint(
        &mut self,
        limits: ClassicScriptLimits,
        interrupt: &ScriptInterruptHandle,
    ) -> MicrotaskCheckpointExecution {
        if self.document_budget.is_some() {
            return MicrotaskCheckpointExecution {
                outcome: MicrotaskCheckpointOutcome::RuntimeBusy,
                report: ClassicScriptReport::empty(AdmissionMetadata::empty()),
            };
        }
        self.perform_microtask_checkpoint_controlled(PhaseAdmission::relative(
            limits,
            interrupt.requested.clone(),
        ))
    }

    fn perform_microtask_checkpoint_controlled(
        &mut self,
        phase: PhaseAdmission,
    ) -> MicrotaskCheckpointExecution {
        let PhaseAdmission { limits, control, diagnostics: _ } = phase;
        let metadata = AdmissionMetadata::empty();
        let mut admission = match BrowserAdmissionGuard::install(
            self.raw,
            limits,
            control,
            metadata.clone(),
            AdmissionKind::MicrotaskCheckpoint,
        ) {
            Ok(admission) => admission,
            Err(AdmissionInstallError::Busy) => {
                return MicrotaskCheckpointExecution {
                    outcome: MicrotaskCheckpointOutcome::RuntimeBusy,
                    report: ClassicScriptReport::empty(metadata),
                };
            }
            Err(AdmissionInstallError::Poisoned) => {
                return MicrotaskCheckpointExecution {
                    outcome: MicrotaskCheckpointOutcome::RuntimePoisoned,
                    report: ClassicScriptReport::empty(metadata),
                };
            }
        };

        let mut raw = self.raw;
        let execution = catch_unwind(AssertUnwindSafe(|| {
            let _handles = HandleScopeGuard::new(raw);
            raw.browser_script_poll_phase();
            let result = raw.run_browser_script_tasks();
            raw.browser_script_poll_phase();
            match result {
                Ok(()) => MicrotaskCheckpointOutcome::Complete,
                Err(error) => {
                    // `EvalError::Value` contains a moving-GC handle escaped into this outer
                    // scope by `run_browser_script_tasks`. Reduce it to a pointer-free summary
                    // before the guard closes; returning the raw error would leave a dangling
                    // handle cell for the caller to inspect after scope exit.
                    let outcome = summarize_checkpoint_error(error);
                    raw.clear_browser_script_tasks();
                    outcome
                }
            }
        }));

        let outcome = match execution {
            Ok(outcome) => outcome,
            Err(payload) if payload.is::<BrowserPolicyUnwind>() => match admission.termination() {
                Some(termination) => {
                    raw.clear_browser_script_tasks();
                    termination.into_checkpoint_outcome()
                }
                None => {
                    raw.poison_browser_script();
                    MicrotaskCheckpointOutcome::EnginePanic
                }
            },
            Err(_) => {
                raw.poison_browser_script();
                MicrotaskCheckpointOutcome::EnginePanic
            }
        };

        if matches!(outcome, MicrotaskCheckpointOutcome::EnginePanic) {
            let report = admission.finish_without_task_queue();
            return MicrotaskCheckpointExecution { outcome, report };
        }
        raw.require_browser_script_idle_or_abort();
        let report = admission.finish();
        MicrotaskCheckpointExecution { outcome, report }
    }

    /// Drain one explicit microtask checkpoint with the same rooted host task used for the
    /// preceding classic-script phase. The caller observes and reports the primary script result
    /// before choosing to invoke this method.
    pub fn perform_microtask_checkpoint_with_host<H: BrowserHostTask>(
        &mut self,
        host: &mut H,
        limits: ClassicScriptLimits,
        interrupt: &ScriptInterruptHandle,
    ) -> BrowserHostMicrotaskExecution {
        if self.document_budget.is_some() {
            return BrowserHostMicrotaskExecution {
                checkpoint: MicrotaskCheckpointExecution {
                    outcome: MicrotaskCheckpointOutcome::RuntimeBusy,
                    report: ClassicScriptReport::empty(AdmissionMetadata::empty()),
                },
                host: BrowserHostPhaseOutcome::NotStarted,
            };
        }
        self.perform_microtask_checkpoint_with_host_controlled(
            host,
            PhaseAdmission::relative(limits, interrupt.requested.clone()),
        )
    }

    fn perform_microtask_checkpoint_with_host_controlled<H: BrowserHostTask>(
        &mut self,
        host: &mut H,
        phase: PhaseAdmission,
    ) -> BrowserHostMicrotaskExecution {
        let PhaseAdmission { limits, control, diagnostics: _ } = phase;
        let metadata = AdmissionMetadata::empty();
        let mut admission = match BrowserAdmissionGuard::install(
            self.raw,
            limits,
            control,
            metadata.clone(),
            AdmissionKind::MicrotaskCheckpoint,
        ) {
            Ok(admission) => admission,
            Err(AdmissionInstallError::Busy) => {
                return BrowserHostMicrotaskExecution {
                    checkpoint: MicrotaskCheckpointExecution {
                        outcome: MicrotaskCheckpointOutcome::RuntimeBusy,
                        report: ClassicScriptReport::empty(metadata),
                    },
                    host: BrowserHostPhaseOutcome::NotStarted,
                };
            }
            Err(AdmissionInstallError::Poisoned) => {
                return BrowserHostMicrotaskExecution {
                    checkpoint: MicrotaskCheckpointExecution {
                        outcome: MicrotaskCheckpointOutcome::RuntimePoisoned,
                        report: ClassicScriptReport::empty(metadata),
                    },
                    host: BrowserHostPhaseOutcome::NotStarted,
                };
            }
        };

        let mut raw = self.raw;
        let mut host_disposition = HostPhaseDisposition::armed();
        let execution = catch_unwind(AssertUnwindSafe(|| {
            let _handles = HandleScopeGuard::new(raw);
            raw.browser_script_poll_phase();
            let _host_scope = match BrowserHostScopeGuard::install(raw, &mut *host) {
                Ok(scope) => scope,
                Err(error) => {
                    return (
                        MicrotaskCheckpointOutcome::HostFailure(error),
                        BrowserHostPhaseOutcome::Failed(error),
                    );
                }
            };
            if let Err(error) = raw.validate_browser_host_phase() {
                raw.clear_browser_script_tasks();
                discard_installed_host_once(raw, &mut host_disposition);
                return (
                    MicrotaskCheckpointOutcome::HostFailure(error),
                    BrowserHostPhaseOutcome::Failed(error),
                );
            }

            if let Err(error) = install_browser_host_bindings(raw) {
                raw.clear_browser_script_tasks();
                discard_installed_host_once(raw, &mut host_disposition);
                let error = match error {
                    #[cfg(feature = "alloc_error")]
                    BrowserHostInstallError::Allocation => {
                        return (
                            MicrotaskCheckpointOutcome::ResourceLimit(
                                ResourceLimitKind::EngineAllocation,
                            ),
                            BrowserHostPhaseOutcome::Discarded,
                        );
                    }
                    BrowserHostInstallError::BindingCollision => BrowserHostError::BindingCollision,
                    BrowserHostInstallError::Internal => BrowserHostError::Internal,
                };
                return (
                    MicrotaskCheckpointOutcome::HostFailure(error),
                    BrowserHostPhaseOutcome::Failed(error),
                );
            }

            raw.browser_script_poll_phase();
            let result = raw.run_browser_script_tasks();
            raw.browser_script_poll_phase();
            let mut outcome = match result {
                Ok(()) => MicrotaskCheckpointOutcome::Complete,
                Err(error) => {
                    let outcome = summarize_checkpoint_error(error);
                    raw.clear_browser_script_tasks();
                    outcome
                }
            };

            // This is the final fallible policy poll before the one-way host disposition.
            raw.browser_script_poll_phase();
            let host_outcome = if checkpoint_outcome_finishes_host_phase(&outcome) {
                match finish_installed_host_once(raw, &mut host_disposition) {
                    Ok(commit) => BrowserHostPhaseOutcome::Completed(commit),
                    Err(error) => {
                        raw.clear_browser_script_tasks();
                        discard_installed_host_once(raw, &mut host_disposition);
                        outcome = MicrotaskCheckpointOutcome::HostFailure(error);
                        BrowserHostPhaseOutcome::Failed(error)
                    }
                }
            } else {
                discard_installed_host_once(raw, &mut host_disposition);
                BrowserHostPhaseOutcome::Discarded
            };
            (outcome, host_outcome)
        }));

        let (outcome, host_outcome) = match execution {
            Ok(result) => {
                complete_direct_host_after_normal_return(host, &mut host_disposition);
                result
            }
            Err(payload) if payload.is::<BrowserPolicyUnwind>() => {
                let termination = admission.termination();
                retire_direct_host_after_unwind(host, &mut host_disposition);
                if termination.is_none() {
                    raw.poison_browser_script();
                    (MicrotaskCheckpointOutcome::EnginePanic, BrowserHostPhaseOutcome::Discarded)
                } else {
                    raw.clear_browser_script_tasks();
                    (
                        termination
                            .unwrap_or_else(|| std::process::abort())
                            .into_checkpoint_outcome(),
                        BrowserHostPhaseOutcome::Discarded,
                    )
                }
            }
            Err(_) => {
                retire_direct_host_after_unwind(host, &mut host_disposition);
                raw.poison_browser_script();
                (MicrotaskCheckpointOutcome::EnginePanic, BrowserHostPhaseOutcome::Discarded)
            }
        };

        if matches!(outcome, MicrotaskCheckpointOutcome::EnginePanic) {
            let report = admission.finish_without_task_queue();
            return BrowserHostMicrotaskExecution {
                checkpoint: MicrotaskCheckpointExecution { outcome, report },
                host: host_outcome,
            };
        }
        raw.require_browser_script_idle_or_abort();
        let report = admission.finish();
        BrowserHostMicrotaskExecution {
            checkpoint: MicrotaskCheckpointExecution { outcome, report },
            host: host_outcome,
        }
    }

    fn perform_microtask_checkpoint_with_installed_host_controlled(
        &mut self,
        phase: PhaseAdmission,
    ) -> BrowserHostMicrotaskExecution {
        let PhaseAdmission { limits, control, diagnostics: _ } = phase;
        let metadata = AdmissionMetadata::empty();
        let mut admission = match BrowserAdmissionGuard::install(
            self.raw,
            limits,
            control,
            metadata.clone(),
            AdmissionKind::MicrotaskCheckpoint,
        ) {
            Ok(admission) => admission,
            Err(AdmissionInstallError::Busy) => {
                return BrowserHostMicrotaskExecution {
                    checkpoint: MicrotaskCheckpointExecution {
                        outcome: MicrotaskCheckpointOutcome::RuntimeBusy,
                        report: ClassicScriptReport::empty(metadata),
                    },
                    host: BrowserHostPhaseOutcome::NotStarted,
                };
            }
            Err(AdmissionInstallError::Poisoned) => {
                return BrowserHostMicrotaskExecution {
                    checkpoint: MicrotaskCheckpointExecution {
                        outcome: MicrotaskCheckpointOutcome::RuntimePoisoned,
                        report: ClassicScriptReport::empty(metadata),
                    },
                    host: BrowserHostPhaseOutcome::NotStarted,
                };
            }
        };

        let mut raw = self.raw;
        let mut host_disposition = HostPhaseDisposition::armed();
        let execution = catch_unwind(AssertUnwindSafe(|| {
            let _handles = HandleScopeGuard::new(raw);
            raw.browser_script_poll_phase();
            if let Err(error) = raw.validate_browser_host_phase() {
                raw.clear_browser_script_tasks();
                discard_installed_host_once(raw, &mut host_disposition);
                return (
                    MicrotaskCheckpointOutcome::HostFailure(error),
                    BrowserHostPhaseOutcome::Failed(error),
                );
            }

            if let Err(error) = install_browser_host_bindings(raw) {
                raw.clear_browser_script_tasks();
                discard_installed_host_once(raw, &mut host_disposition);
                let error = match error {
                    #[cfg(feature = "alloc_error")]
                    BrowserHostInstallError::Allocation => {
                        return (
                            MicrotaskCheckpointOutcome::ResourceLimit(
                                ResourceLimitKind::EngineAllocation,
                            ),
                            BrowserHostPhaseOutcome::Discarded,
                        );
                    }
                    BrowserHostInstallError::BindingCollision => BrowserHostError::BindingCollision,
                    BrowserHostInstallError::Internal => BrowserHostError::Internal,
                };
                return (
                    MicrotaskCheckpointOutcome::HostFailure(error),
                    BrowserHostPhaseOutcome::Failed(error),
                );
            }

            raw.browser_script_poll_phase();
            let result = raw.run_browser_script_tasks();
            raw.browser_script_poll_phase();
            let mut outcome = match result {
                Ok(()) => MicrotaskCheckpointOutcome::Complete,
                Err(error) => {
                    let outcome = summarize_checkpoint_error(error);
                    raw.clear_browser_script_tasks();
                    outcome
                }
            };

            // This is the final fallible policy poll before the one-way host disposition.
            raw.browser_script_poll_phase();
            let host_outcome = if checkpoint_outcome_finishes_host_phase(&outcome) {
                match finish_installed_host_once(raw, &mut host_disposition) {
                    Ok(commit) => BrowserHostPhaseOutcome::Completed(commit),
                    Err(error) => {
                        raw.clear_browser_script_tasks();
                        discard_installed_host_once(raw, &mut host_disposition);
                        outcome = MicrotaskCheckpointOutcome::HostFailure(error);
                        BrowserHostPhaseOutcome::Failed(error)
                    }
                }
            } else {
                discard_installed_host_once(raw, &mut host_disposition);
                BrowserHostPhaseOutcome::Discarded
            };
            (outcome, host_outcome)
        }));

        let (outcome, host_outcome) = match execution {
            Ok(result) => {
                complete_installed_host_after_normal_return(raw, &mut host_disposition);
                result
            }
            Err(payload) if payload.is::<BrowserPolicyUnwind>() => {
                let termination = admission.termination();
                retire_installed_host_after_unwind(raw, &mut host_disposition);
                if termination.is_none() {
                    raw.poison_browser_script();
                    (MicrotaskCheckpointOutcome::EnginePanic, BrowserHostPhaseOutcome::Discarded)
                } else {
                    raw.clear_browser_script_tasks();
                    (
                        termination
                            .unwrap_or_else(|| std::process::abort())
                            .into_checkpoint_outcome(),
                        BrowserHostPhaseOutcome::Discarded,
                    )
                }
            }
            Err(_) => {
                retire_installed_host_after_unwind(raw, &mut host_disposition);
                raw.poison_browser_script();
                (MicrotaskCheckpointOutcome::EnginePanic, BrowserHostPhaseOutcome::Discarded)
            }
        };

        if matches!(outcome, MicrotaskCheckpointOutcome::EnginePanic) {
            let report = admission.finish_without_task_queue();
            return BrowserHostMicrotaskExecution {
                checkpoint: MicrotaskCheckpointExecution { outcome, report },
                host: host_outcome,
            };
        }
        raw.require_browser_script_idle_or_abort();
        let report = admission.finish();
        BrowserHostMicrotaskExecution {
            checkpoint: MicrotaskCheckpointExecution { outcome, report },
            host: host_outcome,
        }
    }
}

fn classic_outcome_finishes_host_phase(outcome: &ClassicScriptOutcome) -> bool {
    matches!(
        outcome,
        ClassicScriptOutcome::Success(_)
            | ClassicScriptOutcome::Thrown(_)
            | ClassicScriptOutcome::ParseError(_)
            | ClassicScriptOutcome::AnalyzeError(_)
            | ClassicScriptOutcome::CompileError(_)
    )
}

fn checkpoint_outcome_finishes_host_phase(outcome: &MicrotaskCheckpointOutcome) -> bool {
    matches!(
        outcome,
        MicrotaskCheckpointOutcome::Complete | MicrotaskCheckpointOutcome::JobThrown(_)
    )
}

#[derive(Clone, Debug, PartialEq)]
pub struct ClassicScriptExecution {
    pub outcome: ClassicScriptOutcome,
    pub report: ClassicScriptReport,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MicrotaskCheckpointExecution {
    pub outcome: MicrotaskCheckpointOutcome,
    pub report: ClassicScriptReport,
}

#[derive(Clone, Debug, PartialEq)]
pub enum MicrotaskCheckpointOutcome {
    Complete,
    JobThrown(ScriptValueSummary),
    Interrupted(InterruptReason),
    ResourceLimit(ResourceLimitKind),
    HostFailure(BrowserHostError),
    InvalidMetadata(InvalidMetadata),
    RuntimeBusy,
    RuntimePoisoned,
    EnginePanic,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ClassicScriptOutcome {
    Success(ScriptValueSummary),
    Thrown(ScriptValueSummary),
    /// A promise job threw earlier in this exact document session. This is terminal for the
    /// contained W9 coordinator and is repeated across later classic/checkpoint observations.
    JobThrown(ScriptValueSummary),
    ParseError(Vec<ScriptDiagnostic>),
    AnalyzeError(Vec<ScriptDiagnostic>),
    CompileError(ScriptDiagnostic),
    /// The document-session envelope rejected limits or nesting before invoking its callback.
    InvalidDocumentSession(LimitConfigurationError),
    /// Older queued work was counted and synchronously retired before a new document callback
    /// could start. The callback was not invoked.
    PendingJobsAtDocumentStart {
        retired_jobs: usize,
    },
    /// A nominally normal callback return left queued work. The work was counted and synchronously
    /// retired, and the session did not report success.
    PendingJobsAtDocumentExit {
        retired_jobs: usize,
    },
    Interrupted(InterruptReason),
    ResourceLimit(ResourceLimitKind),
    HostFailure(BrowserHostError),
    InvalidMetadata(InvalidMetadata),
    RuntimeBusy,
    RuntimePoisoned,
    /// An unexpected Rust panic crossed the engine boundary. VM/handle cleanup was verified, but
    /// this remains a NO-GO result for untrusted execution until Brimstone's wider panic and raw
    /// context debt is removed.
    EnginePanic,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvalidMetadata {
    EmptyFilename,
    FilenameContainsNul,
    BaseContainsNul,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InterruptReason {
    ExternalRequest,
    Deadline,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResourceLimitKind {
    ScriptCandidates { requested_total: u32, limit: u32 },
    SourceBytes { actual: usize, limit: usize },
    FilenameBytes { actual: usize, limit: usize },
    BaseBytes { actual: usize, limit: usize },
    Opcodes { limit: u64 },
    ManagedAllocationBytes { requested_total: usize, limit: usize },
    RecursionDepth { requested_depth: usize, limit: usize },
    Jobs { limit: u64 },
    PendingJobs { limit: u64 },
    Diagnostics { requested_total: usize, limit: usize },
    DiagnosticBytes { limit: usize },
    HostDiagnosticAllocation,
    EngineAllocation,
}

/// A pointer-free summary of a JavaScript completion value.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ScriptValueSummary {
    Undefined,
    Null,
    Empty,
    Boolean(bool),
    Number(f64),
    String { code_units: u32 },
    BigInt,
    Symbol,
    Object,
    UnsupportedHeapValue,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScriptDiagnostic {
    pub message: String,
    pub start_byte: Option<usize>,
    pub end_byte: Option<usize>,
}

/// Exact work counters for one admission. Opcode storage is fixed-size and report construction is
/// allocation-free; callers can iterate only the observed entries.
#[derive(Clone, Debug, PartialEq)]
pub struct ClassicScriptReport {
    source_bytes: usize,
    filename_bytes: usize,
    base_bytes: Option<usize>,
    opcodes_executed: u64,
    opcode_counts: [u64; OpCode::COUNT],
    managed_allocation_bytes: usize,
    jobs_executed: u64,
    pending_jobs_at_exit: usize,
    maximum_recursion_depth: usize,
    elapsed: Duration,
    /// Browser admission never selects the disabled baseline tier.
    pub jit_native_entries: u64,
    pub jit_side_exits: u64,
    pub jit_enabled: bool,
}

impl ClassicScriptReport {
    fn empty(metadata: AdmissionMetadata) -> Self {
        Self {
            source_bytes: metadata.source_bytes,
            filename_bytes: metadata.filename_bytes,
            base_bytes: metadata.base_bytes,
            opcodes_executed: 0,
            opcode_counts: [0; OpCode::COUNT],
            managed_allocation_bytes: 0,
            jobs_executed: 0,
            pending_jobs_at_exit: 0,
            maximum_recursion_depth: 0,
            elapsed: Duration::ZERO,
            jit_native_entries: 0,
            jit_side_exits: 0,
            jit_enabled: false,
        }
    }

    pub fn source_bytes(&self) -> usize {
        self.source_bytes
    }

    pub fn filename_bytes(&self) -> usize {
        self.filename_bytes
    }

    pub fn base_bytes(&self) -> Option<usize> {
        self.base_bytes
    }

    pub fn opcodes_executed(&self) -> u64 {
        self.opcodes_executed
    }

    pub fn opcode_count(&self, opcode: OpCode) -> u64 {
        self.opcode_counts[opcode as usize]
    }

    pub fn observed_opcodes(&self) -> impl Iterator<Item = (OpCode, u64)> + '_ {
        OpCode::ALL.iter().copied().filter_map(|opcode| {
            let count = self.opcode_count(opcode);
            (count != 0).then_some((opcode, count))
        })
    }

    pub fn managed_allocation_bytes(&self) -> usize {
        self.managed_allocation_bytes
    }

    pub fn jobs_executed(&self) -> u64 {
        self.jobs_executed
    }

    pub fn pending_jobs_at_exit(&self) -> usize {
        self.pending_jobs_at_exit
    }

    pub fn maximum_recursion_depth(&self) -> usize {
        self.maximum_recursion_depth
    }

    pub fn elapsed(&self) -> Duration {
        self.elapsed
    }
}

#[derive(Clone)]
struct AdmissionMetadata {
    source_bytes: usize,
    filename_bytes: usize,
    base_bytes: Option<usize>,
    rejection: Option<ClassicScriptOutcome>,
}

impl AdmissionMetadata {
    fn empty() -> Self {
        Self {
            source_bytes: 0,
            filename_bytes: 0,
            base_bytes: None,
            rejection: None,
        }
    }

    fn new(request: ClassicScriptRequest<'_>) -> Self {
        let source_bytes = request.source.len();
        let filename_bytes = request.filename.len();
        let base_bytes = request.base_url.map(str::len);
        let rejection = if source_bytes > MAX_CLASSIC_SCRIPT_SOURCE_BYTES {
            Some(ClassicScriptOutcome::ResourceLimit(ResourceLimitKind::SourceBytes {
                actual: source_bytes,
                limit: MAX_CLASSIC_SCRIPT_SOURCE_BYTES,
            }))
        } else if filename_bytes > MAX_CLASSIC_SCRIPT_FILENAME_BYTES {
            Some(ClassicScriptOutcome::ResourceLimit(ResourceLimitKind::FilenameBytes {
                actual: filename_bytes,
                limit: MAX_CLASSIC_SCRIPT_FILENAME_BYTES,
            }))
        } else if base_bytes.is_some_and(|bytes| bytes > MAX_CLASSIC_SCRIPT_BASE_BYTES) {
            Some(ClassicScriptOutcome::ResourceLimit(ResourceLimitKind::BaseBytes {
                actual: base_bytes.unwrap_or(0),
                limit: MAX_CLASSIC_SCRIPT_BASE_BYTES,
            }))
        } else if request.filename.is_empty() {
            Some(ClassicScriptOutcome::InvalidMetadata(InvalidMetadata::EmptyFilename))
        } else if request.filename.contains('\0') {
            Some(ClassicScriptOutcome::InvalidMetadata(InvalidMetadata::FilenameContainsNul))
        } else if request.base_url.is_some_and(|base| base.contains('\0')) {
            Some(ClassicScriptOutcome::InvalidMetadata(InvalidMetadata::BaseContainsNul))
        } else {
            None
        };

        Self { source_bytes, filename_bytes, base_bytes, rejection }
    }
}

pub(crate) struct BrowserScriptAdmissionState {
    limits: ClassicScriptLimits,
    interrupt: Arc<AtomicBool>,
    deadline: Instant,
    started: Instant,
    metadata: AdmissionMetadata,
    opcode_counts: [u64; OpCode::COUNT],
    opcodes_executed: u64,
    managed_allocation_bytes: usize,
    jobs_executed: u64,
    maximum_recursion_depth: usize,
    termination: Option<PolicyTermination>,
}

impl BrowserScriptAdmissionState {
    fn new(
        limits: ClassicScriptLimits,
        control: AdmissionControl,
        metadata: AdmissionMetadata,
    ) -> Self {
        let started = Instant::now();
        let relative_deadline = started.checked_add(limits.wall_time).unwrap_or(started);
        let deadline = control
            .absolute_deadline
            .map_or(relative_deadline, |deadline| deadline.min(relative_deadline));
        Self {
            limits,
            interrupt: control.interrupt,
            deadline,
            started,
            metadata,
            opcode_counts: [0; OpCode::COUNT],
            opcodes_executed: 0,
            managed_allocation_bytes: 0,
            jobs_executed: 0,
            maximum_recursion_depth: 0,
            termination: None,
        }
    }

    fn check_interrupt(&mut self) {
        if self.interrupt.load(Ordering::Acquire) {
            self.terminate(PolicyTermination::Interrupted(InterruptReason::ExternalRequest));
        }
        if Instant::now() >= self.deadline {
            self.terminate(PolicyTermination::Interrupted(InterruptReason::Deadline));
        }
    }

    fn terminate(&mut self, termination: PolicyTermination) -> ! {
        if self.termination.is_none() {
            self.termination = Some(termination);
        }
        panic_any(BrowserPolicyUnwind)
    }

    fn finish(self, pending_jobs_at_exit: usize) -> ClassicScriptReport {
        ClassicScriptReport {
            source_bytes: self.metadata.source_bytes,
            filename_bytes: self.metadata.filename_bytes,
            base_bytes: self.metadata.base_bytes,
            opcodes_executed: self.opcodes_executed,
            opcode_counts: self.opcode_counts,
            managed_allocation_bytes: self.managed_allocation_bytes,
            jobs_executed: self.jobs_executed,
            pending_jobs_at_exit,
            maximum_recursion_depth: self.maximum_recursion_depth,
            elapsed: self.started.elapsed(),
            jit_native_entries: 0,
            jit_side_exits: 0,
            jit_enabled: false,
        }
    }
}

struct BrowserAdmissionGuard {
    raw: Context,
}

#[derive(Clone, Copy)]
enum AdmissionKind {
    ClassicScript,
    MicrotaskCheckpoint,
}

#[derive(Clone, Copy)]
enum AdmissionInstallError {
    Busy,
    Poisoned,
}

impl BrowserAdmissionGuard {
    fn install(
        mut raw: Context,
        limits: ClassicScriptLimits,
        control: AdmissionControl,
        metadata: AdmissionMetadata,
        kind: AdmissionKind,
    ) -> Result<Self, AdmissionInstallError> {
        if raw.owner_execution_is_poisoned() {
            return Err(AdmissionInstallError::Poisoned);
        }
        if raw.browser_script_admission.is_some()
            || !raw.vm().browser_script_is_idle()
            || (matches!(kind, AdmissionKind::ClassicScript)
                && !raw.task_queue().browser_script_is_empty())
        {
            return Err(AdmissionInstallError::Busy);
        }
        raw.browser_script_admission =
            Some(BrowserScriptAdmissionState::new(limits, control, metadata));
        Ok(Self { raw })
    }

    fn termination(&mut self) -> Option<PolicyTermination> {
        self.raw.browser_script_admission.as_ref()?.termination
    }

    fn finish(mut self) -> ClassicScriptReport {
        let pending_jobs_at_exit = self.raw.task_queue().browser_script_len();
        self.raw
            .browser_script_admission
            .take()
            .unwrap_or_else(|| std::process::abort())
            .finish(pending_jobs_at_exit)
    }

    /// Finish scalar admission evidence after an unexpected panic without inspecting task, VM,
    /// or GC state. The context poison remains set after this guard is consumed.
    fn finish_without_task_queue(mut self) -> ClassicScriptReport {
        self.raw
            .take_browser_script_admission_for_poison_cleanup()
            .unwrap_or_else(|| std::process::abort())
            .finish(0)
    }
}

impl Drop for BrowserAdmissionGuard {
    fn drop(&mut self) {
        let _ = self.raw.take_browser_script_admission_for_poison_cleanup();
    }
}

#[derive(Clone, Copy)]
struct BrowserPolicyUnwind;

#[derive(Clone, Copy)]
enum PolicyTermination {
    Interrupted(InterruptReason),
    Resource(ResourceLimitKind),
    Host(BrowserHostError),
}

impl PolicyTermination {
    fn into_outcome(self) -> ClassicScriptOutcome {
        match self {
            Self::Interrupted(reason) => ClassicScriptOutcome::Interrupted(reason),
            Self::Resource(limit) => ClassicScriptOutcome::ResourceLimit(limit),
            Self::Host(error) => ClassicScriptOutcome::HostFailure(error),
        }
    }

    fn into_checkpoint_outcome(self) -> MicrotaskCheckpointOutcome {
        match self {
            Self::Interrupted(reason) => MicrotaskCheckpointOutcome::Interrupted(reason),
            Self::Resource(limit) => MicrotaskCheckpointOutcome::ResourceLimit(limit),
            Self::Host(error) => MicrotaskCheckpointOutcome::HostFailure(error),
        }
    }
}

impl Context {
    fn browser_script_is_poisoned(&self) -> bool {
        self.owner_execution_is_poisoned()
    }

    fn poison_browser_script(mut self) {
        self.poison_owner_execution();
    }

    pub(crate) fn browser_script_is_active(&self) -> bool {
        self.browser_script_admission.is_some()
    }

    pub(crate) fn browser_script_poll_phase(mut self) {
        self.browser_script_check_pending_task_overflow();
        if let Some(state) = self.browser_script_admission.as_mut() {
            state.check_interrupt();
        }
    }

    pub(crate) fn browser_script_poll_opcode(mut self, opcode: OpCode) {
        self.browser_script_check_pending_task_overflow();
        #[cfg(test)]
        TEST_PANIC_AFTER_PENDING_TASK.with(|slot| {
            if slot.get() && self.task_queue().browser_script_len() != 0 {
                slot.set(false);
                panic!("injected browser-script panic with pending work");
            }
        });
        let Some(state) = self.browser_script_admission.as_mut() else {
            return;
        };
        state.check_interrupt();
        if state.opcodes_executed >= state.limits.max_opcodes {
            state.terminate(PolicyTermination::Resource(ResourceLimitKind::Opcodes {
                limit: state.limits.max_opcodes,
            }));
        }
        state.opcodes_executed += 1;
        state.opcode_counts[opcode as usize] += 1;

        #[cfg(test)]
        TEST_PANIC_AFTER_OPCODE.with(|slot| {
            if slot
                .get()
                .is_some_and(|target| target == state.opcodes_executed)
            {
                slot.set(None);
                panic!("injected browser-script interpreter panic");
            }
        });
    }

    pub(crate) fn browser_script_before_managed_allocation(mut self, bytes: usize) {
        self.browser_script_check_pending_task_overflow();
        let Some(state) = self.browser_script_admission.as_mut() else {
            return;
        };
        state.check_interrupt();
        let requested_total = checked_add_usize_or_max(state.managed_allocation_bytes, bytes);
        if requested_total > state.limits.max_managed_allocation_bytes {
            state.terminate(PolicyTermination::Resource(
                ResourceLimitKind::ManagedAllocationBytes {
                    requested_total,
                    limit: state.limits.max_managed_allocation_bytes,
                },
            ));
        }
        state.managed_allocation_bytes = requested_total;
    }

    pub(crate) fn browser_script_before_frame(mut self, requested_depth: usize) {
        self.browser_script_check_pending_task_overflow();
        let Some(state) = self.browser_script_admission.as_mut() else {
            return;
        };
        state.check_interrupt();
        if requested_depth > state.limits.max_recursion_depth {
            state.terminate(PolicyTermination::Resource(ResourceLimitKind::RecursionDepth {
                requested_depth,
                limit: state.limits.max_recursion_depth,
            }));
        }
        state.maximum_recursion_depth = state.maximum_recursion_depth.max(requested_depth);
    }

    pub(crate) fn browser_script_before_job(mut self) {
        self.browser_script_check_pending_task_overflow();
        let Some(state) = self.browser_script_admission.as_mut() else {
            return;
        };
        state.check_interrupt();
        if state.jobs_executed >= state.limits.max_jobs {
            state.terminate(PolicyTermination::Resource(ResourceLimitKind::Jobs {
                limit: state.limits.max_jobs,
            }));
        }
        state.jobs_executed += 1;
    }

    fn browser_script_check_pending_task_overflow(mut self) {
        let Some(limit) = self.task_queue().browser_pending_cap_overflow() else {
            return;
        };
        let Some(state) = self.browser_script_admission.as_mut() else {
            return;
        };
        let limit = u64::try_from(limit).unwrap_or_else(|_| std::process::abort());
        state.terminate(PolicyTermination::Resource(ResourceLimitKind::PendingJobs { limit }));
    }

    pub(crate) fn browser_host_terminate(mut self, error: BrowserHostError) -> ! {
        let Some(state) = self.browser_script_admission.as_mut() else {
            std::process::abort();
        };
        state.terminate(PolicyTermination::Host(error))
    }

    pub(crate) fn clear_browser_script_tasks(mut self) {
        self.task_queue().clear_browser_script_tasks();
    }

    fn browser_script_pending_job_count(mut self) -> usize {
        self.task_queue().browser_script_len()
    }

    fn install_browser_pending_task_cap(
        mut self,
        limit: usize,
    ) -> Result<(), crate::runtime::tasks::BrowserPendingTaskCapInstallError> {
        self.task_queue().install_browser_pending_cap(limit)
    }

    fn browser_pending_task_cap_overflow(mut self) -> Option<usize> {
        self.task_queue().browser_pending_cap_overflow()
    }

    fn retire_browser_pending_task_cap(
        mut self,
        expected_limit: usize,
        poll: impl FnMut(),
    ) -> crate::runtime::tasks::BrowserPendingTaskRetirement {
        self.task_queue()
            .retire_browser_pending_cap(expected_limit, poll)
    }

    fn retire_foreign_browser_tasks_bounded(
        mut self,
        hard_limit: usize,
        poll: impl FnMut(),
    ) -> Option<usize> {
        self.task_queue()
            .retire_foreign_browser_tasks_bounded(hard_limit, poll)
    }

    fn browser_script_document_session_is_idle(mut self, installed_host_expected: bool) -> bool {
        self.browser_script_admission.is_none()
            && self.vm().browser_script_is_idle()
            && self.browser_host.is_active() == installed_host_expected
    }

    fn require_browser_script_idle_or_abort(mut self) {
        if !self.vm().browser_script_is_idle() {
            std::process::abort();
        }
    }
}

fn execute_classic_inner(
    mut cx: Context,
    request: ClassicScriptRequest<'_>,
    diagnostics: DiagnosticPolicy,
) -> ClassicScriptOutcome {
    cx.browser_script_poll_phase();
    let source =
        match Source::new_for_string(request.filename, Wtf8String::from_str(request.source)) {
            Ok(source) => Rc::new(source),
            Err(error) => return parse_error_outcome(&error, diagnostics),
        };

    cx.browser_script_poll_phase();
    let parse_context = ParseContext::new(source);
    let parsed = match parse_script(&parse_context, cx.options.clone()) {
        Ok(parsed) => parsed,
        Err(error) => return parse_error_outcome(&error, diagnostics),
    };

    cx.browser_script_poll_phase();
    let analyzed = match analyze(parsed) {
        Ok(analyzed) => analyzed,
        Err(errors) => return analyze_error_outcome(&errors, diagnostics),
    };

    cx.browser_script_poll_phase();
    let bytecode = match BytecodeProgramGenerator::generate_from_parse_script_result(
        cx,
        &analyzed,
        cx.initial_realm(),
    ) {
        Ok(bytecode) => bytecode,
        Err(error) => return compile_error_outcome(&error, diagnostics),
    };

    cx.browser_script_poll_phase();
    let evaluation = cx.with_initial_realm_stack_frame(cx.initial_realm_ptr(), |mut realm_cx| {
        realm_cx.vm().execute_script(bytecode)
    });
    cx.browser_script_poll_phase();
    summarize_evaluation(evaluation)
}

fn summarize_evaluation(result: Result<Handle<Value>, EvalError>) -> ClassicScriptOutcome {
    match result {
        Ok(value) => ClassicScriptOutcome::Success(summarize_value(value)),
        Err(EvalError::Value(value)) => ClassicScriptOutcome::Thrown(summarize_value(value)),
        #[cfg(feature = "alloc_error")]
        Err(EvalError::Alloc(_)) => {
            ClassicScriptOutcome::ResourceLimit(ResourceLimitKind::EngineAllocation)
        }
    }
}

fn summarize_checkpoint_error(error: EvalError) -> MicrotaskCheckpointOutcome {
    match error {
        EvalError::Value(value) => MicrotaskCheckpointOutcome::JobThrown(summarize_value(value)),
        #[cfg(feature = "alloc_error")]
        EvalError::Alloc(_) => {
            MicrotaskCheckpointOutcome::ResourceLimit(ResourceLimitKind::EngineAllocation)
        }
    }
}

fn summarize_value(value: Handle<Value>) -> ScriptValueSummary {
    if value.is_undefined() {
        ScriptValueSummary::Undefined
    } else if value.is_null() {
        ScriptValueSummary::Null
    } else if value.is_empty() {
        ScriptValueSummary::Empty
    } else if value.is_bool() {
        ScriptValueSummary::Boolean(value.as_bool())
    } else if value.is_number() {
        ScriptValueSummary::Number(value.as_number())
    } else if value.is_string() {
        ScriptValueSummary::String { code_units: value.as_string().len() }
    } else if value.is_bigint() {
        ScriptValueSummary::BigInt
    } else if value.is_symbol() {
        ScriptValueSummary::Symbol
    } else if value.is_object() {
        ScriptValueSummary::Object
    } else {
        ScriptValueSummary::UnsupportedHeapValue
    }
}

fn parse_error_outcome(
    error: &LocalizedParseError,
    policy: DiagnosticPolicy,
) -> ClassicScriptOutcome {
    match diagnostics_from_errors(std::slice::from_ref(error), policy) {
        Ok(diagnostics) => ClassicScriptOutcome::ParseError(diagnostics),
        Err(limit) => ClassicScriptOutcome::ResourceLimit(limit),
    }
}

fn analyze_error_outcome(
    errors: &LocalizedParseErrors,
    policy: DiagnosticPolicy,
) -> ClassicScriptOutcome {
    match diagnostics_from_errors(&errors.errors, policy) {
        Ok(diagnostics) => ClassicScriptOutcome::AnalyzeError(diagnostics),
        Err(limit) => ClassicScriptOutcome::ResourceLimit(limit),
    }
}

fn compile_error_outcome(error: &EmitError, policy: DiagnosticPolicy) -> ClassicScriptOutcome {
    if matches!(error, EmitError::Alloc(_)) {
        return ClassicScriptOutcome::ResourceLimit(ResourceLimitKind::EngineAllocation);
    }
    if let DiagnosticPolicy::Document { used, limit } = policy {
        let requested_total = checked_add_usize_or_max(used, 1);
        if requested_total > limit {
            return ClassicScriptOutcome::ResourceLimit(ResourceLimitKind::Diagnostics {
                requested_total,
                limit,
            });
        }
    }
    match bounded_display(error, MAX_DIAGNOSTIC_BYTES) {
        Ok(message) => ClassicScriptOutcome::CompileError(ScriptDiagnostic {
            message,
            start_byte: None,
            end_byte: None,
        }),
        Err(limit) => ClassicScriptOutcome::ResourceLimit(limit),
    }
}

fn diagnostics_from_errors(
    errors: &[LocalizedParseError],
    policy: DiagnosticPolicy,
) -> Result<Vec<ScriptDiagnostic>, ResourceLimitKind> {
    let count = errors.len().min(MAX_DIAGNOSTICS);
    if let DiagnosticPolicy::Document { used, limit } = policy {
        let requested_total = checked_add_usize_or_max(used, count);
        if requested_total > limit {
            return Err(ResourceLimitKind::Diagnostics { requested_total, limit });
        }
    }
    let mut diagnostics = Vec::new();
    diagnostics
        .try_reserve_exact(count)
        .map_err(|_| ResourceLimitKind::HostDiagnosticAllocation)?;
    let mut remaining_bytes = MAX_DIAGNOSTIC_BYTES;

    for error in errors.iter().take(count) {
        let message = bounded_display(&error.error, remaining_bytes)?;
        remaining_bytes -= message.len();
        let (start_byte, end_byte) = error
            .source_loc
            .as_ref()
            .map_or((None, None), |(loc, _)| (Some(loc.start), Some(loc.end)));
        diagnostics.push(ScriptDiagnostic { message, start_byte, end_byte });
    }

    Ok(diagnostics)
}

fn bounded_display(value: &impl Display, max_bytes: usize) -> Result<String, ResourceLimitKind> {
    let mut writer = FallibleBoundedWriter::new(max_bytes);
    if write!(&mut writer, "{value}").is_err() {
        return Err(if writer.allocation_failed {
            ResourceLimitKind::HostDiagnosticAllocation
        } else {
            ResourceLimitKind::DiagnosticBytes { limit: MAX_DIAGNOSTIC_BYTES }
        });
    }
    Ok(writer.text)
}

struct FallibleBoundedWriter {
    text: String,
    max_bytes: usize,
    allocation_failed: bool,
}

impl FallibleBoundedWriter {
    fn new(max_bytes: usize) -> Self {
        Self { text: String::new(), max_bytes, allocation_failed: false }
    }
}

impl Write for FallibleBoundedWriter {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        let Some(next_len) = self.text.len().checked_add(value.len()) else {
            return Err(fmt::Error);
        };
        if next_len > self.max_bytes {
            return Err(fmt::Error);
        }
        if self.text.try_reserve(value.len()).is_err() {
            self.allocation_failed = true;
            return Err(fmt::Error);
        }
        self.text.push_str(value);
        Ok(())
    }
}

#[cfg(test)]
thread_local! {
    static TEST_PANIC_AFTER_OPCODE: std::cell::Cell<Option<u64>> = const {
        std::cell::Cell::new(None)
    };
    static TEST_PANIC_AFTER_PENDING_TASK: std::cell::Cell<bool> = const {
        std::cell::Cell::new(false)
    };
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc, thread, time::Duration};

    use crate::{
        common::options::OptionsBuilder,
        runtime::{
            BrowserHostCommitOutcome, BrowserHostDocumentVersion, BrowserHostNodeToken,
            ContextBuilder, bytecode::instruction::OpCode, gc::HandleScopeGuard,
            property::Property, property_key::PropertyKey,
        },
    };

    use super::*;

    #[cfg(feature = "alloc_error")]
    static FIXED_HEAP_OOM_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[cfg(feature = "alloc_error")]
    fn serialize_fixed_heap_oom_test() -> std::sync::MutexGuard<'static, ()> {
        FIXED_HEAP_OOM_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn context() -> OwnedContext {
        let options = OptionsBuilder::new().serialized_heap(None).build().unwrap();
        ContextBuilder::new()
            .set_options(Rc::new(options))
            .build()
            .unwrap()
    }

    fn context_with_exposed_gc() -> OwnedContext {
        let options = OptionsBuilder::new()
            .serialized_heap(None)
            .expose_gc(true)
            .build()
            .unwrap();
        ContextBuilder::new()
            .set_options(Rc::new(options))
            .build()
            .unwrap()
    }

    #[derive(Default)]
    struct NoopDocumentHost {
        finished_phases: u32,
        aborted_phases: u32,
    }

    impl BrowserHostTask for NoopDocumentHost {
        fn validate_phase(&mut self) -> Result<(), BrowserHostError> {
            Ok(())
        }

        fn document_node(&mut self) -> Result<BrowserHostNodeToken, BrowserHostError> {
            Err(BrowserHostError::InvalidOperation)
        }

        fn lookup_node(&mut self, _slot: u32) -> Result<BrowserHostNodeToken, BrowserHostError> {
            Err(BrowserHostError::InvalidOperation)
        }

        fn create_html_element(
            &mut self,
            _local_name: &str,
        ) -> Result<BrowserHostNodeToken, BrowserHostError> {
            Err(BrowserHostError::InvalidOperation)
        }

        fn create_text(&mut self, _data: &str) -> Result<BrowserHostNodeToken, BrowserHostError> {
            Err(BrowserHostError::InvalidOperation)
        }

        fn append_child(
            &mut self,
            _parent: BrowserHostNodeToken,
            _child: BrowserHostNodeToken,
        ) -> Result<(), BrowserHostError> {
            Err(BrowserHostError::InvalidOperation)
        }

        fn set_html_attribute(
            &mut self,
            _element: BrowserHostNodeToken,
            _local_name: &str,
            _value: &str,
        ) -> Result<(), BrowserHostError> {
            Err(BrowserHostError::InvalidOperation)
        }

        fn set_character_data(
            &mut self,
            _node: BrowserHostNodeToken,
            _data: &str,
        ) -> Result<(), BrowserHostError> {
            Err(BrowserHostError::InvalidOperation)
        }

        fn finish_phase(&mut self) -> Result<BrowserHostCommitOutcome, BrowserHostError> {
            self.finished_phases += 1;
            Ok(BrowserHostCommitOutcome::NoChanges(BrowserHostDocumentVersion::new(1, 0)))
        }

        fn abort_phase(&mut self) {
            self.aborted_phases += 1;
        }
    }

    #[cfg(feature = "alloc_error")]
    fn context_with_fixed_heap(bytes: usize) -> OwnedContext {
        let options = OptionsBuilder::new()
            .serialized_heap(None)
            .min_heap_size(bytes)
            .max_heap_size(bytes)
            .build()
            .unwrap();
        ContextBuilder::new()
            .set_options(Rc::new(options))
            .build()
            .unwrap()
    }

    fn limits(
        opcodes: u64,
        allocations: usize,
        recursion: usize,
        jobs: u64,
        wall_time: Duration,
    ) -> ClassicScriptLimits {
        ClassicScriptLimits::new(opcodes, allocations, recursion, jobs, wall_time).unwrap()
    }

    fn run(realm: &mut BrowserScriptRealm<'_>, source: &str) -> ClassicScriptExecution {
        realm.execute_classic(
            ClassicScriptRequest::new(source, "https://example.test/app.js")
                .with_base_url("https://example.test/"),
            ClassicScriptLimits::default(),
            &ScriptInterruptHandle::new(),
        )
    }

    fn owned_source(name: &str, contents: &str) -> Rc<Source> {
        Rc::new(Source::new_for_string(name, Wtf8String::from_str(contents)).unwrap())
    }

    fn assert_poisoned_owner_surfaces_are_sealed(cx: &mut OwnedContext, mut raw: Context) {
        let before = cx.poisoned_owner_diagnostics();
        assert!(before.0);

        let browser_callback_ran = Cell::new(false);
        assert!(
            catch_unwind(AssertUnwindSafe(|| {
                cx.with_browser_script_realm(|_| browser_callback_ran.set(true));
            }))
            .is_err()
        );
        assert!(!browser_callback_ran.get());

        let root_callback_ran = Cell::new(false);
        assert!(
            catch_unwind(AssertUnwindSafe(|| {
                cx.with_root_scope(|_| root_callback_ran.set(true));
            }))
            .is_err()
        );
        assert!(!root_callback_ran.get());

        assert!(
            catch_unwind(AssertUnwindSafe(|| {
                let _ = cx.evaluate_script(owned_source("poisoned-owner.js", "1 + 1;"));
            }))
            .is_err()
        );
        assert!(
            catch_unwind(AssertUnwindSafe(|| {
                let _ = cx.evaluate_module(owned_source("poisoned-owner.mjs", "export {};"));
            }))
            .is_err()
        );
        assert!(
            catch_unwind(AssertUnwindSafe(|| {
                let _ = cx.install_optional_globals();
            }))
            .is_err()
        );
        assert!(
            catch_unwind(AssertUnwindSafe(|| unsafe {
                let _ = cx.raw_context_unchecked();
            }))
            .is_err()
        );

        assert!(
            catch_unwind(AssertUnwindSafe(|| {
                let _ = raw.evaluate_script(owned_source("poisoned-raw.js", "2 + 2;"));
            }))
            .is_err()
        );
        assert!(
            catch_unwind(AssertUnwindSafe(|| {
                let _ = raw.evaluate_module(owned_source("poisoned-raw.mjs", "export {};"));
            }))
            .is_err()
        );
        assert!(
            catch_unwind(AssertUnwindSafe(|| {
                let _ = raw.run_all_tasks();
            }))
            .is_err()
        );
        assert!(
            catch_unwind(AssertUnwindSafe(|| {
                let _queue = raw.task_queue();
            }))
            .is_err()
        );
        assert!(
            catch_unwind(AssertUnwindSafe(|| {
                let _vm = raw.vm();
            }))
            .is_err()
        );
        assert!(
            catch_unwind(AssertUnwindSafe(|| {
                let _ = raw.alloc_string("must not allocate");
            }))
            .is_err()
        );
        assert!(
            catch_unwind(AssertUnwindSafe(|| {
                let _ = raw.init_builtin_names();
            }))
            .is_err()
        );
        assert!(
            catch_unwind(AssertUnwindSafe(|| {
                let _ = raw.init_builtin_symbols();
            }))
            .is_err()
        );
        assert!(
            catch_unwind(AssertUnwindSafe(|| {
                let _ = raw.initial_realm();
            }))
            .is_err()
        );
        assert!(
            catch_unwind(AssertUnwindSafe(|| {
                let _ = raw.global_symbol_registry_field();
            }))
            .is_err()
        );
        #[cfg(feature = "gc_stress_test")]
        {
            assert!(catch_unwind(AssertUnwindSafe(|| cx.enable_gc_stress_test())).is_err());
            assert!(catch_unwind(AssertUnwindSafe(|| raw.enable_gc_stress_test())).is_err());
        }
        #[cfg(feature = "baseline_jit")]
        {
            assert!(
                catch_unwind(AssertUnwindSafe(|| {
                    let _roots = raw.jit_dispatch_roots();
                }))
                .is_err()
            );
            assert!(
                catch_unwind(AssertUnwindSafe(|| {
                    let _dispatch = raw.jit_dispatch();
                }))
                .is_err()
            );
        }
        assert!(
            catch_unwind(AssertUnwindSafe(|| {
                raw.print_or_add_to_dump_buffer("must not publish output");
            }))
            .is_err()
        );

        assert_eq!(cx.poisoned_owner_diagnostics(), before);
    }

    #[test]
    fn classic_script_runs_objects_properties_functions_and_closures() {
        let mut cx = context();
        cx.with_browser_script_realm(|realm| {
            let execution = run(
                realm,
                "const object = { base: 40 };\n\
                 function outer(delta) { return function inner(value) {\n\
                   object.answer = object.base + delta + value; return object.answer;\n\
                 }; }\n\
                 if (outer(1)(1) !== 42) throw 'bad closure result';",
            );
            assert_eq!(
                execution.outcome,
                ClassicScriptOutcome::Success(ScriptValueSummary::Undefined)
            );
            assert!(execution.report.opcodes_executed() > 0);
            assert!(execution.report.opcode_count(OpCode::Ret) > 0);
            assert_eq!(execution.report.jit_native_entries, 0);
            assert_eq!(execution.report.jit_side_exits, 0);
            assert!(!execution.report.jit_enabled);
        });
    }

    #[test]
    fn parse_throw_and_repeated_realm_results_are_typed_and_reusable() {
        let mut cx = context();
        cx.with_browser_script_realm(|realm| {
            assert!(matches!(
                run(realm, "function (").outcome,
                ClassicScriptOutcome::ParseError(_)
            ));
            assert_eq!(
                run(realm, "throw 73;").outcome,
                ClassicScriptOutcome::Thrown(ScriptValueSummary::Number(73.0))
            );
            assert_eq!(
                run(realm, "globalThis.persisted = 39;").outcome,
                ClassicScriptOutcome::Success(ScriptValueSummary::Undefined)
            );
            assert_eq!(
                run(realm, "if (persisted + 3 !== 42) throw 'realm was not reused';").outcome,
                ClassicScriptOutcome::Success(ScriptValueSummary::Undefined)
            );
        });
    }

    #[test]
    fn promise_jobs_have_a_bounded_checkpoint_and_reuse_the_same_realm() {
        let mut cx = context();
        cx.with_browser_script_realm(|realm| {
            let first = run(
                realm,
                "globalThis.jobResult = 0;\n\
                 Promise.resolve(21).then(value => { jobResult = value * 2; });\n\
                 throw 17;",
            );
            assert_eq!(
                first.outcome,
                ClassicScriptOutcome::Thrown(ScriptValueSummary::Number(17.0))
            );
            assert_eq!(first.report.jobs_executed(), 0);
            assert!(first.report.pending_jobs_at_exit() >= 1);
            assert_eq!(
                run(realm, "throw 'must not run before checkpoint';").outcome,
                ClassicScriptOutcome::RuntimeBusy
            );

            let checkpoint = realm.perform_microtask_checkpoint(
                ClassicScriptLimits::default(),
                &ScriptInterruptHandle::new(),
            );
            assert_eq!(checkpoint.outcome, MicrotaskCheckpointOutcome::Complete);
            assert!(checkpoint.report.jobs_executed() >= 1);
            assert_eq!(checkpoint.report.pending_jobs_at_exit(), 0);
            assert_eq!(
                run(realm, "if (jobResult !== 42) throw 'job checkpoint did not run';").outcome,
                ClassicScriptOutcome::Success(ScriptValueSummary::Undefined)
            );
        });
    }

    #[test]
    fn thrown_cleanup_job_is_summarized_inside_its_live_handle_scope() {
        let mut cx = context_with_exposed_gc();
        cx.with_browser_script_realm(|realm| {
            let scheduled = run(
                realm,
                "globalThis.cleanupRegistry = new FinalizationRegistry(() => {\n\
                     throw 'cleanup exploded';\n\
                 });\n\
                 (() => {\n\
                     const target = {};\n\
                     cleanupRegistry.register(target, 'held');\n\
                 })();\n\
                 gc.run();",
            );
            assert_eq!(
                scheduled.outcome,
                ClassicScriptOutcome::Success(ScriptValueSummary::Undefined)
            );
            assert!(scheduled.report.pending_jobs_at_exit() >= 1);

            let checkpoint = realm.perform_microtask_checkpoint(
                ClassicScriptLimits::default(),
                &ScriptInterruptHandle::new(),
            );
            assert_eq!(
                checkpoint.outcome,
                MicrotaskCheckpointOutcome::JobThrown(ScriptValueSummary::String {
                    code_units: 16,
                })
            );
            assert_eq!(checkpoint.report.pending_jobs_at_exit(), 0);
            assert_eq!(
                run(realm, "if (6 * 7 !== 42) throw 'job cleanup reuse failed';").outcome,
                ClassicScriptOutcome::Success(ScriptValueSummary::Undefined)
            );
        });
    }

    #[test]
    fn external_deadline_opcode_allocation_recursion_and_job_limits_are_distinct() {
        let mut cx = context();
        cx.with_browser_script_realm(|realm| {
            let interrupted = ScriptInterruptHandle::new();
            let requester = interrupted.clone();
            let request_thread = thread::spawn(move || requester.request_interrupt());
            let external = realm.execute_classic(
                ClassicScriptRequest::new("while (true) {}", "external.js"),
                limits(1_000_000, 1024 * 1024, 16, 16, Duration::from_secs(1)),
                &interrupted,
            );
            request_thread.join().unwrap();
            assert_eq!(
                external.outcome,
                ClassicScriptOutcome::Interrupted(InterruptReason::ExternalRequest)
            );

            let deadline = realm.execute_classic(
                ClassicScriptRequest::new("1;", "deadline.js"),
                limits(100, 1024 * 1024, 16, 16, Duration::ZERO),
                &ScriptInterruptHandle::new(),
            );
            assert_eq!(
                deadline.outcome,
                ClassicScriptOutcome::Interrupted(InterruptReason::Deadline)
            );

            let opcode = realm.execute_classic(
                ClassicScriptRequest::new("let i = 0; while (true) i++;", "opcode.js"),
                limits(32, 1024 * 1024, 16, 16, Duration::from_secs(1)),
                &ScriptInterruptHandle::new(),
            );
            assert_eq!(
                opcode.outcome,
                ClassicScriptOutcome::ResourceLimit(ResourceLimitKind::Opcodes { limit: 32 })
            );

            let allocation = realm.execute_classic(
                ClassicScriptRequest::new("({ value: 1 });", "allocation.js"),
                limits(10_000, 1, 16, 16, Duration::from_secs(1)),
                &ScriptInterruptHandle::new(),
            );
            assert!(matches!(
                allocation.outcome,
                ClassicScriptOutcome::ResourceLimit(ResourceLimitKind::ManagedAllocationBytes {
                    limit: 1,
                    ..
                })
            ));

            let recursion = realm.execute_classic(
                ClassicScriptRequest::new(
                    "function recurse() { return recurse(); } recurse();",
                    "recursion.js",
                ),
                limits(10_000, 1024 * 1024, 2, 16, Duration::from_secs(1)),
                &ScriptInterruptHandle::new(),
            );
            assert!(matches!(
                recursion.outcome,
                ClassicScriptOutcome::ResourceLimit(ResourceLimitKind::RecursionDepth {
                    limit: 2,
                    ..
                })
            ));

            let jobs_script = realm.execute_classic(
                ClassicScriptRequest::new(
                    "Promise.resolve().then(() => Promise.resolve()).then(() => 1);",
                    "jobs.js",
                ),
                ClassicScriptLimits::default(),
                &ScriptInterruptHandle::new(),
            );
            assert_eq!(
                jobs_script.outcome,
                ClassicScriptOutcome::Success(ScriptValueSummary::Undefined)
            );
            let jobs = realm.perform_microtask_checkpoint(
                limits(100_000, 4 * 1024 * 1024, 16, 1, Duration::from_secs(1)),
                &ScriptInterruptHandle::new(),
            );
            assert_eq!(
                jobs.outcome,
                MicrotaskCheckpointOutcome::ResourceLimit(ResourceLimitKind::Jobs { limit: 1 })
            );

            assert_eq!(
                run(realm, "if (6 * 7 !== 42) throw 'context reuse failed';").outcome,
                ClassicScriptOutcome::Success(ScriptValueSummary::Undefined)
            );
        });
    }

    #[test]
    fn unexpected_interpreter_panic_permanently_poisons_browser_admission() {
        let mut cx = context();
        cx.with_browser_script_realm(|realm| {
            TEST_PANIC_AFTER_OPCODE.with(|slot| slot.set(Some(3)));
            let panic = run(realm, "let value = 1; value += 2; value;");
            assert_eq!(panic.outcome, ClassicScriptOutcome::EnginePanic);
            assert_eq!(panic.report.pending_jobs_at_exit(), 0);

            let rejected = run(realm, "throw 'poisoned context executed';");
            assert_eq!(rejected.outcome, ClassicScriptOutcome::RuntimePoisoned);
            assert_eq!(rejected.report.opcodes_executed(), 0);
            assert_eq!(rejected.report.managed_allocation_bytes(), 0);
            assert_eq!(rejected.report.jobs_executed(), 0);

            let checkpoint = realm.perform_microtask_checkpoint(
                ClassicScriptLimits::default(),
                &ScriptInterruptHandle::new(),
            );
            assert_eq!(checkpoint.outcome, MicrotaskCheckpointOutcome::RuntimePoisoned);
            assert_eq!(checkpoint.report.opcodes_executed(), 0);
            assert_eq!(checkpoint.report.managed_allocation_bytes(), 0);
            assert_eq!(checkpoint.report.jobs_executed(), 0);
        });
    }

    #[cfg(feature = "alloc_error")]
    #[test]
    fn engine_allocation_failure_discards_queued_jobs_and_allows_fresh_admission() {
        // These tests deliberately saturate separate fixed 8 MiB heaps. Running both saturation
        // loops concurrently can exhaust their wall-time policy before either reaches the intended
        // engine-allocation failure, so serialize only this resource-hostile pair.
        let _oom_test = serialize_fixed_heap_oom_test();
        const HEAP_BYTES: usize = 8 * 1024 * 1024;
        let mut cx = context_with_fixed_heap(HEAP_BYTES);
        cx.with_browser_script_realm(|realm| {
            let failed = realm.execute_classic(
                ClassicScriptRequest::new(
                    "globalThis.jobRan = false;\n\
                     Promise.resolve().then(() => { jobRan = true; });\n\
                     globalThis.a = new Array(200000).fill(1);\n\
                     globalThis.b = new Array(200000).fill(2);\n\
                     globalThis.c = new Array(200000).fill(3);",
                    "oom.js",
                ),
                limits(1_000_000, 256 * 1024 * 1024, 64, 64, Duration::from_secs(30)),
                &ScriptInterruptHandle::new(),
            );
            assert_eq!(
                failed.outcome,
                ClassicScriptOutcome::ResourceLimit(ResourceLimitKind::EngineAllocation)
            );
            assert_eq!(failed.report.pending_jobs_at_exit(), 0);

            let checkpoint = realm.perform_microtask_checkpoint(
                ClassicScriptLimits::default(),
                &ScriptInterruptHandle::new(),
            );
            assert_eq!(checkpoint.outcome, MicrotaskCheckpointOutcome::Complete);
            assert_eq!(checkpoint.report.jobs_executed(), 0);
            assert_eq!(
                run(realm, "if (jobRan !== false) throw 'job survived engine allocation failure';")
                    .outcome,
                ClassicScriptOutcome::Success(ScriptValueSummary::Undefined)
            );
        });
    }

    #[cfg(feature = "alloc_error")]
    #[test]
    fn document_engine_oom_is_terminal_for_task_but_context_remains_recoverable() {
        let _oom_test = serialize_fixed_heap_oom_test();
        const HEAP_BYTES: usize = 8 * 1024 * 1024;
        let mut cx = context_with_fixed_heap(HEAP_BYTES);
        cx.with_browser_script_realm(|realm| {
            let limits =
                ClassicScriptLimits::parser_blocking_document(Duration::from_secs(30)).unwrap();
            let completion =
                realm.with_document_script_budget(limits, &ScriptInterruptHandle::new(), |realm| {
                    let failed = realm.execute_document_classic(ClassicScriptRequest::new(
                        "globalThis.documentJobRan = false;\n\
                         Promise.resolve().then(() => { documentJobRan = true; });\n\
                         globalThis.documentA = new Array(200000).fill(1);\n\
                         globalThis.documentB = new Array(200000).fill(2);\n\
                         globalThis.documentC = new Array(200000).fill(3);",
                        "document-oom.js",
                    ));
                    assert_eq!(
                        failed.outcome,
                        ClassicScriptOutcome::ResourceLimit(ResourceLimitKind::EngineAllocation)
                    );
                    assert_eq!(failed.report.pending_jobs_at_exit(), 0);
                    let repeated = realm.perform_document_microtask_checkpoint();
                    assert_eq!(
                        repeated.outcome,
                        MicrotaskCheckpointOutcome::ResourceLimit(
                            ResourceLimitKind::EngineAllocation
                        )
                    );
                    assert_eq!(repeated.report.jobs_executed(), 0);
                });
            assert_eq!(
                completion.unwrap_err(),
                ClassicScriptOutcome::ResourceLimit(ResourceLimitKind::EngineAllocation)
            );

            let recovered = run(
                realm,
                "if (documentJobRan !== false) throw 'OOM job survived';\n\
                 if (6 * 7 !== 42) throw 'context did not recover';",
            );
            assert!(matches!(recovered.outcome, ClassicScriptOutcome::Success(_)));
        });
    }

    #[cfg(feature = "alloc_error")]
    #[test]
    fn bytecode_generator_allocation_failure_is_a_resource_outcome() {
        let outcome = compile_error_outcome(
            &EmitError::Alloc(crate::runtime::alloc_error::AllocError::oom()),
            DiagnosticPolicy::PerAdmission,
        );
        assert_eq!(
            outcome,
            ClassicScriptOutcome::ResourceLimit(ResourceLimitKind::EngineAllocation)
        );
    }

    #[test]
    fn source_identity_metadata_is_bounded_before_runtime_admission() {
        let mut cx = context();
        let oversized_filename = "x".repeat(MAX_CLASSIC_SCRIPT_FILENAME_BYTES + 1);
        cx.with_browser_script_realm(|realm| {
            let outcome = realm.execute_classic(
                ClassicScriptRequest::new("1;", &oversized_filename),
                ClassicScriptLimits::default(),
                &ScriptInterruptHandle::new(),
            );
            assert_eq!(
                outcome.outcome,
                ClassicScriptOutcome::ResourceLimit(ResourceLimitKind::FilenameBytes {
                    actual: MAX_CLASSIC_SCRIPT_FILENAME_BYTES + 1,
                    limit: MAX_CLASSIC_SCRIPT_FILENAME_BYTES,
                })
            );
            assert_eq!(outcome.report.opcodes_executed(), 0);
        });
    }

    #[cfg(feature = "gc_stress_test")]
    #[test]
    fn admission_survives_forced_moving_gc_and_reuses_realm() {
        let mut cx = context();
        cx.enable_gc_stress_test();
        cx.with_browser_script_realm(|realm| {
            let result = run(
                realm,
                "let sum = 0; for (let i = 0; i < 50; i++) {\n\
                   const value = { i, text: 'moving-' + i }; sum += value.i;\n\
                 } if (sum !== 1225) throw 'moving GC corrupted state';",
            );
            assert_eq!(
                result.outcome,
                ClassicScriptOutcome::Success(ScriptValueSummary::Undefined)
            );
            assert_eq!(
                run(realm, "if (sum + 1 !== 1226) throw 'realm was not reused';").outcome,
                ClassicScriptOutcome::Success(ScriptValueSummary::Undefined)
            );
        });
    }

    #[test]
    fn document_budget_reuses_one_realm_preserves_checkpoints_and_seals_direct_entry() {
        let mut cx = context();
        let interrupt = ScriptInterruptHandle::new();
        let limits =
            ClassicScriptLimits::new(100_000, 8 * 1024 * 1024, 64, 16, Duration::from_secs(2))
                .unwrap();

        cx.with_browser_script_realm(|realm| {
            realm
                .with_document_script_budget(limits, &interrupt, |realm| {
                    let pre = realm.perform_document_microtask_checkpoint();
                    assert_eq!(pre.outcome, MicrotaskCheckpointOutcome::Complete);
                    assert_eq!(realm.document_jobs_executed(), Some(0));

                    let nested_callback_invoked = std::cell::Cell::new(false);
                    let nested = realm.with_document_script_budget(
                        limits,
                        &ScriptInterruptHandle::new(),
                        |_| nested_callback_invoked.set(true),
                    );
                    assert!(!nested_callback_invoked.get());
                    assert_eq!(
                        nested.unwrap_err(),
                        ClassicScriptOutcome::InvalidDocumentSession(
                            LimitConfigurationError::DocumentBudgetAlreadyActive
                        )
                    );

                    let wrong_mode = realm.perform_hosted_document_microtask_checkpoint();
                    assert_eq!(
                        wrong_mode.checkpoint.outcome,
                        MicrotaskCheckpointOutcome::RuntimeBusy
                    );
                    assert_eq!(wrong_mode.host, BrowserHostPhaseOutcome::NotStarted);

                    let bypass = realm.execute_classic(
                        ClassicScriptRequest::new("throw 'bypass';", "bypass.js"),
                        ClassicScriptLimits::default(),
                        &ScriptInterruptHandle::new(),
                    );
                    assert_eq!(bypass.outcome, ClassicScriptOutcome::RuntimeBusy);

                    let first = realm.execute_document_classic(ClassicScriptRequest::new(
                        "globalThis.documentOrder = 'script-1';\n\
                         Promise.resolve().then(() => { documentOrder += ':post-1'; });",
                        "inline-1.js",
                    ));
                    assert!(matches!(first.outcome, ClassicScriptOutcome::Success(_)));
                    assert!(first.report.pending_jobs_at_exit() >= 1);
                    let opcodes_after_first = first.report.opcodes_executed();
                    assert_eq!(realm.document_opcodes_executed(), Some(opcodes_after_first));

                    let post_first = realm.perform_document_microtask_checkpoint();
                    assert_eq!(post_first.outcome, MicrotaskCheckpointOutcome::Complete);
                    assert!(post_first.report.jobs_executed() >= 1);
                    let opcodes_after_post = opcodes_after_first
                        .checked_add(post_first.report.opcodes_executed())
                        .unwrap();
                    assert_eq!(realm.document_opcodes_executed(), Some(opcodes_after_post));

                    let pre_second = realm.perform_document_microtask_checkpoint();
                    assert_eq!(pre_second.outcome, MicrotaskCheckpointOutcome::Complete);
                    assert_eq!(pre_second.report.jobs_executed(), 0);

                    let second = realm.execute_document_classic(ClassicScriptRequest::new(
                        "if (documentOrder !== 'script-1:post-1') throw 'wrong checkpoint order';\n\
                         documentOrder += ':script-2';",
                        "inline-2.js",
                    ));
                    assert!(matches!(second.outcome, ClassicScriptOutcome::Success(_)));
                    let post_second = realm.perform_document_microtask_checkpoint();
                    assert_eq!(post_second.outcome, MicrotaskCheckpointOutcome::Complete);
                    assert_eq!(realm.document_script_candidates(), Some(2));
                    assert_eq!(
                        realm.document_source_bytes(),
                        Some(first.report.source_bytes() + second.report.source_bytes())
                    );
                    assert_eq!(realm.document_diagnostics_emitted(), Some(0));
                    assert_eq!(first.report.jit_native_entries, 0);
                    assert_eq!(second.report.jit_native_entries, 0);
                })
                .unwrap();
        });
    }

    #[test]
    fn document_candidate_source_and_diagnostic_caps_fail_typed_and_latch() {
        let limits =
            ClassicScriptLimits::new(100_000, 8 * 1024 * 1024, 64, 16, Duration::from_secs(2))
                .unwrap();

        let mut candidate_cx = context();
        candidate_cx.with_browser_script_realm(|realm| {
            let completion = realm.with_document_script_budget_caps(
                limits,
                &ScriptInterruptHandle::new(),
                DocumentScriptCaps { script_candidates: 1, source_bytes: 32, diagnostics: 4 },
                |realm| {
                    realm.account_skipped_document_script(1).unwrap();
                    let exhausted = realm.account_skipped_document_script(0).unwrap_err();
                    assert_eq!(
                        exhausted,
                        ClassicScriptOutcome::ResourceLimit(ResourceLimitKind::ScriptCandidates {
                            requested_total: 2,
                            limit: 1,
                        })
                    );
                    assert_eq!(realm.document_script_candidates(), Some(1));
                    assert_eq!(realm.account_skipped_document_script(0).unwrap_err(), exhausted);
                },
            );
            assert_eq!(
                completion.unwrap_err(),
                ClassicScriptOutcome::ResourceLimit(ResourceLimitKind::ScriptCandidates {
                    requested_total: 2,
                    limit: 1,
                })
            );
        });

        let mut source_cx = context();
        source_cx.with_browser_script_realm(|realm| {
            let completion = realm.with_document_script_budget_caps(
                limits,
                &ScriptInterruptHandle::new(),
                DocumentScriptCaps { script_candidates: 4, source_bytes: 2, diagnostics: 4 },
                |realm| {
                    realm.account_skipped_document_script(1).unwrap();
                    let exhausted = realm.execute_document_classic(ClassicScriptRequest::new(
                        "1;",
                        "source-limit.js",
                    ));
                    assert_eq!(
                        exhausted.outcome,
                        ClassicScriptOutcome::ResourceLimit(ResourceLimitKind::SourceBytes {
                            actual: 3,
                            limit: 2,
                        })
                    );
                    assert_eq!(exhausted.report.opcodes_executed(), 0);
                    assert_eq!(realm.document_source_bytes(), Some(1));
                },
            );
            assert_eq!(
                completion.unwrap_err(),
                ClassicScriptOutcome::ResourceLimit(ResourceLimitKind::SourceBytes {
                    actual: 3,
                    limit: 2,
                })
            );
        });

        let mut diagnostic_cx = context();
        diagnostic_cx.with_browser_script_realm(|realm| {
            let completion = realm.with_document_script_budget_caps(
                limits,
                &ScriptInterruptHandle::new(),
                DocumentScriptCaps { script_candidates: 4, source_bytes: 1024, diagnostics: 1 },
                |realm| {
                    let first = realm.execute_document_classic(ClassicScriptRequest::new(
                        "function (",
                        "diagnostic-1.js",
                    ));
                    assert!(matches!(first.outcome, ClassicScriptOutcome::ParseError(_)));
                    assert_eq!(realm.document_diagnostics_emitted(), Some(1));
                    assert_eq!(
                        realm.perform_document_microtask_checkpoint().outcome,
                        MicrotaskCheckpointOutcome::Complete
                    );

                    let exhausted = realm.execute_document_classic(ClassicScriptRequest::new(
                        "function (",
                        "diagnostic-2.js",
                    ));
                    assert_eq!(
                        exhausted.outcome,
                        ClassicScriptOutcome::ResourceLimit(ResourceLimitKind::Diagnostics {
                            requested_total: 2,
                            limit: 1,
                        })
                    );
                    assert_eq!(realm.document_diagnostics_emitted(), Some(1));
                },
            );
            assert_eq!(
                completion.unwrap_err(),
                ClassicScriptOutcome::ResourceLimit(ResourceLimitKind::Diagnostics {
                    requested_total: 2,
                    limit: 1,
                })
            );
        });
    }

    #[test]
    fn document_opcode_allocation_and_job_accounting_is_cumulative() {
        let seed_source = "globalThis.documentSeed = { value: 1 };";
        let mut measuring_cx = context();
        let (seed_opcodes, seed_allocations) = measuring_cx.with_browser_script_realm(|realm| {
            let measured = run(realm, seed_source);
            assert!(matches!(measured.outcome, ClassicScriptOutcome::Success(_)));
            (measured.report.opcodes_executed(), measured.report.managed_allocation_bytes())
        });
        assert!(seed_opcodes > 0);
        assert!(seed_allocations > 0);

        let opcode_limit = seed_opcodes.checked_add(2).unwrap();
        let allocation_limit = seed_allocations.checked_add(1).unwrap();
        let limits =
            ClassicScriptLimits::new(opcode_limit, 8 * 1024 * 1024, 64, 1, Duration::from_secs(2))
                .unwrap();
        let mut cx = context();
        cx.with_browser_script_realm(|realm| {
            let completion = realm.with_document_script_budget_caps(
                limits,
                &ScriptInterruptHandle::new(),
                DocumentScriptCaps { script_candidates: 8, source_bytes: 4096, diagnostics: 8 },
                |realm| {
                    let first = realm.execute_document_classic(ClassicScriptRequest::new(
                        seed_source,
                        "seed.js",
                    ));
                    assert!(matches!(first.outcome, ClassicScriptOutcome::Success(_)));
                    assert_eq!(realm.document_opcodes_executed(), Some(seed_opcodes));
                    assert_eq!(
                        realm.document_managed_allocation_bytes(),
                        Some(first.report.managed_allocation_bytes())
                    );
                    assert_eq!(
                        realm.perform_document_microtask_checkpoint().outcome,
                        MicrotaskCheckpointOutcome::Complete
                    );

                    let opcode = realm.execute_document_classic(ClassicScriptRequest::new(
                        "while (true) {}",
                        "opcode-limit.js",
                    ));
                    assert_eq!(
                        opcode.outcome,
                        ClassicScriptOutcome::ResourceLimit(ResourceLimitKind::Opcodes {
                            limit: opcode_limit,
                        })
                    );
                    assert_eq!(realm.document_opcodes_executed(), Some(opcode_limit));
                },
            );
            assert_eq!(
                completion.unwrap_err(),
                ClassicScriptOutcome::ResourceLimit(ResourceLimitKind::Opcodes {
                    limit: opcode_limit,
                })
            );
        });

        let allocation_limits =
            ClassicScriptLimits::new(100_000, allocation_limit, 64, 8, Duration::from_secs(2))
                .unwrap();
        let mut allocation_cx = context();
        allocation_cx.with_browser_script_realm(|realm| {
            let completion = realm.with_document_script_budget_caps(
                allocation_limits,
                &ScriptInterruptHandle::new(),
                DocumentScriptCaps { script_candidates: 4, source_bytes: 4096, diagnostics: 4 },
                |realm| {
                    let first = realm.execute_document_classic(
                        ClassicScriptRequest::new(seed_source, "https://example.test/app.js")
                            .with_base_url("https://example.test/"),
                    );
                    assert!(matches!(first.outcome, ClassicScriptOutcome::Success(_)));
                    assert_eq!(realm.document_managed_allocation_bytes(), Some(seed_allocations));
                    assert_eq!(
                        realm.perform_document_microtask_checkpoint().outcome,
                        MicrotaskCheckpointOutcome::Complete
                    );

                    let exhausted = realm.execute_document_classic(ClassicScriptRequest::new(
                        "({ value: 2 });",
                        "allocation-limit.js",
                    ));
                    assert!(matches!(
                        exhausted.outcome,
                        ClassicScriptOutcome::ResourceLimit(
                            ResourceLimitKind::ManagedAllocationBytes {
                                requested_total,
                                limit,
                            }
                        ) if requested_total > limit && limit == allocation_limit
                    ));
                    assert!(realm.document_managed_allocation_bytes().unwrap() <= allocation_limit);
                },
            );
            assert!(matches!(
                completion.unwrap_err(),
                ClassicScriptOutcome::ResourceLimit(
                    ResourceLimitKind::ManagedAllocationBytes {
                        requested_total,
                        limit,
                    }
                ) if requested_total > limit && limit == allocation_limit
            ));
        });

        let job_limits =
            ClassicScriptLimits::new(100_000, 8 * 1024 * 1024, 64, 1, Duration::from_secs(2))
                .unwrap();
        let mut jobs_cx = context();
        jobs_cx.with_browser_script_realm(|realm| {
            let completion = realm.with_document_script_budget_caps(
                job_limits,
                &ScriptInterruptHandle::new(),
                DocumentScriptCaps { script_candidates: 4, source_bytes: 4096, diagnostics: 4 },
                |realm| {
                    let first = realm.execute_document_classic(ClassicScriptRequest::new(
                        "globalThis.firstJob = false;\n\
                             Promise.resolve().then(() => { firstJob = true; });",
                        "job-1.js",
                    ));
                    assert!(matches!(first.outcome, ClassicScriptOutcome::Success(_)));
                    let first_checkpoint = realm.perform_document_microtask_checkpoint();
                    assert_eq!(first_checkpoint.outcome, MicrotaskCheckpointOutcome::Complete);
                    assert_eq!(realm.document_jobs_executed(), Some(1));

                    let second = realm.execute_document_classic(ClassicScriptRequest::new(
                        "if (!firstJob) throw 'first job did not run';\n\
                             Promise.resolve().then(() => { globalThis.secondJob = true; });",
                        "job-2.js",
                    ));
                    assert!(matches!(second.outcome, ClassicScriptOutcome::Success(_)));
                    let exhausted = realm.perform_document_microtask_checkpoint();
                    assert_eq!(
                        exhausted.outcome,
                        MicrotaskCheckpointOutcome::ResourceLimit(ResourceLimitKind::Jobs {
                            limit: 1,
                        })
                    );
                    assert_eq!(realm.document_jobs_executed(), Some(1));
                    assert_eq!(exhausted.report.pending_jobs_at_exit(), 0);
                },
            );
            assert_eq!(
                completion.unwrap_err(),
                ClassicScriptOutcome::ResourceLimit(ResourceLimitKind::Jobs { limit: 1 })
            );
        });
    }

    #[test]
    fn document_session_retires_foreign_and_uncheckpointed_jobs_before_recovery() {
        let limits = ClassicScriptLimits::parser_blocking_document(Duration::from_secs(2)).unwrap();
        let mut cx = context();
        cx.with_browser_script_realm(|realm| {
            let foreign = run(
                realm,
                "globalThis.foreignJobRan = false;\n\
                 Promise.resolve().then(() => { foreignJobRan = true; });",
            );
            assert!(matches!(foreign.outcome, ClassicScriptOutcome::Success(_)));
            assert!(foreign.report.pending_jobs_at_exit() >= 1);

            let callback_invoked = std::cell::Cell::new(false);
            let rejected =
                realm.with_document_script_budget(limits, &ScriptInterruptHandle::new(), |_| {
                    callback_invoked.set(true)
                });
            assert!(!callback_invoked.get());
            assert!(matches!(
                rejected,
                Err(ClassicScriptOutcome::PendingJobsAtDocumentStart {
                    retired_jobs,
                }) if retired_jobs >= 1
            ));

            realm
                .with_document_script_budget(limits, &ScriptInterruptHandle::new(), |realm| {
                    let check = realm.execute_document_classic(ClassicScriptRequest::new(
                        "if (foreignJobRan !== false) throw 'foreign job escaped';",
                        "foreign-job-check.js",
                    ));
                    assert!(matches!(check.outcome, ClassicScriptOutcome::Success(_)));
                })
                .unwrap();

            let pending_exit =
                realm.with_document_script_budget(limits, &ScriptInterruptHandle::new(), |realm| {
                    let queued = realm.execute_document_classic(ClassicScriptRequest::new(
                        "globalThis.uncheckpointedJobRan = false;\n\
                         Promise.resolve().then(() => { uncheckpointedJobRan = true; });",
                        "uncheckpointed-job.js",
                    ));
                    assert!(matches!(queued.outcome, ClassicScriptOutcome::Success(_)));
                    assert!(queued.report.pending_jobs_at_exit() >= 1);
                });
            assert!(matches!(
                pending_exit,
                Err(ClassicScriptOutcome::PendingJobsAtDocumentExit {
                    retired_jobs,
                }) if retired_jobs >= 1
            ));

            realm
                .with_document_script_budget(limits, &ScriptInterruptHandle::new(), |realm| {
                    let check = realm.execute_document_classic(ClassicScriptRequest::new(
                        "if (uncheckpointedJobRan !== false) throw 'exit job escaped';",
                        "uncheckpointed-job-check.js",
                    ));
                    assert!(matches!(check.outcome, ClassicScriptOutcome::Success(_)));
                })
                .unwrap();
        });
    }

    #[test]
    fn document_cancellation_retires_pending_job_before_fresh_budget() {
        let limits = ClassicScriptLimits::parser_blocking_document(Duration::from_secs(2)).unwrap();
        let interrupt = ScriptInterruptHandle::new();
        let mut cx = context();
        cx.with_browser_script_realm(|realm| {
            let completion = realm.with_document_script_budget(limits, &interrupt, |realm| {
                let queued = realm.execute_document_classic(ClassicScriptRequest::new(
                    "globalThis.cancelledJobRan = false;\n\
                         Promise.resolve().then(() => { cancelledJobRan = true; });",
                    "cancelled-pending-job.js",
                ));
                assert!(matches!(queued.outcome, ClassicScriptOutcome::Success(_)));
                assert!(queued.report.pending_jobs_at_exit() >= 1);

                interrupt.request_interrupt();
                assert_eq!(
                    realm.account_skipped_document_script(0).unwrap_err(),
                    ClassicScriptOutcome::Interrupted(InterruptReason::ExternalRequest)
                );
            });
            assert_eq!(
                completion.unwrap_err(),
                ClassicScriptOutcome::Interrupted(InterruptReason::ExternalRequest)
            );

            realm
                .with_document_script_budget(limits, &ScriptInterruptHandle::new(), |realm| {
                    let check = realm.execute_document_classic(ClassicScriptRequest::new(
                        "if (cancelledJobRan !== false) throw 'cancelled job escaped';",
                        "cancelled-job-check.js",
                    ));
                    assert!(matches!(check.outcome, ClassicScriptOutcome::Success(_)));
                })
                .unwrap();
        });
    }

    #[test]
    fn document_exit_retirement_polls_deadline_and_still_empties_pending_jobs() {
        let limits =
            ClassicScriptLimits::new(100_000, 8 * 1024 * 1024, 64, 4, Duration::from_millis(10))
                .unwrap();
        let mut cx = context();
        cx.with_browser_script_realm(|realm| {
            let completion =
                realm.with_document_script_budget(limits, &ScriptInterruptHandle::new(), |realm| {
                    let queued = realm.execute_document_classic(ClassicScriptRequest::new(
                        "globalThis.deadlineCleanupJobRan = false;\n\
                         Promise.resolve().then(() => { deadlineCleanupJobRan = true; });",
                        "deadline-cleanup-job.js",
                    ));
                    assert!(matches!(queued.outcome, ClassicScriptOutcome::Success(_)));
                    thread::sleep(Duration::from_millis(50));
                });
            assert_eq!(
                completion.unwrap_err(),
                ClassicScriptOutcome::Interrupted(InterruptReason::Deadline)
            );

            realm
                .with_document_script_budget(
                    ClassicScriptLimits::parser_blocking_document(Duration::from_secs(2)).unwrap(),
                    &ScriptInterruptHandle::new(),
                    |realm| {
                        let check = realm.execute_document_classic(ClassicScriptRequest::new(
                            "if (deadlineCleanupJobRan !== false) throw 'deadline job escaped';",
                            "deadline-cleanup-check.js",
                        ));
                        assert!(matches!(check.outcome, ClassicScriptOutcome::Success(_)));
                    },
                )
                .unwrap();
        });
    }

    #[test]
    fn document_callback_panic_with_pending_job_permanently_poisons_context() {
        let limits = ClassicScriptLimits::parser_blocking_document(Duration::from_secs(2)).unwrap();
        let mut cx = context();
        // SAFETY: Saved only to prove that every safe method on a legacy token is sealed after
        // poison; no handle or mutable reference is retained across owner calls.
        let raw = unsafe { cx.raw_context_unchecked() };
        let panic = catch_unwind(AssertUnwindSafe(|| {
            cx.with_browser_script_realm(|realm| {
                let _ = realm.with_document_script_budget(
                    limits,
                    &ScriptInterruptHandle::new(),
                    |realm| {
                        let queued = realm.execute_document_classic(ClassicScriptRequest::new(
                            "Promise.resolve().then(() => { globalThis.panicJobRan = true; });",
                            "callback-panic-job.js",
                        ));
                        assert!(matches!(queued.outcome, ClassicScriptOutcome::Success(_)));
                        assert!(queued.report.pending_jobs_at_exit() >= 1);
                        panic!("injected document coordinator panic");
                    },
                );
            });
        }));
        assert!(panic.is_err());
        assert_eq!(cx.poisoned_owner_diagnostics(), (true, 0, false));
        assert_poisoned_owner_surfaces_are_sealed(&mut cx, raw);
        drop(cx);
    }

    #[test]
    fn document_pending_job_cap_is_exact_monotone_and_reusable_only_after_bounded_cleanup() {
        let limits =
            ClassicScriptLimits::new(100_000, 8 * 1024 * 1024, 64, 2, Duration::from_secs(2))
                .unwrap();
        let terminal =
            ClassicScriptOutcome::ResourceLimit(ResourceLimitKind::PendingJobs { limit: 2 });
        let checkpoint_terminal =
            MicrotaskCheckpointOutcome::ResourceLimit(ResourceLimitKind::PendingJobs { limit: 2 });
        let mut cx = context();
        cx.with_browser_script_realm(|realm| {
            let completion =
                realm.with_document_script_budget(limits, &ScriptInterruptHandle::new(), |realm| {
                    let overflow = realm.execute_document_classic(ClassicScriptRequest::new(
                        "globalThis.pendingOverflowRan = 0;\n\
                         for (let i = 0; i < 8; i++) {\n\
                           Promise.resolve().then(() => { pendingOverflowRan += 1; });\n\
                         }",
                        "pending-overflow.js",
                    ));
                    assert_eq!(overflow.outcome, terminal);
                    assert_eq!(overflow.report.pending_jobs_at_exit(), 0);
                    assert_eq!(realm.raw.task_queue().browser_script_len(), 0);
                    assert_eq!(realm.raw.task_queue().browser_pending_cap_peak_len(), Some(2));
                    assert_eq!(realm.raw.task_queue().browser_pending_cap_overflow(), Some(2));

                    assert_eq!(
                        realm.perform_document_microtask_checkpoint().outcome,
                        checkpoint_terminal
                    );
                    assert_eq!(
                        realm
                            .execute_document_classic(ClassicScriptRequest::new(
                                "throw 'terminal reason changed';",
                                "after-pending-overflow.js",
                            ))
                            .outcome,
                        terminal
                    );
                });
            assert_eq!(completion.unwrap_err(), terminal);

            realm
                .with_document_script_budget(limits, &ScriptInterruptHandle::new(), |realm| {
                    let check = realm.execute_document_classic(ClassicScriptRequest::new(
                        "if (pendingOverflowRan !== 0) throw 'overflow job escaped';",
                        "pending-overflow-recovery.js",
                    ));
                    assert!(matches!(check.outcome, ClassicScriptOutcome::Success(_)));
                })
                .unwrap();
        });
    }

    #[test]
    fn document_job_throw_is_one_terminal_across_checkpoint_classic_and_close() {
        let limits = ClassicScriptLimits::parser_blocking_document(Duration::from_secs(2)).unwrap();
        let terminal = ScriptValueSummary::String { code_units: 16 };
        let mut cx = context_with_exposed_gc();
        cx.with_browser_script_realm(|realm| {
            let completion =
                realm.with_document_script_budget(limits, &ScriptInterruptHandle::new(), |realm| {
                    let queued = realm.execute_document_classic(ClassicScriptRequest::new(
                        "globalThis.documentCleanupRegistry = new FinalizationRegistry(() => {\n\
                           throw 'cleanup exploded';\n\
                         });\n\
                         (() => {\n\
                           const target = {};\n\
                           documentCleanupRegistry.register(target, 'held');\n\
                         })();\n\
                         gc.run();",
                        "throwing-job.js",
                    ));
                    assert!(matches!(queued.outcome, ClassicScriptOutcome::Success(_)));

                    let checkpoint = realm.perform_document_microtask_checkpoint();
                    assert_eq!(checkpoint.outcome, MicrotaskCheckpointOutcome::JobThrown(terminal));
                    let classic = realm.execute_document_classic(ClassicScriptRequest::new(
                        "throw 'must not execute';",
                        "after-job-throw.js",
                    ));
                    assert_eq!(classic.outcome, ClassicScriptOutcome::JobThrown(terminal));
                    assert_eq!(classic.report.opcodes_executed(), 0);
                    assert_eq!(
                        realm.perform_document_microtask_checkpoint().outcome,
                        MicrotaskCheckpointOutcome::JobThrown(terminal)
                    );
                });
            assert_eq!(completion.unwrap_err(), ClassicScriptOutcome::JobThrown(terminal));
        });
    }

    #[test]
    fn document_recursion_configuration_rejects_257_and_512_without_poisoning() {
        let mut cx = context();
        cx.with_browser_script_realm(|realm| {
            for recursion_depth in [257, 512] {
                let excessive = ClassicScriptLimits::new(
                    100_000,
                    8 * 1024 * 1024,
                    recursion_depth,
                    16,
                    Duration::from_secs(2),
                )
                .unwrap();
                let callback_invoked = std::cell::Cell::new(false);
                let rejected = realm.with_document_script_budget(
                    excessive,
                    &ScriptInterruptHandle::new(),
                    |_| callback_invoked.set(true),
                );
                assert!(!callback_invoked.get());
                assert_eq!(
                    rejected.unwrap_err(),
                    ClassicScriptOutcome::InvalidDocumentSession(
                        LimitConfigurationError::ExceedsHardMaximum {
                            field: "document_max_recursion_depth",
                            hard_maximum: 256,
                        }
                    )
                );
            }

            realm
                .with_document_script_budget(
                    ClassicScriptLimits::parser_blocking_document(Duration::from_secs(2)).unwrap(),
                    &ScriptInterruptHandle::new(),
                    |_| {},
                )
                .unwrap();
        });
    }

    #[test]
    fn hosted_document_elapsed_absolute_deadline_rejects_before_callback() {
        let limits = ClassicScriptLimits::parser_blocking_document(Duration::from_secs(2)).unwrap();
        let deadline = Instant::now()
            .checked_sub(Duration::from_millis(1))
            .unwrap();
        let mut host = NoopDocumentHost::default();
        let mut cx = context();

        cx.with_browser_script_realm(|realm| {
            let callback_invoked = Cell::new(false);
            let completion = realm.with_hosted_document_script_budget_until(
                &mut host,
                limits,
                &ScriptInterruptHandle::new(),
                deadline,
                |_| callback_invoked.set(true),
            );
            assert!(!callback_invoked.get());
            assert_eq!(
                completion.unwrap_err(),
                ClassicScriptOutcome::Interrupted(InterruptReason::Deadline)
            );
            assert!(realm.document_budget.is_none());
        });

        assert_eq!(host.finished_phases, 0);
        assert_eq!(host.aborted_phases, 0);
    }

    #[test]
    fn hosted_document_deadline_crossed_during_host_setup_never_enters_callback() {
        let limits = ClassicScriptLimits::parser_blocking_document(Duration::from_secs(2)).unwrap();
        let mut host = NoopDocumentHost::default();
        let mut cx = context();

        cx.with_browser_script_realm(|realm| {
            for _ in 0..32 {
                let callback_invoked = Cell::new(false);
                let deadline = Instant::now()
                    .checked_add(Duration::from_nanos(150))
                    .unwrap();
                let completion = realm.with_hosted_document_script_budget_until(
                    &mut host,
                    limits,
                    &ScriptInterruptHandle::new(),
                    deadline,
                    |_| callback_invoked.set(true),
                );
                assert!(!callback_invoked.get());
                assert_eq!(
                    completion.unwrap_err(),
                    ClassicScriptOutcome::Interrupted(InterruptReason::Deadline)
                );
                assert!(realm.document_budget.is_none());
            }
        });

        assert_eq!(host.finished_phases, 0);
        assert_eq!(host.aborted_phases, 0);
    }

    #[test]
    fn hosted_document_absolute_deadline_includes_pre_entry_setup_time() {
        let limits = ClassicScriptLimits::parser_blocking_document(Duration::from_secs(5)).unwrap();
        let setup_started = Instant::now();
        let caller_deadline = setup_started.checked_add(Duration::from_secs(2)).unwrap();
        thread::sleep(Duration::from_millis(100));
        let entry_time = Instant::now();
        assert!(setup_started.elapsed() >= Duration::from_millis(75));
        assert!(caller_deadline.saturating_duration_since(entry_time) < Duration::from_secs(2));

        let mut host = NoopDocumentHost::default();
        let mut cx = context();
        cx.with_browser_script_realm(|realm| {
            realm
                .with_hosted_document_script_budget_until(
                    &mut host,
                    limits,
                    &ScriptInterruptHandle::new(),
                    caller_deadline,
                    |realm| {
                        let budget = realm.document_budget.as_ref().unwrap();
                        assert_eq!(budget.deadline, caller_deadline);
                        assert!(
                            budget.deadline.saturating_duration_since(Instant::now())
                                < Duration::from_secs(2)
                        );
                    },
                )
                .unwrap();
        });
    }

    #[test]
    fn hosted_document_relative_hard_limit_wins_before_later_absolute_deadline() {
        let relative_wall_time = Duration::from_secs(2);
        let limits = ClassicScriptLimits::parser_blocking_document(relative_wall_time).unwrap();
        let before_entry = Instant::now();
        let caller_deadline = before_entry.checked_add(Duration::from_secs(20)).unwrap();
        let mut host = NoopDocumentHost::default();
        let mut cx = context();

        cx.with_browser_script_realm(|realm| {
            realm
                .with_hosted_document_script_budget_until(
                    &mut host,
                    limits,
                    &ScriptInterruptHandle::new(),
                    caller_deadline,
                    |realm| {
                        let stored_deadline = realm.document_budget.as_ref().unwrap().deadline;
                        assert!(stored_deadline < caller_deadline);
                        assert!(
                            stored_deadline
                                >= before_entry.checked_add(relative_wall_time).unwrap()
                        );
                        assert!(
                            stored_deadline
                                <= Instant::now().checked_add(relative_wall_time).unwrap()
                        );
                    },
                )
                .unwrap();
        });
    }

    #[test]
    fn hosted_document_absolute_deadline_preserves_cancellation_latching_and_cleanup() {
        let limits = ClassicScriptLimits::parser_blocking_document(Duration::from_secs(5)).unwrap();
        let pre_requested = ScriptInterruptHandle::new();
        pre_requested.request_interrupt();
        let elapsed_deadline = Instant::now()
            .checked_sub(Duration::from_millis(1))
            .unwrap();
        let mut host = NoopDocumentHost::default();
        let mut cx = context();

        cx.with_browser_script_realm(|realm| {
            let callback_invoked = Cell::new(false);
            let precedence = realm.with_hosted_document_script_budget_until(
                &mut host,
                limits,
                &pre_requested,
                elapsed_deadline,
                |_| callback_invoked.set(true),
            );
            assert!(!callback_invoked.get());
            assert_eq!(
                precedence.unwrap_err(),
                ClassicScriptOutcome::Interrupted(InterruptReason::ExternalRequest)
            );

            let interrupt = ScriptInterruptHandle::new();
            let future_deadline = Instant::now().checked_add(Duration::from_secs(10)).unwrap();
            let cancelled = realm.with_hosted_document_script_budget_until(
                &mut host,
                limits,
                &interrupt,
                future_deadline,
                |realm| {
                    let queued = realm.execute_hosted_document_classic(ClassicScriptRequest::new(
                        "globalThis.absoluteCancelledJobRan = false;\n\
                             Promise.resolve().then(() => {\n\
                               absoluteCancelledJobRan = true;\n\
                             });",
                        "absolute-cancelled-job.js",
                    ));
                    assert!(matches!(queued.script.outcome, ClassicScriptOutcome::Success(_)));
                    assert!(matches!(
                        queued.host,
                        BrowserHostPhaseOutcome::Completed(BrowserHostCommitOutcome::NoChanges(_))
                    ));
                    assert!(queued.script.report.pending_jobs_at_exit() >= 1);

                    interrupt.request_interrupt();
                    assert_eq!(
                        realm.account_skipped_document_script(0).unwrap_err(),
                        ClassicScriptOutcome::Interrupted(InterruptReason::ExternalRequest)
                    );
                    let repeated = realm.perform_hosted_document_microtask_checkpoint();
                    assert_eq!(
                        repeated.checkpoint.outcome,
                        MicrotaskCheckpointOutcome::Interrupted(InterruptReason::ExternalRequest)
                    );
                    assert_eq!(repeated.host, BrowserHostPhaseOutcome::NotStarted);
                    assert_eq!(realm.raw.task_queue().browser_script_len(), 1);
                },
            );
            assert_eq!(
                cancelled.unwrap_err(),
                ClassicScriptOutcome::Interrupted(InterruptReason::ExternalRequest)
            );
            assert_eq!(realm.raw.task_queue().browser_script_len(), 0);

            realm
                .with_hosted_document_script_budget_until(
                    &mut host,
                    limits,
                    &ScriptInterruptHandle::new(),
                    Instant::now().checked_add(Duration::from_secs(10)).unwrap(),
                    |realm| {
                        let check =
                            realm.execute_hosted_document_classic(ClassicScriptRequest::new(
                                "if (absoluteCancelledJobRan !== false) {\n\
                                   throw 'cancelled job escaped cleanup';\n\
                                 }",
                                "absolute-cancelled-job-check.js",
                            ));
                        assert!(matches!(check.script.outcome, ClassicScriptOutcome::Success(_)));
                    },
                )
                .unwrap();
        });

        assert_eq!(host.finished_phases, 2);
        assert_eq!(host.aborted_phases, 0);
    }

    #[test]
    fn document_navigation_interrupt_has_deadline_precedence_and_latches() {
        let limits = ClassicScriptLimits::parser_blocking_document(Duration::from_secs(2)).unwrap();
        let interrupt = ScriptInterruptHandle::new();
        let requester = interrupt.clone();
        let mut cx = context();
        cx.with_browser_script_realm(|realm| {
            let completion = realm.with_document_script_budget(limits, &interrupt, |realm| {
                let request_thread = thread::spawn(move || {
                    thread::sleep(Duration::from_millis(1));
                    requester.request_interrupt();
                });
                let interrupted = realm.execute_document_classic(ClassicScriptRequest::new(
                    "while (true) {}",
                    "cancelled-navigation.js",
                ));
                request_thread.join().unwrap();
                assert_eq!(
                    interrupted.outcome,
                    ClassicScriptOutcome::Interrupted(InterruptReason::ExternalRequest)
                );
                let repeated = realm.perform_document_microtask_checkpoint();
                assert_eq!(
                    repeated.outcome,
                    MicrotaskCheckpointOutcome::Interrupted(InterruptReason::ExternalRequest)
                );
            });
            assert_eq!(
                completion.unwrap_err(),
                ClassicScriptOutcome::Interrupted(InterruptReason::ExternalRequest)
            );
        });

        let pre_requested = ScriptInterruptHandle::new();
        pre_requested.request_interrupt();
        let mut precedence_cx = context();
        precedence_cx.with_browser_script_realm(|realm| {
            let immediate = ClassicScriptLimits::parser_blocking_document(Duration::ZERO).unwrap();
            let callback_invoked = std::cell::Cell::new(false);
            let completion = realm.with_document_script_budget(immediate, &pre_requested, |_| {
                callback_invoked.set(true);
            });
            assert!(!callback_invoked.get());
            assert_eq!(
                completion.unwrap_err(),
                ClassicScriptOutcome::Interrupted(InterruptReason::ExternalRequest)
            );
        });

        let mut absolute_deadline_cx = context();
        absolute_deadline_cx.with_browser_script_realm(|realm| {
            let absolute =
                ClassicScriptLimits::parser_blocking_document(Duration::from_millis(100)).unwrap();
            let completion = realm.with_document_script_budget(
                absolute,
                &ScriptInterruptHandle::new(),
                |realm| {
                    realm.account_skipped_document_script(0).unwrap();
                    thread::sleep(Duration::from_millis(150));
                    let expired = realm.perform_document_microtask_checkpoint();
                    assert_eq!(
                        expired.outcome,
                        MicrotaskCheckpointOutcome::Interrupted(InterruptReason::Deadline)
                    );
                    assert_eq!(expired.report.opcodes_executed(), 0);
                    assert_eq!(
                        realm.perform_document_microtask_checkpoint().outcome,
                        expired.outcome
                    );
                },
            );
            assert_eq!(
                completion.unwrap_err(),
                ClassicScriptOutcome::Interrupted(InterruptReason::Deadline)
            );
        });
    }

    #[test]
    fn engine_panic_seals_preissued_raw_handle_property_and_root_mutations() {
        let mut cx = context();
        // SAFETY: This crate-internal hostile regression serializes the raw token, keeps every
        // handle in this exact scope, invokes only the defensive poison checks after the injected
        // fatal seam, and drops the scope before its owner. It never extracts a raw pointer.
        let raw = unsafe { cx.raw_context_unchecked() };
        let scope = HandleScopeGuard::new(raw);
        let mut object = raw.initial_realm().global_object().as_object();
        let array_key = PropertyKey::array_index_handle(raw, 7).unwrap();
        let named_key = raw.names.length();
        let property_value = Value::smi(41).to_handle(raw);
        object
            .set_property(raw, array_key, Property::default_data(property_value))
            .unwrap();
        object
            .set_property(raw, named_key, Property::default_data(property_value))
            .unwrap();
        let mut root_slot = Value::smi(17).to_handle(raw);

        cx.with_browser_script_realm(|realm| {
            let limits =
                ClassicScriptLimits::new(10_000, 8 * 1024 * 1024, 64, 8, Duration::from_secs(2))
                    .unwrap();
            let completion =
                realm.with_document_script_budget(limits, &ScriptInterruptHandle::new(), |realm| {
                    TEST_PANIC_AFTER_OPCODE.with(|slot| slot.set(Some(3)));
                    let panic = realm.execute_document_classic(ClassicScriptRequest::new(
                        "let poisonProbe = 1; poisonProbe += 2;",
                        "raw-handle-poison.js",
                    ));
                    assert_eq!(panic.outcome, ClassicScriptOutcome::EnginePanic);
                });
            assert_eq!(completion.unwrap_err(), ClassicScriptOutcome::EnginePanic);
        });
        assert!(cx.poisoned_owner_diagnostics().0);
        let diagnostics = cx.poisoned_owner_diagnostics();

        assert!(catch_unwind(AssertUnwindSafe(|| object.remove_property(raw, array_key))).is_err());
        assert!(catch_unwind(AssertUnwindSafe(|| object.remove_property(raw, named_key))).is_err());
        assert!(
            catch_unwind(AssertUnwindSafe(|| {
                object.set_property(raw, array_key, Property::default_data(property_value))
            }))
            .is_err()
        );
        assert!(catch_unwind(AssertUnwindSafe(|| object.delete(raw, array_key))).is_err());
        assert!(catch_unwind(AssertUnwindSafe(|| object.set_uninit_hash_code())).is_err());
        assert!(catch_unwind(AssertUnwindSafe(|| root_slot.replace(Value::smi(99)))).is_err());
        assert_eq!(cx.poisoned_owner_diagnostics(), diagnostics);

        drop(scope);
        drop(cx);
    }

    #[test]
    fn document_engine_panic_is_terminal_and_scope_cleanup_does_not_unpoison() {
        let mut cx = context();
        // SAFETY: Saved only to probe fail-closed safe methods after poison.
        let raw = unsafe { cx.raw_context_unchecked() };
        cx.with_browser_script_realm(|realm| {
            let limits =
                ClassicScriptLimits::new(10_000, 8 * 1024 * 1024, 64, 8, Duration::from_secs(2))
                    .unwrap();
            let completion =
                realm.with_document_script_budget(limits, &ScriptInterruptHandle::new(), |realm| {
                    TEST_PANIC_AFTER_PENDING_TASK.with(|slot| slot.set(true));
                    let panic = realm.execute_document_classic(ClassicScriptRequest::new(
                        "globalThis.enginePanicJobRan = false;\n\
                         Promise.resolve().then(() => { enginePanicJobRan = true; });\n\
                         let value = 1; value += 2;",
                        "panic.js",
                    ));
                    assert_eq!(panic.outcome, ClassicScriptOutcome::EnginePanic);
                    let repeated = realm.execute_document_classic(ClassicScriptRequest::new(
                        "throw 'must not execute';",
                        "after-panic.js",
                    ));
                    assert_eq!(repeated.outcome, ClassicScriptOutcome::EnginePanic);
                    assert_eq!(repeated.report.opcodes_executed(), 0);
                });
            assert_eq!(completion.unwrap_err(), ClassicScriptOutcome::EnginePanic);
        });
        let diagnostics = cx.poisoned_owner_diagnostics();
        assert!(diagnostics.0);
        assert!(diagnostics.1 >= 1, "fatal panic must leave queued work sealed in place");
        assert!(diagnostics.2, "fatal panic must not restore the scoped queue cap");
        assert_poisoned_owner_surfaces_are_sealed(&mut cx, raw);
        drop(cx);
    }
}
