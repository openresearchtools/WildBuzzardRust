//! Bounded classic-script admission for a future browser embedding.
//!
//! This module deliberately exposes a much smaller surface than the legacy raw [`Context`]. A
//! caller borrows one exact [`OwnedContext`] through a lifetime-branded realm token, supplies only
//! host-owned UTF-8 metadata, and receives only copied scalar summaries. No moving-GC handle or raw
//! context token can cross the boundary.

use std::{
    fmt::{self, Display, Write},
    marker::PhantomData,
    panic::{AssertUnwindSafe, catch_unwind, panic_any},
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

/// A lifetime-branded authority for classic scripts in one exact caller-owned context and its
/// initial realm. The higher-ranked constructor on [`OwnedContext`] prevents this token from being
/// retained after the callback.
pub struct BrowserScriptRealm<'realm> {
    raw: Context,
    _brand: PhantomData<&'realm mut OwnedContext>,
}

impl<'realm> BrowserScriptRealm<'realm> {
    pub(crate) fn new(raw: Context) -> Self {
        Self { raw, _brand: PhantomData }
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
            interrupt.requested.clone(),
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
            execute_classic_inner(raw, request)
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

    /// Perform one explicit, rooted, bounded promise-job checkpoint after the embedding has
    /// handled the primary script outcome. A job failure is fail-closed and discards remaining
    /// jobs because Brimstone does not yet expose the HTML host error-reporting continuation.
    pub fn perform_microtask_checkpoint(
        &mut self,
        limits: ClassicScriptLimits,
        interrupt: &ScriptInterruptHandle,
    ) -> MicrotaskCheckpointExecution {
        let metadata = AdmissionMetadata::empty();
        let mut admission = match BrowserAdmissionGuard::install(
            self.raw,
            limits,
            interrupt.requested.clone(),
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
    RuntimeBusy,
    RuntimePoisoned,
    EnginePanic,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ClassicScriptOutcome {
    Success(ScriptValueSummary),
    Thrown(ScriptValueSummary),
    ParseError(Vec<ScriptDiagnostic>),
    AnalyzeError(Vec<ScriptDiagnostic>),
    CompileError(ScriptDiagnostic),
    Interrupted(InterruptReason),
    ResourceLimit(ResourceLimitKind),
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
    SourceBytes { actual: usize, limit: usize },
    FilenameBytes { actual: usize, limit: usize },
    BaseBytes { actual: usize, limit: usize },
    Opcodes { limit: u64 },
    ManagedAllocationBytes { requested_total: usize, limit: usize },
    RecursionDepth { requested_depth: usize, limit: usize },
    Jobs { limit: u64 },
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
        interrupt: Arc<AtomicBool>,
        metadata: AdmissionMetadata,
    ) -> Self {
        let started = Instant::now();
        let deadline = started.checked_add(limits.wall_time).unwrap_or(started);
        Self {
            limits,
            interrupt,
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
        interrupt: Arc<AtomicBool>,
        metadata: AdmissionMetadata,
        kind: AdmissionKind,
    ) -> Result<Self, AdmissionInstallError> {
        if raw.browser_script_poisoned {
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
            Some(BrowserScriptAdmissionState::new(limits, interrupt, metadata));
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
            .browser_script_admission
            .take()
            .unwrap_or_else(|| std::process::abort())
            .finish(0)
    }
}

impl Drop for BrowserAdmissionGuard {
    fn drop(&mut self) {
        self.raw.browser_script_admission = None;
    }
}

#[derive(Clone, Copy)]
struct BrowserPolicyUnwind;

#[derive(Clone, Copy)]
enum PolicyTermination {
    Interrupted(InterruptReason),
    Resource(ResourceLimitKind),
}

impl PolicyTermination {
    fn into_outcome(self) -> ClassicScriptOutcome {
        match self {
            Self::Interrupted(reason) => ClassicScriptOutcome::Interrupted(reason),
            Self::Resource(limit) => ClassicScriptOutcome::ResourceLimit(limit),
        }
    }

    fn into_checkpoint_outcome(self) -> MicrotaskCheckpointOutcome {
        match self {
            Self::Interrupted(reason) => MicrotaskCheckpointOutcome::Interrupted(reason),
            Self::Resource(limit) => MicrotaskCheckpointOutcome::ResourceLimit(limit),
        }
    }
}

impl Context {
    fn browser_script_is_poisoned(&self) -> bool {
        self.browser_script_poisoned
    }

    fn poison_browser_script(mut self) {
        self.browser_script_poisoned = true;
    }

    pub(crate) fn browser_script_is_active(&self) -> bool {
        self.browser_script_admission.is_some()
    }

    pub(crate) fn browser_script_poll_phase(mut self) {
        if let Some(state) = self.browser_script_admission.as_mut() {
            state.check_interrupt();
        }
    }

    pub(crate) fn browser_script_poll_opcode(mut self, opcode: OpCode) {
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
        let Some(state) = self.browser_script_admission.as_mut() else {
            return;
        };
        state.check_interrupt();
        let requested_total = state.managed_allocation_bytes.saturating_add(bytes);
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

    pub(crate) fn clear_browser_script_tasks(mut self) {
        self.task_queue().clear_browser_script_tasks();
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
) -> ClassicScriptOutcome {
    cx.browser_script_poll_phase();
    let source =
        match Source::new_for_string(request.filename, Wtf8String::from_str(request.source)) {
            Ok(source) => Rc::new(source),
            Err(error) => return parse_error_outcome(&error),
        };

    cx.browser_script_poll_phase();
    let parse_context = ParseContext::new(source);
    let parsed = match parse_script(&parse_context, cx.options.clone()) {
        Ok(parsed) => parsed,
        Err(error) => return parse_error_outcome(&error),
    };

    cx.browser_script_poll_phase();
    let analyzed = match analyze(parsed) {
        Ok(analyzed) => analyzed,
        Err(errors) => return analyze_error_outcome(&errors),
    };

    cx.browser_script_poll_phase();
    let bytecode = match BytecodeProgramGenerator::generate_from_parse_script_result(
        cx,
        &analyzed,
        cx.initial_realm(),
    ) {
        Ok(bytecode) => bytecode,
        Err(error) => return compile_error_outcome(&error),
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

fn parse_error_outcome(error: &LocalizedParseError) -> ClassicScriptOutcome {
    match diagnostics_from_errors(std::slice::from_ref(error)) {
        Ok(diagnostics) => ClassicScriptOutcome::ParseError(diagnostics),
        Err(limit) => ClassicScriptOutcome::ResourceLimit(limit),
    }
}

fn analyze_error_outcome(errors: &LocalizedParseErrors) -> ClassicScriptOutcome {
    match diagnostics_from_errors(&errors.errors) {
        Ok(diagnostics) => ClassicScriptOutcome::AnalyzeError(diagnostics),
        Err(limit) => ClassicScriptOutcome::ResourceLimit(limit),
    }
}

fn compile_error_outcome(error: &EmitError) -> ClassicScriptOutcome {
    if matches!(error, EmitError::Alloc(_)) {
        return ClassicScriptOutcome::ResourceLimit(ResourceLimitKind::EngineAllocation);
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
) -> Result<Vec<ScriptDiagnostic>, ResourceLimitKind> {
    let count = errors.len().min(MAX_DIAGNOSTICS);
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
}

#[cfg(test)]
mod tests {
    use std::{rc::Rc, thread, time::Duration};

    use crate::{
        common::options::OptionsBuilder,
        runtime::{ContextBuilder, bytecode::instruction::OpCode},
    };

    use super::*;

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
        const HEAP_BYTES: usize = 64 * 1024 * 1024;
        let mut cx = context_with_fixed_heap(HEAP_BYTES);
        cx.with_browser_script_realm(|realm| {
            let failed = realm.execute_classic(
                ClassicScriptRequest::new(
                    "globalThis.jobRan = false;\n\
                     Promise.resolve().then(() => { jobRan = true; });\n\
                     globalThis.a = new Array(2000000).fill(1);\n\
                     globalThis.b = new Array(2000000).fill(2);\n\
                     globalThis.c = new Array(2000000).fill(3);",
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
    fn bytecode_generator_allocation_failure_is_a_resource_outcome() {
        let outcome = compile_error_outcome(&EmitError::Alloc(
            crate::runtime::alloc_error::AllocError::oom(),
        ));
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
}
