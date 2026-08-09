//! Contained Cranelift baseline prototype with bounded native local control flow.
//!
//! This is deliberately not a VM dispatch tier. It accepts only checked, trusted in-process
//! bytecode and exits to a checked contained continuation before unsupported operations or slow
//! dynamic types. The sole allocation-capable generated call is zero-capacity `NewObject`.
//! Compiler-derived liveness spills every live boxed value into the context-registered frame
//! before that call; no moving pointer is embedded in code or retained in a native temporary
//! across the safepoint. Every taken native nonpositive edge publishes its exact target and calls
//! the versioned nonallocating poll helper before another iteration.

use std::{
    mem::size_of,
    num::NonZeroU64,
    sync::atomic::{AtomicU64, Ordering},
};

use cranelift_codegen::{
    Context,
    control::ControlPlane,
    ir::{
        AbiParam, Block, Function, InstBuilder, MemFlagsData, SigRef, Signature, SourceLoc,
        UserFuncName, Value as ClifValue, condcodes::IntCC, types,
    },
    isa::{CallConv, OwnedTargetIsa},
    settings::{self, Configurable},
};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};

use super::abi::{
    ACTIVATION_ABI_VERSION_OFFSET, ACTIVATION_CONTEXT_OFFSET, ACTIVATION_FRAME_OFFSET,
    ACTIVATION_HELPERS_OFFSET, ACTIVATION_INTERRUPT_BUDGET_OFFSET,
    ACTIVATION_NATIVE_BACKEDGE_WORK_REMAINING_OFFSET, ACTIVATION_POISONED_OFFSET,
    ACTIVATION_RESERVED_TAIL_OFFSET, ACTIVATION_RETURN_VALUE_OFFSET, ACTIVATION_SIDE_EXIT_OFFSET,
    ACTIVATION_STRUCT_SIZE_OFFSET, GENERATED_CODE_ABI_VERSION, HELPER_TABLE_ABI_VERSION_OFFSET,
    HELPER_TABLE_BACKEDGE_POLL_OFFSET, HELPER_TABLE_NEW_OBJECT_ZERO_OFFSET,
    HELPER_TABLE_RESERVED_OFFSET, HELPER_TABLE_STRUCT_SIZE_OFFSET, JIT_ACTIVATION_SIZE,
    JIT_HELPER_TABLE_SIZE, MAX_LIVE_ROOT_ENTRIES, MAX_NATIVE_BACKEDGE_WORK_UNITS,
    MAX_SAFEPOINT_RECORDS, NO_BYTECODE_OFFSET, NO_SAFEPOINT, SAFEPOINT_FLAG_ALLOCATING_HELPER,
    SHADOW_FRAME_BYTECODE_OFFSET, SHADOW_FRAME_LIVE_SLOT_COUNT_OFFSET,
    SHADOW_FRAME_LIVE_SLOTS_OFFSET, SHADOW_FRAME_RECORD_COUNT_OFFSET, SHADOW_FRAME_RECORDS_OFFSET,
    SHADOW_FRAME_SAFEPOINT_INDEX_OFFSET, SHADOW_FRAME_SLOT_COUNT_OFFSET, SHADOW_FRAME_SLOTS_OFFSET,
    STATUS_ALLOCATION_FAILED, STATUS_INTERRUPTED, STATUS_INVALID_ACTIVATION, STATUS_POISONED,
    STATUS_RETURNED, STATUS_SIDE_EXIT, SafepointMetadata, SafepointMetadataError, SafepointRecord,
};
use crate::runtime::{
    Value,
    bytecode::{
        WidthEnum,
        instruction::OpCode,
        metadata::{ControlFlow, EffectFlags, OperandAccess},
        verifier::{DecodedOperand, VerifiedBytecode, VerifiedInstruction},
    },
    value::SMI_TAG,
};

pub(crate) const MAX_PROTOTYPE_INSTRUCTIONS: usize = 100_000;
pub(crate) const MAX_PROTOTYPE_FRAME_SLOTS: usize = 1 << 20;
pub(crate) const MAX_PROTOTYPE_CODE_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const MAX_LIVENESS_ANALYSIS_BYTES: usize = 32 * 1024 * 1024;
pub(crate) const MAX_LIVENESS_WORKLIST_DEQUEUES: usize = 2_000_000;
pub(crate) const MAX_NATIVE_VALUE_WORKLIST_DEQUEUES: usize = 2_000_000;

const VALUE_TAG_SHIFT: i64 = 48;
const SLOT_BYTES: usize = size_of::<u64>();
const BACKEDGE_POLL_SOURCE_LOC_BASE: u32 = 0x5000_0000;
const SAFEPOINT_SOURCE_LOC_BASE: u32 = 0x6000_0000;

const HELPER_STATUS_OK: i64 = 0;
const HELPER_STATUS_INTERRUPTED: i64 = 2;
const HELPER_STATUS_ALLOCATION_FAILED: i64 = 3;
const HELPER_STATUS_POISONED: i64 = 4;
const HELPER_STATUS_SIDE_EXIT: i64 = 5;

static NEXT_VM_BINDING_ID: AtomicU64 = AtomicU64::new(1);

/// Process-unique identity binding one rooted VM target to one compiled artifact.
///
/// Values are never reused. Exhaustion fails closed instead of wrapping, so a stale executable
/// artifact can never acquire the identity of a later function.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::runtime::jit) struct VmBindingId(NonZeroU64);

impl VmBindingId {
    #[cfg(test)]
    const fn get(self) -> u64 {
        self.0.get()
    }
}

pub(in crate::runtime::jit) fn allocate_vm_binding_id() -> Result<VmBindingId, BaselineCompileError>
{
    allocate_vm_binding_id_from(&NEXT_VM_BINDING_ID)
}

fn allocate_vm_binding_id_from(next: &AtomicU64) -> Result<VmBindingId, BaselineCompileError> {
    let value = next
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| current.checked_add(1))
        .map_err(|_| BaselineCompileError::VmBindingIdsExhausted)?;
    NonZeroU64::new(value)
        .map(VmBindingId)
        .ok_or(BaselineCompileError::VmBindingIdsExhausted)
}

#[derive(Debug)]
pub(crate) enum BaselineCompileError {
    TooManyInstructions { actual: usize, maximum: usize },
    TooManyFrameSlots { actual: usize, maximum: usize },
    BytecodeOffsetTooLarge(usize),
    SlotOffsetTooLarge(usize),
    HostUnsupported(&'static str),
    Setting(String),
    Target(String),
    Codegen(String),
    UnexpectedRelocations(usize),
    UnexpectedTraps(usize),
    MachineCodeTooLarge { actual: usize, maximum: usize },
    LivenessAnalysisTooLarge { actual: usize, maximum: usize },
    LivenessAnalysisWorkLimitExceeded { maximum: usize },
    NativeValueAnalysisWorkLimitExceeded { maximum: usize },
    NativeValueAnalysisShapeMismatch { expected: usize, actual: usize },
    NativeEffectContractMismatch { bytecode_offset: usize, opcode: OpCode },
    UnsupportedNonLocalRegister { bytecode_offset: usize },
    UnsupportedClassMethodLiveness { bytecode_offset: usize },
    TooManySafepoints { actual: usize, maximum: usize },
    TooManyLiveRoots { actual: usize, maximum: usize },
    SafepointSourceLocationOverflow(usize),
    SafepointCallCountMismatch { expected: usize, actual: usize },
    MissingSafepointSourceLocation { native_return_offset: u32 },
    InvalidSafepointSourceLocation(u32),
    DuplicateSafepointCall(usize),
    BackedgePollSourceLocationOverflow(usize),
    MissingGeneratedCallSourceLocation { native_return_offset: u32 },
    InvalidBackedgePollSourceLocation(u32),
    DuplicateBackedgePollCall(usize),
    BackedgePollCallCountMismatch { expected: usize, actual: usize },
    SafepointMetadata(SafepointMetadataError),
    VmBindingIdsExhausted,
    VmArtifactAlreadyBound,
    AllocationFailed,
}

/// Owned, exact decoded program representation bound to one compilation.
///
/// The raw bytes are retained alongside the decoded instructions because constant-backed branch
/// targets are resolved during verification and are not recoverable from the bytes alone. No
/// caller can replace either representation after this value is prepared.
#[derive(Debug)]
pub(crate) struct PreparedProgram {
    bytes: Box<[u8]>,
    instructions: Box<[VerifiedInstruction]>,
    num_locals: usize,
    num_arguments: usize,
    num_constants: usize,
    num_caches: usize,
}

impl PreparedProgram {
    fn capture(bytecode: &VerifiedBytecode<'_>) -> Result<Self, BaselineCompileError> {
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(bytecode.bytes().len())
            .map_err(|_| BaselineCompileError::AllocationFailed)?;
        bytes.extend_from_slice(bytecode.bytes());

        let mut instructions = Vec::new();
        instructions
            .try_reserve_exact(bytecode.instructions().len())
            .map_err(|_| BaselineCompileError::AllocationFailed)?;
        for instruction in bytecode.instructions() {
            let mut operands = Vec::new();
            operands
                .try_reserve_exact(instruction.operands.len())
                .map_err(|_| BaselineCompileError::AllocationFailed)?;
            operands.extend_from_slice(&instruction.operands);
            instructions.push(VerifiedInstruction {
                offset: instruction.offset,
                opcode_index: instruction.opcode_index,
                next_offset: instruction.next_offset,
                width: instruction.width,
                opcode: instruction.opcode,
                operands,
                branch_target: instruction.branch_target,
                branch_constant: instruction.branch_constant,
                effects: instruction.effects,
            });
        }

        Ok(Self {
            bytes: bytes.into_boxed_slice(),
            instructions: instructions.into_boxed_slice(),
            num_locals: bytecode.num_locals(),
            num_arguments: bytecode.num_arguments(),
            num_constants: bytecode.num_constants(),
            num_caches: bytecode.num_caches(),
        })
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub(crate) fn instructions(&self) -> &[VerifiedInstruction] {
        &self.instructions
    }

    pub(crate) fn is_instruction_start(&self, offset: usize) -> bool {
        self.instructions
            .binary_search_by_key(&offset, |instruction| instruction.offset)
            .is_ok()
    }

    pub(crate) const fn num_locals(&self) -> usize {
        self.num_locals
    }

    pub(crate) const fn num_arguments(&self) -> usize {
        self.num_arguments
    }

    pub(crate) const fn num_constants(&self) -> usize {
        self.num_constants
    }

    pub(crate) const fn num_caches(&self) -> usize {
        self.num_caches
    }
}

/// Relocation-free machine code, checked maps, and the exact decoded program that produced both.
///
/// Fields and construction stay private to this compiler module. Loading consumes the whole value,
/// so safe code cannot pair executable bytes from one compilation with another program or map.
pub(crate) struct PreparedPrototype {
    machine_code: Vec<u8>,
    required_frame_slots: usize,
    safepoints: SafepointMetadata,
    program: PreparedProgram,
    vm_binding_id: Option<VmBindingId>,
}

impl PreparedPrototype {
    pub(crate) fn machine_code(&self) -> &[u8] {
        &self.machine_code
    }

    pub(crate) const fn required_frame_slots(&self) -> usize {
        self.required_frame_slots
    }

    pub(crate) fn safepoints(&self) -> &SafepointMetadata {
        &self.safepoints
    }

    pub(crate) fn program(&self) -> &PreparedProgram {
        &self.program
    }

    pub(in crate::runtime::jit) fn is_vm_bound(&self) -> bool {
        self.vm_binding_id.is_some()
    }

    pub(in crate::runtime::jit) fn is_bound_to_vm(&self, binding_id: VmBindingId) -> bool {
        matches!(self.vm_binding_id, Some(actual) if actual.0.get() == binding_id.0.get())
    }

    pub(in crate::runtime::jit) fn bind_to_vm(
        mut self,
        binding_id: VmBindingId,
    ) -> Result<Self, BaselineCompileError> {
        if self.vm_binding_id.is_some() {
            return Err(BaselineCompileError::VmArtifactAlreadyBound);
        }
        self.vm_binding_id = Some(binding_id);
        Ok(self)
    }
}

/// Exact generated-code contract for one verified instruction.
///
/// `SideExit` is itself an admitted terminal lowering: it executes no bytecode effect and cannot
/// contribute an edge to a native cycle. All other variants have explicit dynamic guards where a
/// boxed type matters; a failed guard exits at the current instruction before its destination is
/// mutated.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NativeInstructionKind {
    Constant,
    Move,
    SmiBinary,
    SmiImmediate,
    SmiUnary,
    SmiComparison,
    StrictSmiEquality,
    LogicalNot,
    NewObjectZero,
    Jump,
    ExactBooleanBranch,
    ToBooleanBranch,
    UndefinedBranch,
    NullishBranch,
    Return,
    SideExit,
}

impl NativeInstructionKind {
    const fn emits_branch(self) -> bool {
        matches!(
            self,
            Self::Jump
                | Self::ExactBooleanBranch
                | Self::ToBooleanBranch
                | Self::UndefinedBranch
                | Self::NullishBranch
        )
    }
}

struct CompilationPlan {
    native_kinds: Vec<NativeInstructionKind>,
    consumed_value_is_proven_js: Vec<bool>,
    backedge_poll_for_instruction: Vec<Option<u32>>,
    backedge_poll_count: usize,
    safepoint_for_instruction: Vec<Option<u32>>,
    records: Vec<SafepointRecord>,
    live_slots: Vec<u32>,
}

impl CompilationPlan {
    fn analyze(bytecode: &VerifiedBytecode<'_>) -> Result<Self, BaselineCompileError> {
        let instruction_count = bytecode.instructions().len();
        let mut native_kinds = Vec::new();
        native_kinds
            .try_reserve_exact(instruction_count)
            .map_err(|_| BaselineCompileError::AllocationFailed)?;
        let mut backedge_poll_for_instruction = Vec::new();
        backedge_poll_for_instruction
            .try_reserve_exact(instruction_count)
            .map_err(|_| BaselineCompileError::AllocationFailed)?;
        backedge_poll_for_instruction.resize(instruction_count, None);
        let mut backedge_poll_count = 0_usize;

        for instruction in bytecode.instructions() {
            let kind = classify_native_instruction(instruction)?;
            native_kinds.push(kind);
        }

        // Native reachability stops at the first unsupported instruction on each path. Do not
        // promise callsites in bytecode which can execute only after a VM handoff: Cranelift
        // correctly deletes those generated blocks, and the resumed VM owns their backedge polls.
        // Reclassifying unreachable instructions as terminal side exits also keeps emission,
        // provenance, and release callsite accounting on one exact reachable CFG.
        let native_reachable = mark_native_reachable(bytecode, &native_kinds)?;
        for (instruction_index, instruction) in bytecode.instructions().iter().enumerate() {
            if native_reachable[instruction_index] == 0 {
                native_kinds[instruction_index] = NativeInstructionKind::SideExit;
                continue;
            }
            let kind = native_kinds[instruction_index];
            if instruction.effects.contains(EffectFlags::BACKEDGE) && kind.emits_branch() {
                let poll_index = u32::try_from(backedge_poll_count).map_err(|_| {
                    BaselineCompileError::BackedgePollSourceLocationOverflow(backedge_poll_count)
                })?;
                backedge_poll_for_instruction[instruction_index] = Some(poll_index);
                backedge_poll_count = backedge_poll_count
                    .checked_add(1)
                    .ok_or(BaselineCompileError::BackedgePollSourceLocationOverflow(usize::MAX))?;
            }
        }

        let words_per_set = bytecode.num_locals().div_ceil(u64::BITS as usize);
        let cells = instruction_count.checked_mul(words_per_set).ok_or(
            BaselineCompileError::LivenessAnalysisTooLarge {
                actual: usize::MAX,
                maximum: MAX_LIVENESS_ANALYSIS_BYTES,
            },
        )?;
        let analysis_bytes = cells
            .checked_mul(size_of::<u64>())
            .and_then(|bytes| bytes.checked_mul(3))
            .ok_or(BaselineCompileError::LivenessAnalysisTooLarge {
                actual: usize::MAX,
                maximum: MAX_LIVENESS_ANALYSIS_BYTES,
            })?;
        if analysis_bytes > MAX_LIVENESS_ANALYSIS_BYTES {
            return Err(BaselineCompileError::LivenessAnalysisTooLarge {
                actual: analysis_bytes,
                maximum: MAX_LIVENESS_ANALYSIS_BYTES,
            });
        }

        let mut live_in = zeroed_words(cells)?;
        let mut uses = zeroed_words(cells)?;
        let mut defs = zeroed_words(cells)?;
        for (instruction_index, instruction) in bytecode.instructions().iter().enumerate() {
            populate_use_def(instruction, instruction_index, words_per_set, &mut uses, &mut defs)?;
        }

        let mut predecessors = Vec::new();
        predecessors
            .try_reserve_exact(instruction_count)
            .map_err(|_| BaselineCompileError::AllocationFailed)?;
        predecessors.resize_with(instruction_count, Vec::new);
        for index in 0..instruction_count {
            for successor in instruction_successors(bytecode, index)
                .into_iter()
                .flatten()
            {
                predecessors[successor]
                    .try_reserve(1)
                    .map_err(|_| BaselineCompileError::AllocationFailed)?;
                predecessors[successor].push(index);
            }
        }

        let mut worklist = Vec::new();
        worklist
            .try_reserve_exact(instruction_count)
            .map_err(|_| BaselineCompileError::AllocationFailed)?;
        worklist.extend((0..instruction_count).rev());
        let mut queued = Vec::new();
        queued
            .try_reserve_exact(instruction_count)
            .map_err(|_| BaselineCompileError::AllocationFailed)?;
        queued.resize(instruction_count, true);
        let mut live_out = zeroed_words(words_per_set)?;

        let mut worklist_dequeues = 0_usize;
        while let Some(index) = worklist.pop() {
            charge_liveness_work(&mut worklist_dequeues)?;
            queued[index] = false;
            live_out.fill(0);
            for successor in instruction_successors(bytecode, index)
                .into_iter()
                .flatten()
            {
                let successor_start = successor * words_per_set;
                for word in 0..words_per_set {
                    live_out[word] |= live_in[successor_start + word];
                }
            }

            let row_start = index * words_per_set;
            let mut changed = false;
            for word in 0..words_per_set {
                let next = uses[row_start + word] | (live_out[word] & !defs[row_start + word]);
                if live_in[row_start + word] != next {
                    live_in[row_start + word] = next;
                    changed = true;
                }
            }
            if changed {
                for &predecessor in &predecessors[index] {
                    if !queued[predecessor] {
                        queued[predecessor] = true;
                        worklist.push(predecessor);
                    }
                }
            }
        }

        let mut safepoint_for_instruction = Vec::new();
        safepoint_for_instruction
            .try_reserve_exact(instruction_count)
            .map_err(|_| BaselineCompileError::AllocationFailed)?;
        safepoint_for_instruction.resize(instruction_count, None);
        let mut records = Vec::new();
        let mut live_slots = Vec::new();

        for (instruction_index, instruction) in bytecode.instructions().iter().enumerate() {
            if native_kinds[instruction_index] != NativeInstructionKind::NewObjectZero {
                continue;
            }
            let Some(result_slot) = local_index(instruction.operands[0], instruction.width) else {
                return Err(BaselineCompileError::UnsupportedNonLocalRegister {
                    bytecode_offset: instruction.offset,
                });
            };
            if records.len() == MAX_SAFEPOINT_RECORDS {
                return Err(BaselineCompileError::TooManySafepoints {
                    actual: records.len() + 1,
                    maximum: MAX_SAFEPOINT_RECORDS,
                });
            }

            let live_start = live_slots.len();
            let row_start = instruction_index * words_per_set;
            for slot in 0..bytecode.num_locals() {
                let word = slot / u64::BITS as usize;
                let bit = slot % u64::BITS as usize;
                if (live_in[row_start + word] & (1_u64 << bit)) != 0 {
                    if live_slots.len() == MAX_LIVE_ROOT_ENTRIES {
                        return Err(BaselineCompileError::TooManyLiveRoots {
                            actual: live_slots.len() + 1,
                            maximum: MAX_LIVE_ROOT_ENTRIES,
                        });
                    }
                    live_slots.push(
                        u32::try_from(slot)
                            .map_err(|_| BaselineCompileError::SlotOffsetTooLarge(slot))?,
                    );
                }
            }
            let safepoint_index = records.len();
            safepoint_for_instruction[instruction_index] =
                Some(u32::try_from(safepoint_index).map_err(|_| {
                    BaselineCompileError::TooManySafepoints {
                        actual: safepoint_index + 1,
                        maximum: MAX_SAFEPOINT_RECORDS,
                    }
                })?);
            records.push(SafepointRecord {
                native_return_offset: 0,
                bytecode_offset: u32::try_from(instruction.offset).map_err(|_| {
                    BaselineCompileError::BytecodeOffsetTooLarge(instruction.offset)
                })?,
                live_slot_start: u32::try_from(live_start).map_err(|_| {
                    BaselineCompileError::TooManyLiveRoots {
                        actual: live_start,
                        maximum: MAX_LIVE_ROOT_ENTRIES,
                    }
                })?,
                live_slot_count: u32::try_from(live_slots.len() - live_start).map_err(|_| {
                    BaselineCompileError::TooManyLiveRoots {
                        actual: live_slots.len(),
                        maximum: MAX_LIVE_ROOT_ENTRIES,
                    }
                })?,
                result_slot: u32::try_from(result_slot)
                    .map_err(|_| BaselineCompileError::SlotOffsetTooLarge(result_slot))?,
                flags: SAFEPOINT_FLAG_ALLOCATING_HELPER,
            });
        }

        // Reuse the no-longer-needed liveness-use allocation for the native value-provenance
        // must-analysis. This keeps peak bit-matrix storage within the three matrices charged by
        // `MAX_LIVENESS_ANALYSIS_BYTES` rather than quietly adding a fourth one.
        drop(defs);
        let consumed_value_is_proven_js =
            analyze_native_js_provenance(bytecode, &native_kinds, uses)?;

        Ok(Self {
            native_kinds,
            consumed_value_is_proven_js,
            backedge_poll_for_instruction,
            backedge_poll_count,
            safepoint_for_instruction,
            records,
            live_slots,
        })
    }
}

fn mark_native_reachable(
    bytecode: &VerifiedBytecode<'_>,
    native_kinds: &[NativeInstructionKind],
) -> Result<Vec<u8>, BaselineCompileError> {
    let instruction_count = bytecode.instructions().len();
    if native_kinds.len() != instruction_count {
        return Err(BaselineCompileError::NativeValueAnalysisShapeMismatch {
            expected: instruction_count,
            actual: native_kinds.len(),
        });
    }

    let mut reachable = zeroed_flags(instruction_count)?;
    let mut worklist = Vec::new();
    worklist
        .try_reserve_exact(instruction_count)
        .map_err(|_| BaselineCompileError::AllocationFailed)?;
    reachable[0] = 1;
    worklist.push(0);
    while let Some(index) = worklist.pop() {
        for successor in native_instruction_successors(bytecode, native_kinds[index], index)
            .into_iter()
            .flatten()
        {
            if reachable[successor] == 0 {
                reachable[successor] = 1;
                worklist.push(successor);
            }
        }
    }
    Ok(reachable)
}

fn charge_liveness_work(dequeues: &mut usize) -> Result<(), BaselineCompileError> {
    *dequeues =
        dequeues
            .checked_add(1)
            .ok_or(BaselineCompileError::LivenessAnalysisWorkLimitExceeded {
                maximum: MAX_LIVENESS_WORKLIST_DEQUEUES,
            })?;
    if *dequeues > MAX_LIVENESS_WORKLIST_DEQUEUES {
        return Err(BaselineCompileError::LivenessAnalysisWorkLimitExceeded {
            maximum: MAX_LIVENESS_WORKLIST_DEQUEUES,
        });
    }
    Ok(())
}

/// Prove which native inputs are ECMAScript values on every generated path reaching them.
///
/// Entry slots are deliberately unknown: `JitSlot` proves representation and heap-allocation
/// identity, but Brimstone stores engine metadata in the same pointer-shaped `Value` carrier. A
/// successful guarded native operation or an exact JS-producing load establishes provenance;
/// `Mov` preserves it and joins intersect it. Side exits have no generated successor. The returned
/// booleans are meaningful for pointer-capable consumers (`Ret` and the undefined/nullish branch
/// families): they allow a proven `NewObject` result without allowing an arbitrary metadata pointer
/// supplied at entry to become observable.
fn analyze_native_js_provenance(
    bytecode: &VerifiedBytecode<'_>,
    native_kinds: &[NativeInstructionKind],
    mut states: Vec<u64>,
) -> Result<Vec<bool>, BaselineCompileError> {
    let instruction_count = bytecode.instructions().len();
    let words_per_set = bytecode.num_locals().div_ceil(u64::BITS as usize);
    let expected_cells = instruction_count.checked_mul(words_per_set).ok_or(
        BaselineCompileError::LivenessAnalysisTooLarge {
            actual: usize::MAX,
            maximum: MAX_LIVENESS_ANALYSIS_BYTES,
        },
    )?;
    if states.len() != expected_cells {
        return Err(BaselineCompileError::NativeValueAnalysisShapeMismatch {
            expected: expected_cells,
            actual: states.len(),
        });
    }
    if native_kinds.len() != instruction_count {
        return Err(BaselineCompileError::NativeValueAnalysisShapeMismatch {
            expected: instruction_count,
            actual: native_kinds.len(),
        });
    }
    states.fill(0);

    let mut reached = zeroed_flags(instruction_count)?;
    let mut queued = zeroed_flags(instruction_count)?;
    let mut worklist = Vec::new();
    worklist
        .try_reserve_exact(instruction_count)
        .map_err(|_| BaselineCompileError::AllocationFailed)?;
    let mut outgoing = zeroed_words(words_per_set)?;
    let mut consumed_value_is_proven_js = Vec::new();
    consumed_value_is_proven_js
        .try_reserve_exact(instruction_count)
        .map_err(|_| BaselineCompileError::AllocationFailed)?;
    consumed_value_is_proven_js.resize(instruction_count, false);

    reached[0] = 1;
    queued[0] = 1;
    worklist.push(0);
    let mut dequeues = 0_usize;
    while let Some(index) = worklist.pop() {
        charge_native_value_work(&mut dequeues)?;
        queued[index] = 0;
        let row_start = index * words_per_set;
        outgoing.copy_from_slice(&states[row_start..row_start + words_per_set]);
        let instruction = &bytecode.instructions()[index];

        if matches!(
            native_kinds[index],
            NativeInstructionKind::Return
                | NativeInstructionKind::UndefinedBranch
                | NativeInstructionKind::NullishBranch
        ) {
            let source = local_index(instruction.operands[0], instruction.width).ok_or(
                BaselineCompileError::UnsupportedNonLocalRegister {
                    bytecode_offset: instruction.offset,
                },
            )?;
            consumed_value_is_proven_js[index] = provenance_bit_is_set(&outgoing, source);
        }
        transfer_native_js_provenance(instruction, native_kinds[index], &mut outgoing)?;

        for successor in native_instruction_successors(bytecode, native_kinds[index], index)
            .into_iter()
            .flatten()
        {
            let successor_start = successor * words_per_set;
            let successor_state = &mut states[successor_start..successor_start + words_per_set];
            let mut changed = reached[successor] == 0;
            if changed {
                successor_state.copy_from_slice(&outgoing);
                reached[successor] = 1;
            } else {
                for (known, incoming) in successor_state.iter_mut().zip(&outgoing) {
                    let joined = *known & *incoming;
                    changed |= joined != *known;
                    *known = joined;
                }
            }
            if changed && queued[successor] == 0 {
                queued[successor] = 1;
                worklist.push(successor);
            }
        }
    }

    Ok(consumed_value_is_proven_js)
}

fn charge_native_value_work(dequeues: &mut usize) -> Result<(), BaselineCompileError> {
    *dequeues = dequeues.checked_add(1).ok_or(
        BaselineCompileError::NativeValueAnalysisWorkLimitExceeded {
            maximum: MAX_NATIVE_VALUE_WORKLIST_DEQUEUES,
        },
    )?;
    if *dequeues > MAX_NATIVE_VALUE_WORKLIST_DEQUEUES {
        return Err(BaselineCompileError::NativeValueAnalysisWorkLimitExceeded {
            maximum: MAX_NATIVE_VALUE_WORKLIST_DEQUEUES,
        });
    }
    Ok(())
}

fn zeroed_flags(len: usize) -> Result<Vec<u8>, BaselineCompileError> {
    let mut flags = Vec::new();
    flags
        .try_reserve_exact(len)
        .map_err(|_| BaselineCompileError::AllocationFailed)?;
    flags.resize(len, 0);
    Ok(flags)
}

fn transfer_native_js_provenance(
    instruction: &VerifiedInstruction,
    kind: NativeInstructionKind,
    state: &mut [u64],
) -> Result<(), BaselineCompileError> {
    match kind {
        NativeInstructionKind::Constant => {
            let dest = local_index(instruction.operands[0], instruction.width).ok_or(
                BaselineCompileError::UnsupportedNonLocalRegister {
                    bytecode_offset: instruction.offset,
                },
            )?;
            assign_provenance_bit(state, dest, instruction.opcode != OpCode::LoadEmpty);
        }
        NativeInstructionKind::Move => {
            let dest = local_index(instruction.operands[0], instruction.width).ok_or(
                BaselineCompileError::UnsupportedNonLocalRegister {
                    bytecode_offset: instruction.offset,
                },
            )?;
            let source = local_index(instruction.operands[1], instruction.width).ok_or(
                BaselineCompileError::UnsupportedNonLocalRegister {
                    bytecode_offset: instruction.offset,
                },
            )?;
            let source_is_proven = provenance_bit_is_set(state, source);
            assign_provenance_bit(state, dest, source_is_proven);
        }
        NativeInstructionKind::SmiBinary
        | NativeInstructionKind::SmiImmediate
        | NativeInstructionKind::SmiUnary
        | NativeInstructionKind::SmiComparison
        | NativeInstructionKind::StrictSmiEquality
        | NativeInstructionKind::LogicalNot
        | NativeInstructionKind::NewObjectZero => {
            let dest = local_index(instruction.operands[0], instruction.width).ok_or(
                BaselineCompileError::UnsupportedNonLocalRegister {
                    bytecode_offset: instruction.offset,
                },
            )?;
            assign_provenance_bit(state, dest, true);
        }
        NativeInstructionKind::Jump
        | NativeInstructionKind::ExactBooleanBranch
        | NativeInstructionKind::ToBooleanBranch
        | NativeInstructionKind::UndefinedBranch
        | NativeInstructionKind::NullishBranch
        | NativeInstructionKind::Return
        | NativeInstructionKind::SideExit => {}
    }
    Ok(())
}

fn native_instruction_successors(
    bytecode: &VerifiedBytecode<'_>,
    kind: NativeInstructionKind,
    index: usize,
) -> [Option<usize>; 2] {
    if matches!(kind, NativeInstructionKind::Return | NativeInstructionKind::SideExit) {
        [None, None]
    } else {
        instruction_successors(bytecode, index)
    }
}

fn provenance_bit_is_set(words: &[u64], slot: usize) -> bool {
    let word = slot / u64::BITS as usize;
    let bit = slot % u64::BITS as usize;
    (words[word] & (1_u64 << bit)) != 0
}

fn assign_provenance_bit(words: &mut [u64], slot: usize, proven: bool) {
    let word = slot / u64::BITS as usize;
    let bit = slot % u64::BITS as usize;
    if proven {
        words[word] |= 1_u64 << bit;
    } else {
        words[word] &= !(1_u64 << bit);
    }
}

fn classify_native_instruction(
    instruction: &VerifiedInstruction,
) -> Result<NativeInstructionKind, BaselineCompileError> {
    let metadata = instruction.opcode.metadata();
    let mut exact_effects = metadata.effects;
    if instruction
        .branch_target
        .is_some_and(|target| target <= instruction.offset)
    {
        exact_effects = exact_effects
            .union(EffectFlags::BACKEDGE)
            .union(EffectFlags::SAFEPOINT);
    }
    if instruction.effects != exact_effects {
        return Err(BaselineCompileError::NativeEffectContractMismatch {
            bytecode_offset: instruction.offset,
            opcode: instruction.opcode,
        });
    }

    let kind = match instruction.opcode {
        OpCode::LoadImmediate
        | OpCode::LoadUndefined
        | OpCode::LoadNull
        | OpCode::LoadEmpty
        | OpCode::LoadTrue
        | OpCode::LoadFalse => NativeInstructionKind::Constant,
        OpCode::Mov => NativeInstructionKind::Move,
        OpCode::Add
        | OpCode::Sub
        | OpCode::Mul
        | OpCode::BitAnd
        | OpCode::BitOr
        | OpCode::BitXor
        | OpCode::ShiftLeft
        | OpCode::ShiftRightArithmetic
        | OpCode::ShiftRightLogical => NativeInstructionKind::SmiBinary,
        OpCode::AddImm
        | OpCode::SubImm
        | OpCode::MulImm
        | OpCode::BitAndImm
        | OpCode::BitOrImm
        | OpCode::BitXorImm
        | OpCode::ShiftLeftImm
        | OpCode::ShiftRightArithmeticImm
        | OpCode::ShiftRightLogicalImm => NativeInstructionKind::SmiImmediate,
        OpCode::Neg | OpCode::Inc | OpCode::Dec | OpCode::BitNot => NativeInstructionKind::SmiUnary,
        OpCode::LessThan
        | OpCode::LessThanOrEqual
        | OpCode::GreaterThan
        | OpCode::GreaterThanOrEqual => NativeInstructionKind::SmiComparison,
        OpCode::StrictEqual | OpCode::StrictNotEqual => NativeInstructionKind::StrictSmiEquality,
        OpCode::LogNot => NativeInstructionKind::LogicalNot,
        OpCode::NewObject if instruction.operands[1].as_unsigned() == 0 => {
            NativeInstructionKind::NewObjectZero
        }
        OpCode::Jump | OpCode::JumpConstant => NativeInstructionKind::Jump,
        OpCode::JumpTrue
        | OpCode::JumpTrueConstant
        | OpCode::JumpFalse
        | OpCode::JumpFalseConstant => NativeInstructionKind::ExactBooleanBranch,
        OpCode::JumpToBooleanTrue
        | OpCode::JumpToBooleanTrueConstant
        | OpCode::JumpToBooleanFalse
        | OpCode::JumpToBooleanFalseConstant => NativeInstructionKind::ToBooleanBranch,
        OpCode::JumpNotUndefined | OpCode::JumpNotUndefinedConstant => {
            NativeInstructionKind::UndefinedBranch
        }
        OpCode::JumpNullish
        | OpCode::JumpNullishConstant
        | OpCode::JumpNotNullish
        | OpCode::JumpNotNullishConstant => NativeInstructionKind::NullishBranch,
        OpCode::Ret => NativeInstructionKind::Return,
        _ => NativeInstructionKind::SideExit,
    };
    Ok(kind)
}

fn zeroed_words(len: usize) -> Result<Vec<u64>, BaselineCompileError> {
    let mut words = Vec::new();
    words
        .try_reserve_exact(len)
        .map_err(|_| BaselineCompileError::AllocationFailed)?;
    words.resize(len, 0);
    Ok(words)
}

fn populate_use_def(
    instruction: &VerifiedInstruction,
    instruction_index: usize,
    words_per_set: usize,
    uses: &mut [u64],
    defs: &mut [u64],
) -> Result<(), BaselineCompileError> {
    let row_start = instruction_index * words_per_set;
    for (operand_index, &access) in instruction
        .opcode
        .metadata()
        .operand_accesses
        .iter()
        .enumerate()
    {
        if access == OperandAccess::None {
            continue;
        }
        let Some(first_slot) = local_index(instruction.operands[operand_index], instruction.width)
        else {
            return Err(BaselineCompileError::UnsupportedNonLocalRegister {
                bytecode_offset: instruction.offset,
            });
        };

        match access {
            OperandAccess::Read => set_bit(uses, row_start, first_slot),
            OperandAccess::Write => set_bit(defs, row_start, first_slot),
            OperandAccess::ReadWrite => {
                set_bit(uses, row_start, first_slot);
                set_bit(defs, row_start, first_slot);
            }
            OperandAccess::ReadRange { length_operand } => {
                let count = instruction.operands[length_operand as usize].as_unsigned();
                let Some(end_slot) = first_slot.checked_add(count) else {
                    return Err(BaselineCompileError::UnsupportedNonLocalRegister {
                        bytecode_offset: instruction.offset,
                    });
                };
                for slot in first_slot..end_slot {
                    set_bit(uses, row_start, slot);
                }
            }
            OperandAccess::ReadClassMethods { .. } => {
                return Err(BaselineCompileError::UnsupportedClassMethodLiveness {
                    bytecode_offset: instruction.offset,
                });
            }
            OperandAccess::None => unreachable!(),
        }
    }
    Ok(())
}

fn set_bit(words: &mut [u64], row_start: usize, slot: usize) {
    let word = slot / u64::BITS as usize;
    let bit = slot % u64::BITS as usize;
    words[row_start + word] |= 1_u64 << bit;
}

fn instruction_successors(bytecode: &VerifiedBytecode<'_>, index: usize) -> [Option<usize>; 2] {
    let instruction = &bytecode.instructions()[index];
    let fallthrough = || (index + 1 < bytecode.instructions().len()).then_some(index + 1);
    let target = || {
        instruction.branch_target.map(|offset| {
            bytecode
                .instructions()
                .binary_search_by_key(&offset, |candidate| candidate.offset)
                .expect("verified branch target")
        })
    };
    match instruction.opcode.metadata().control_flow {
        ControlFlow::Jump => [target(), None],
        ControlFlow::ConditionalJump => [target(), fallthrough()],
        ControlFlow::Return | ControlFlow::Throw => [None, None],
        ControlFlow::Prefix | ControlFlow::Fallthrough | ControlFlow::Suspend => {
            [fallthrough(), None]
        }
    }
}

pub(crate) fn compile_prototype(
    bytecode: &VerifiedBytecode<'_>,
) -> Result<PreparedPrototype, BaselineCompileError> {
    if bytecode.instructions().len() > MAX_PROTOTYPE_INSTRUCTIONS {
        return Err(BaselineCompileError::TooManyInstructions {
            actual: bytecode.instructions().len(),
            maximum: MAX_PROTOTYPE_INSTRUCTIONS,
        });
    }
    if bytecode.num_locals() > MAX_PROTOTYPE_FRAME_SLOTS {
        return Err(BaselineCompileError::TooManyFrameSlots {
            actual: bytecode.num_locals(),
            maximum: MAX_PROTOTYPE_FRAME_SLOTS,
        });
    }
    if let Some(offset) = bytecode
        .instructions()
        .iter()
        .map(|instruction| instruction.offset)
        .find(|&offset| u32::try_from(offset).is_err())
    {
        return Err(BaselineCompileError::BytecodeOffsetTooLarge(offset));
    }

    let mut plan = CompilationPlan::analyze(bytecode)?;
    let isa = target_isa()?;
    if isa.pointer_type() != types::I64 {
        return Err(BaselineCompileError::HostUnsupported(
            "baseline prototype requires 64-bit pointers",
        ));
    }
    if isa.default_call_conv() != CallConv::SystemV {
        return Err(BaselineCompileError::HostUnsupported(
            "baseline prototype requires the System V C ABI",
        ));
    }

    let mut signature = Signature::new(CallConv::SystemV);
    signature.params.push(AbiParam::new(types::I64));
    signature.returns.push(AbiParam::new(types::I32));
    let mut function = Function::with_name_signature(UserFuncName::user(0, 0), signature);
    let mut builder_context = FunctionBuilderContext::new();

    {
        let mut builder = FunctionBuilder::new(&mut function, &mut builder_context);
        build_function(&mut builder, bytecode, &plan)?;
        builder.seal_all_blocks();
        builder.finalize(isa.frontend_config());
    }

    let mut context = Context::for_function(function);
    let mut control_plane = ControlPlane::default();
    let compiled = context
        .compile(isa.as_ref(), &mut control_plane)
        .map_err(|error| BaselineCompileError::Codegen(format!("{error:?}")))?;

    if !compiled.buffer.relocs().is_empty() {
        return Err(BaselineCompileError::UnexpectedRelocations(compiled.buffer.relocs().len()));
    }
    if !compiled.buffer.traps().is_empty() {
        return Err(BaselineCompileError::UnexpectedTraps(compiled.buffer.traps().len()));
    }
    if compiled.code_buffer().len() > MAX_PROTOTYPE_CODE_BYTES {
        return Err(BaselineCompileError::MachineCodeTooLarge {
            actual: compiled.code_buffer().len(),
            maximum: MAX_PROTOTYPE_CODE_BYTES,
        });
    }

    let mut seen_safepoints = Vec::new();
    seen_safepoints
        .try_reserve_exact(plan.records.len())
        .map_err(|_| BaselineCompileError::AllocationFailed)?;
    seen_safepoints.resize(plan.records.len(), false);
    let mut seen_backedge_polls = Vec::new();
    seen_backedge_polls
        .try_reserve_exact(plan.backedge_poll_count)
        .map_err(|_| BaselineCompileError::AllocationFailed)?;
    seen_backedge_polls.resize(plan.backedge_poll_count, false);
    let mut safepoint_call_count = 0_usize;
    let mut backedge_poll_call_count = 0_usize;
    for call_site in compiled.buffer.call_sites() {
        let native_return_offset = call_site.ret_addr;
        let Some(call_byte_offset) = native_return_offset.checked_sub(1) else {
            return Err(BaselineCompileError::MissingGeneratedCallSourceLocation {
                native_return_offset,
            });
        };
        let Some(source_range) =
            compiled.buffer.get_srclocs_sorted().iter().find(|mapping| {
                mapping.start <= call_byte_offset && call_byte_offset < mapping.end
            })
        else {
            return Err(BaselineCompileError::MissingGeneratedCallSourceLocation {
                native_return_offset,
            });
        };
        let source_bits = source_range.loc.bits();
        if source_bits >= SAFEPOINT_SOURCE_LOC_BASE {
            safepoint_call_count = safepoint_call_count.checked_add(1).ok_or(
                BaselineCompileError::SafepointCallCountMismatch {
                    expected: plan.records.len(),
                    actual: usize::MAX,
                },
            )?;
            let raw_index = source_bits - SAFEPOINT_SOURCE_LOC_BASE;
            let safepoint_index = usize::try_from(raw_index)
                .map_err(|_| BaselineCompileError::InvalidSafepointSourceLocation(source_bits))?;
            let Some(record) = plan.records.get_mut(safepoint_index) else {
                return Err(BaselineCompileError::InvalidSafepointSourceLocation(source_bits));
            };
            if seen_safepoints[safepoint_index] {
                return Err(BaselineCompileError::DuplicateSafepointCall(safepoint_index));
            }
            seen_safepoints[safepoint_index] = true;
            record.native_return_offset = native_return_offset;
        } else if source_bits >= BACKEDGE_POLL_SOURCE_LOC_BASE {
            backedge_poll_call_count = backedge_poll_call_count.checked_add(1).ok_or(
                BaselineCompileError::BackedgePollCallCountMismatch {
                    expected: plan.backedge_poll_count,
                    actual: usize::MAX,
                },
            )?;
            let raw_index = source_bits - BACKEDGE_POLL_SOURCE_LOC_BASE;
            let poll_index = usize::try_from(raw_index).map_err(|_| {
                BaselineCompileError::InvalidBackedgePollSourceLocation(source_bits)
            })?;
            let Some(seen) = seen_backedge_polls.get_mut(poll_index) else {
                return Err(BaselineCompileError::InvalidBackedgePollSourceLocation(source_bits));
            };
            if *seen {
                return Err(BaselineCompileError::DuplicateBackedgePollCall(poll_index));
            }
            *seen = true;
        } else {
            return Err(BaselineCompileError::InvalidBackedgePollSourceLocation(source_bits));
        }
    }
    if safepoint_call_count != plan.records.len() || seen_safepoints.iter().any(|seen| !seen) {
        return Err(BaselineCompileError::SafepointCallCountMismatch {
            expected: plan.records.len(),
            actual: safepoint_call_count,
        });
    }
    if backedge_poll_call_count != plan.backedge_poll_count
        || seen_backedge_polls.iter().any(|seen| !seen)
    {
        return Err(BaselineCompileError::BackedgePollCallCountMismatch {
            expected: plan.backedge_poll_count,
            actual: backedge_poll_call_count,
        });
    }

    let mut instruction_starts = Vec::new();
    instruction_starts
        .try_reserve_exact(bytecode.instructions().len())
        .map_err(|_| BaselineCompileError::AllocationFailed)?;
    for instruction in bytecode.instructions() {
        instruction_starts.push(
            u32::try_from(instruction.offset)
                .map_err(|_| BaselineCompileError::BytecodeOffsetTooLarge(instruction.offset))?,
        );
    }
    let safepoints = SafepointMetadata::new(
        bytecode.num_locals(),
        bytecode.bytes().len(),
        compiled.code_buffer().len(),
        plan.records,
        plan.live_slots,
        instruction_starts,
    )
    .map_err(BaselineCompileError::SafepointMetadata)?;

    let mut machine_code = Vec::new();
    machine_code
        .try_reserve_exact(compiled.code_buffer().len())
        .map_err(|_| BaselineCompileError::AllocationFailed)?;
    machine_code.extend_from_slice(compiled.code_buffer());

    let program = PreparedProgram::capture(bytecode)?;

    Ok(PreparedPrototype {
        machine_code,
        required_frame_slots: bytecode.num_locals(),
        safepoints,
        program,
        vm_binding_id: None,
    })
}

fn target_isa() -> Result<OwnedTargetIsa, BaselineCompileError> {
    let mut flags_builder = settings::builder();
    flags_builder
        .set("opt_level", "speed_and_size")
        .map_err(|error| BaselineCompileError::Setting(error.to_string()))?;
    flags_builder
        .enable("enable_verifier")
        .map_err(|error| BaselineCompileError::Setting(error.to_string()))?;
    flags_builder
        .enable("preserve_frame_pointers")
        .map_err(|error| BaselineCompileError::Setting(error.to_string()))?;
    let flags = settings::Flags::new(flags_builder);

    // No host-specific feature inference: this code uses only the x86-64 baseline selected by the
    // pinned Cranelift backend and is never persisted across machines.
    let isa_builder = cranelift_native::builder_with_options(false)
        .map_err(BaselineCompileError::HostUnsupported)?;
    isa_builder
        .finish(flags)
        .map_err(|error| BaselineCompileError::Target(error.to_string()))
}

fn build_function(
    builder: &mut FunctionBuilder<'_>,
    bytecode: &VerifiedBytecode<'_>,
    plan: &CompilationPlan,
) -> Result<(), BaselineCompileError> {
    let entry_block = builder.create_block();
    let activation_header_block = builder.create_block();
    let helper_header_block = builder.create_block();
    let frame_header_block = builder.create_block();
    let invalid_activation_block = builder.create_block();

    let mut instruction_blocks = Vec::new();
    instruction_blocks
        .try_reserve_exact(bytecode.instructions().len())
        .map_err(|_| BaselineCompileError::AllocationFailed)?;
    for _ in bytecode.instructions() {
        instruction_blocks.push(builder.create_block());
    }

    let activation_var = builder.declare_var(types::I64);
    let frame_var = builder.declare_var(types::I64);
    let new_object_helper_var = builder.declare_var(types::I64);
    let backedge_poll_helper_var = builder.declare_var(types::I64);
    let slots_var = builder.declare_var(types::I64);
    let mut helper_signature = Signature::new(CallConv::SystemV);
    helper_signature.params.push(AbiParam::new(types::I64));
    helper_signature.returns.push(AbiParam::new(types::I32));
    let helper_signature = builder.import_signature(helper_signature);

    builder.append_block_params_for_function_params(entry_block);
    builder.switch_to_block(entry_block);
    let activation = builder.block_params(entry_block)[0];
    builder.def_var(activation_var, activation);
    let activation_is_nonnull = builder.ins().icmp_imm_s(IntCC::NotEqual, activation, 0);
    builder.ins().brif(
        activation_is_nonnull,
        activation_header_block,
        &[],
        invalid_activation_block,
        &[],
    );

    builder.switch_to_block(activation_header_block);
    let activation = builder.use_var(activation_var);
    let mut activation_valid = compare_loaded_imm(
        builder,
        types::I32,
        activation,
        ACTIVATION_ABI_VERSION_OFFSET,
        GENERATED_CODE_ABI_VERSION as i64,
    );
    activation_valid = and_loaded_imm(
        builder,
        activation_valid,
        types::I32,
        activation,
        ACTIVATION_STRUCT_SIZE_OFFSET,
        JIT_ACTIVATION_SIZE as i64,
    );
    let memory_flags = plain_mem_flags();
    let frame = builder
        .ins()
        .load(types::I64, memory_flags, activation, ACTIVATION_FRAME_OFFSET);
    let frame_is_nonnull = builder.ins().icmp_imm_s(IntCC::NotEqual, frame, 0);
    activation_valid = and_condition(builder, activation_valid, frame_is_nonnull);
    let helpers =
        builder
            .ins()
            .load(types::I64, memory_flags, activation, ACTIVATION_HELPERS_OFFSET);
    let helpers_are_nonnull = builder.ins().icmp_imm_s(IntCC::NotEqual, helpers, 0);
    activation_valid = and_condition(builder, activation_valid, helpers_are_nonnull);
    let context =
        builder
            .ins()
            .load(types::I64, memory_flags, activation, ACTIVATION_CONTEXT_OFFSET);
    let context_is_nonnull = builder.ins().icmp_imm_s(IntCC::NotEqual, context, 0);
    activation_valid = and_condition(builder, activation_valid, context_is_nonnull);
    let interrupt_budget = builder.ins().load(
        types::I64,
        memory_flags,
        activation,
        ACTIVATION_INTERRUPT_BUDGET_OFFSET,
    );
    let interrupt_budget_is_nonnull =
        builder
            .ins()
            .icmp_imm_s(IntCC::NotEqual, interrupt_budget, 0);
    activation_valid = and_condition(builder, activation_valid, interrupt_budget_is_nonnull);
    activation_valid = and_loaded_imm(
        builder,
        activation_valid,
        types::I32,
        activation,
        ACTIVATION_SIDE_EXIT_OFFSET,
        0,
    );
    activation_valid = and_loaded_imm(
        builder,
        activation_valid,
        types::I32,
        activation,
        ACTIVATION_NATIVE_BACKEDGE_WORK_REMAINING_OFFSET,
        MAX_NATIVE_BACKEDGE_WORK_UNITS as i64,
    );
    activation_valid = and_loaded_imm(
        builder,
        activation_valid,
        types::I64,
        activation,
        ACTIVATION_RETURN_VALUE_OFFSET,
        0,
    );
    activation_valid = and_loaded_imm(
        builder,
        activation_valid,
        types::I32,
        activation,
        ACTIVATION_POISONED_OFFSET,
        0,
    );
    activation_valid = and_loaded_imm(
        builder,
        activation_valid,
        types::I32,
        activation,
        ACTIVATION_RESERVED_TAIL_OFFSET,
        0,
    );
    builder
        .ins()
        .brif(activation_valid, helper_header_block, &[], invalid_activation_block, &[]);

    builder.switch_to_block(helper_header_block);
    let mut helper_valid = compare_loaded_imm(
        builder,
        types::I32,
        helpers,
        HELPER_TABLE_ABI_VERSION_OFFSET,
        GENERATED_CODE_ABI_VERSION as i64,
    );
    helper_valid = and_loaded_imm(
        builder,
        helper_valid,
        types::I32,
        helpers,
        HELPER_TABLE_STRUCT_SIZE_OFFSET,
        JIT_HELPER_TABLE_SIZE as i64,
    );
    helper_valid =
        and_loaded_imm(builder, helper_valid, types::I64, helpers, HELPER_TABLE_RESERVED_OFFSET, 0);
    let new_object_helper =
        builder
            .ins()
            .load(types::I64, memory_flags, helpers, HELPER_TABLE_NEW_OBJECT_ZERO_OFFSET);
    let helper_is_nonnull = builder
        .ins()
        .icmp_imm_s(IntCC::NotEqual, new_object_helper, 0);
    helper_valid = and_condition(builder, helper_valid, helper_is_nonnull);
    builder.def_var(new_object_helper_var, new_object_helper);
    let backedge_poll_helper =
        builder
            .ins()
            .load(types::I64, memory_flags, helpers, HELPER_TABLE_BACKEDGE_POLL_OFFSET);
    let backedge_poll_is_nonnull =
        builder
            .ins()
            .icmp_imm_s(IntCC::NotEqual, backedge_poll_helper, 0);
    helper_valid = and_condition(builder, helper_valid, backedge_poll_is_nonnull);
    builder.def_var(backedge_poll_helper_var, backedge_poll_helper);
    builder
        .ins()
        .brif(helper_valid, frame_header_block, &[], invalid_activation_block, &[]);

    builder.switch_to_block(frame_header_block);
    let mut frame_valid = compare_loaded_imm(
        builder,
        types::I64,
        frame,
        SHADOW_FRAME_SLOT_COUNT_OFFSET,
        bytecode.num_locals() as i64,
    );
    let memory_flags = plain_mem_flags();
    let slots = builder
        .ins()
        .load(types::I64, memory_flags, frame, SHADOW_FRAME_SLOTS_OFFSET);
    let slots_are_nonnull = builder.ins().icmp_imm_s(IntCC::NotEqual, slots, 0);
    frame_valid = and_condition(builder, frame_valid, slots_are_nonnull);
    let records = builder
        .ins()
        .load(types::I64, memory_flags, frame, SHADOW_FRAME_RECORDS_OFFSET);
    let records_are_nonnull = builder.ins().icmp_imm_s(IntCC::NotEqual, records, 0);
    frame_valid = and_condition(builder, frame_valid, records_are_nonnull);
    frame_valid = and_loaded_imm(
        builder,
        frame_valid,
        types::I64,
        frame,
        SHADOW_FRAME_RECORD_COUNT_OFFSET,
        plan.records.len() as i64,
    );
    let live_slots =
        builder
            .ins()
            .load(types::I64, memory_flags, frame, SHADOW_FRAME_LIVE_SLOTS_OFFSET);
    let live_slots_are_nonnull = builder.ins().icmp_imm_s(IntCC::NotEqual, live_slots, 0);
    frame_valid = and_condition(builder, frame_valid, live_slots_are_nonnull);
    frame_valid = and_loaded_imm(
        builder,
        frame_valid,
        types::I64,
        frame,
        SHADOW_FRAME_LIVE_SLOT_COUNT_OFFSET,
        plan.live_slots.len() as i64,
    );
    frame_valid = and_loaded_imm(
        builder,
        frame_valid,
        types::I32,
        frame,
        SHADOW_FRAME_BYTECODE_OFFSET,
        NO_BYTECODE_OFFSET as i32 as i64,
    );
    frame_valid = and_loaded_imm(
        builder,
        frame_valid,
        types::I32,
        frame,
        SHADOW_FRAME_SAFEPOINT_INDEX_OFFSET,
        NO_SAFEPOINT as i32 as i64,
    );
    builder.def_var(frame_var, frame);
    builder.def_var(slots_var, slots);
    builder
        .ins()
        .brif(frame_valid, instruction_blocks[0], &[], invalid_activation_block, &[]);

    builder.switch_to_block(invalid_activation_block);
    emit_status_return(builder, STATUS_INVALID_ACTIVATION);

    for (index, instruction) in bytecode.instructions().iter().enumerate() {
        builder.switch_to_block(instruction_blocks[index]);
        emit_instruction(
            builder,
            activation_var,
            frame_var,
            new_object_helper_var,
            backedge_poll_helper_var,
            slots_var,
            helper_signature,
            bytecode,
            plan,
            &instruction_blocks,
            index,
            instruction,
        )?;
    }

    Ok(())
}

fn emit_instruction(
    builder: &mut FunctionBuilder<'_>,
    activation_var: Variable,
    frame_var: Variable,
    new_object_helper_var: Variable,
    backedge_poll_helper_var: Variable,
    slots_var: Variable,
    helper_signature: SigRef,
    bytecode: &VerifiedBytecode<'_>,
    plan: &CompilationPlan,
    blocks: &[Block],
    index: usize,
    instruction: &VerifiedInstruction,
) -> Result<(), BaselineCompileError> {
    let activation = builder.use_var(activation_var);
    let frame = builder.use_var(frame_var);
    let new_object_helper = builder.use_var(new_object_helper_var);
    let backedge_poll_helper = builder.use_var(backedge_poll_helper_var);
    let slots = builder.use_var(slots_var);

    if plan.native_kinds[index] == NativeInstructionKind::SideExit {
        emit_side_exit(builder, activation, instruction.offset)?;
        return Ok(());
    }

    match instruction.opcode {
        OpCode::LoadImmediate => {
            let Some(dest) = local_index(instruction.operands[0], instruction.width) else {
                emit_side_exit(builder, activation, instruction.offset)?;
                return Ok(());
            };
            let immediate = instruction.operands[1].as_signed(instruction.width) as i32;
            let raw = Value::raw_smi(immediate).as_raw_bits();
            store_raw_constant(builder, slots, dest, raw)?;
            jump_to_next(builder, blocks, index);
        }
        OpCode::LoadUndefined
        | OpCode::LoadNull
        | OpCode::LoadEmpty
        | OpCode::LoadTrue
        | OpCode::LoadFalse => {
            let Some(dest) = local_index(instruction.operands[0], instruction.width) else {
                emit_side_exit(builder, activation, instruction.offset)?;
                return Ok(());
            };
            let raw = match instruction.opcode {
                OpCode::LoadUndefined => Value::undefined().as_raw_bits(),
                OpCode::LoadNull => Value::null().as_raw_bits(),
                OpCode::LoadEmpty => Value::empty().as_raw_bits(),
                OpCode::LoadTrue => Value::bool(true).as_raw_bits(),
                OpCode::LoadFalse => Value::bool(false).as_raw_bits(),
                _ => unreachable!(),
            };
            store_raw_constant(builder, slots, dest, raw)?;
            jump_to_next(builder, blocks, index);
        }
        OpCode::Mov => {
            let (Some(dest), Some(src)) = (
                local_index(instruction.operands[0], instruction.width),
                local_index(instruction.operands[1], instruction.width),
            ) else {
                emit_side_exit(builder, activation, instruction.offset)?;
                return Ok(());
            };
            let raw = load_slot(builder, slots, src)?;
            store_slot(builder, slots, dest, raw)?;
            jump_to_next(builder, blocks, index);
        }
        OpCode::Add
        | OpCode::Sub
        | OpCode::Mul
        | OpCode::BitAnd
        | OpCode::BitOr
        | OpCode::BitXor
        | OpCode::ShiftLeft
        | OpCode::ShiftRightArithmetic
        | OpCode::ShiftRightLogical => {
            emit_smi_binary(builder, activation, slots, blocks, index, instruction)?
        }
        OpCode::AddImm
        | OpCode::SubImm
        | OpCode::MulImm
        | OpCode::BitAndImm
        | OpCode::BitOrImm
        | OpCode::BitXorImm
        | OpCode::ShiftLeftImm
        | OpCode::ShiftRightArithmeticImm
        | OpCode::ShiftRightLogicalImm => {
            emit_smi_immediate(builder, activation, slots, blocks, index, instruction)?
        }
        OpCode::Neg | OpCode::Inc | OpCode::Dec | OpCode::BitNot => {
            emit_smi_unary(builder, activation, slots, blocks, index, instruction)?
        }
        OpCode::LessThan
        | OpCode::LessThanOrEqual
        | OpCode::GreaterThan
        | OpCode::GreaterThanOrEqual
        | OpCode::StrictEqual
        | OpCode::StrictNotEqual => {
            emit_smi_comparison(builder, activation, slots, blocks, index, instruction)?
        }
        OpCode::LogNot => emit_logical_not(builder, activation, slots, blocks, index, instruction)?,
        OpCode::NewObject => emit_new_object_zero(
            builder,
            activation,
            frame,
            new_object_helper,
            slots,
            helper_signature,
            blocks,
            plan,
            index,
            instruction,
        )?,
        OpCode::Jump | OpCode::JumpConstant => {
            let target = instruction.branch_target.expect("verified jump target");
            emit_branch_edge(
                builder,
                activation,
                backedge_poll_helper,
                helper_signature,
                bytecode,
                plan,
                blocks,
                index,
                instruction,
                target,
            )?;
        }
        OpCode::JumpTrue
        | OpCode::JumpTrueConstant
        | OpCode::JumpFalse
        | OpCode::JumpFalseConstant => {
            let Some(condition) = local_index(instruction.operands[0], instruction.width) else {
                emit_side_exit(builder, activation, instruction.offset)?;
                return Ok(());
            };
            emit_exact_boolean_branch(
                builder,
                activation,
                backedge_poll_helper,
                slots,
                helper_signature,
                bytecode,
                plan,
                blocks,
                index,
                instruction,
                condition,
            )?;
        }
        OpCode::JumpToBooleanTrue
        | OpCode::JumpToBooleanTrueConstant
        | OpCode::JumpToBooleanFalse
        | OpCode::JumpToBooleanFalseConstant => {
            emit_to_boolean_branch(
                builder,
                activation,
                backedge_poll_helper,
                slots,
                helper_signature,
                bytecode,
                plan,
                blocks,
                index,
                instruction,
            )?;
        }
        OpCode::JumpNotUndefined | OpCode::JumpNotUndefinedConstant => {
            emit_simple_value_branch(
                builder,
                activation,
                backedge_poll_helper,
                slots,
                helper_signature,
                bytecode,
                plan,
                blocks,
                index,
                instruction,
                plan.consumed_value_is_proven_js[index],
            )?;
        }
        OpCode::JumpNullish
        | OpCode::JumpNullishConstant
        | OpCode::JumpNotNullish
        | OpCode::JumpNotNullishConstant => {
            emit_simple_value_branch(
                builder,
                activation,
                backedge_poll_helper,
                slots,
                helper_signature,
                bytecode,
                plan,
                blocks,
                index,
                instruction,
                plan.consumed_value_is_proven_js[index],
            )?;
        }
        OpCode::Ret => {
            let Some(src) = local_index(instruction.operands[0], instruction.width) else {
                emit_side_exit(builder, activation, instruction.offset)?;
                return Ok(());
            };
            let raw = load_slot(builder, slots, src)?;
            emit_checked_return(
                builder,
                activation,
                raw,
                plan.consumed_value_is_proven_js[index],
                instruction.offset,
            )?;
        }
        _ => emit_side_exit(builder, activation, instruction.offset)?,
    }

    Ok(())
}

/// Publish a native return only when its value is known to be an ECMAScript value.
///
/// Proven native producers may return pointer values such as `NewObject` results. An unproven
/// entry or join value may still return a canonical non-Empty immediate, but pointer-shaped values
/// side-exit so the rooted VM admission check can distinguish JavaScript objects/strings/symbols/
/// bigints from engine metadata. Empty always side-exits. `ActivationOwner` independently checks
/// canonical representation before entry and again before accepting a return.
fn emit_checked_return(
    builder: &mut FunctionBuilder<'_>,
    activation: ClifValue,
    raw: ClifValue,
    proven_js: bool,
    bytecode_offset: usize,
) -> Result<(), BaselineCompileError> {
    if !proven_js {
        guard_unproven_js_value(builder, activation, raw, bytecode_offset)?;
    }

    let memory_flags = plain_mem_flags();
    builder
        .ins()
        .store(memory_flags, raw, activation, ACTIVATION_RETURN_VALUE_OFFSET);
    emit_status_return(builder, STATUS_RETURNED);
    Ok(())
}

/// Permit only canonical non-Empty immediates when native provenance cannot prove a JS value.
///
/// Representation is independently checked by `ActivationOwner`. Pointer-shaped values must go
/// through the rooted VM admission check because only it safely inspects the heap-item kind and can
/// distinguish ordinary ECMAScript values from Brimstone engine metadata.
fn guard_unproven_js_value(
    builder: &mut FunctionBuilder<'_>,
    activation: ClifValue,
    raw: ClifValue,
    bytecode_offset: usize,
) -> Result<(), BaselineCompileError> {
    let tag = builder.ins().ushr_imm_u(raw, VALUE_TAG_SHIFT);
    let is_pointer = builder.ins().icmp_imm_u(IntCC::Equal, tag, 0);
    let is_empty = builder
        .ins()
        .icmp_imm_s(IntCC::Equal, raw, Value::empty().as_raw_bits() as i64);
    let is_internal = builder.ins().bor(is_pointer, is_empty);
    let is_js_immediate = builder.ins().icmp_imm_s(IntCC::Equal, is_internal, 0);
    let accepted_block = builder.create_block();
    let slow_exit_block = builder.create_block();
    builder
        .ins()
        .brif(is_js_immediate, accepted_block, &[], slow_exit_block, &[]);

    builder.switch_to_block(slow_exit_block);
    emit_side_exit(builder, activation, bytecode_offset)?;
    builder.switch_to_block(accepted_block);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn emit_branch_edge(
    builder: &mut FunctionBuilder<'_>,
    activation: ClifValue,
    backedge_poll_helper: ClifValue,
    helper_signature: SigRef,
    bytecode: &VerifiedBytecode<'_>,
    plan: &CompilationPlan,
    blocks: &[Block],
    index: usize,
    instruction: &VerifiedInstruction,
    target: usize,
) -> Result<(), BaselineCompileError> {
    let target_block = block_for_offset(bytecode, blocks, target);
    if !instruction.effects.contains(EffectFlags::BACKEDGE) {
        builder.ins().jump(target_block, &[]);
        return Ok(());
    }
    debug_assert!(target <= instruction.offset);

    let Some(poll_index) = plan.backedge_poll_for_instruction[index] else {
        // A native edge without an assigned poll site could form an uninterruptible cycle. The
        // compilation plan is private, but retain a release check at emission as defense in depth.
        return Err(BaselineCompileError::InvalidBackedgePollSourceLocation(u32::MAX));
    };
    let target_offset =
        u32::try_from(target).map_err(|_| BaselineCompileError::BytecodeOffsetTooLarge(target))?;
    let memory_flags = plain_mem_flags();
    let target_value = builder.ins().iconst(types::I32, target_offset as i64);
    builder
        .ins()
        .store(memory_flags, target_value, activation, ACTIVATION_SIDE_EXIT_OFFSET);

    let source_bits = BACKEDGE_POLL_SOURCE_LOC_BASE
        .checked_add(poll_index)
        .filter(|bits| *bits < SAFEPOINT_SOURCE_LOC_BASE)
        .ok_or(BaselineCompileError::BackedgePollSourceLocationOverflow(poll_index as usize))?;
    builder.set_srcloc(SourceLoc::new(source_bits));
    let call = builder
        .ins()
        .call_indirect(helper_signature, backedge_poll_helper, &[activation]);
    builder.set_srcloc(SourceLoc::default());
    let helper_status = builder.inst_results(call)[0];

    let success_block = builder.create_block();
    let check_interrupted_block = builder.create_block();
    let interrupted_block = builder.create_block();
    let check_side_exit_block = builder.create_block();
    let side_exit_block = builder.create_block();
    let check_poisoned_block = builder.create_block();
    let poisoned_block = builder.create_block();
    let invalid_helper_block = builder.create_block();

    let succeeded = builder
        .ins()
        .icmp_imm_s(IntCC::Equal, helper_status, HELPER_STATUS_OK);
    builder
        .ins()
        .brif(succeeded, success_block, &[], check_interrupted_block, &[]);

    builder.switch_to_block(success_block);
    let clear_offset = builder.ins().iconst(types::I32, 0);
    builder
        .ins()
        .store(memory_flags, clear_offset, activation, ACTIVATION_SIDE_EXIT_OFFSET);
    builder.ins().jump(target_block, &[]);

    builder.switch_to_block(check_interrupted_block);
    let interrupted =
        builder
            .ins()
            .icmp_imm_s(IntCC::Equal, helper_status, HELPER_STATUS_INTERRUPTED);
    builder
        .ins()
        .brif(interrupted, interrupted_block, &[], check_side_exit_block, &[]);

    builder.switch_to_block(interrupted_block);
    emit_status_return(builder, STATUS_INTERRUPTED);

    builder.switch_to_block(check_side_exit_block);
    let should_side_exit =
        builder
            .ins()
            .icmp_imm_s(IntCC::Equal, helper_status, HELPER_STATUS_SIDE_EXIT);
    builder
        .ins()
        .brif(should_side_exit, side_exit_block, &[], check_poisoned_block, &[]);

    builder.switch_to_block(side_exit_block);
    emit_status_return(builder, STATUS_SIDE_EXIT);

    builder.switch_to_block(check_poisoned_block);
    let helper_poisoned =
        builder
            .ins()
            .icmp_imm_s(IntCC::Equal, helper_status, HELPER_STATUS_POISONED);
    builder
        .ins()
        .brif(helper_poisoned, poisoned_block, &[], invalid_helper_block, &[]);

    builder.switch_to_block(poisoned_block);
    emit_status_return(builder, STATUS_POISONED);

    builder.switch_to_block(invalid_helper_block);
    let poisoned = builder.ins().iconst(types::I32, 1);
    builder
        .ins()
        .store(memory_flags, poisoned, activation, ACTIVATION_POISONED_OFFSET);
    emit_status_return(builder, STATUS_POISONED);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn emit_conditional_edge(
    builder: &mut FunctionBuilder<'_>,
    is_taken: ClifValue,
    activation: ClifValue,
    backedge_poll_helper: ClifValue,
    helper_signature: SigRef,
    bytecode: &VerifiedBytecode<'_>,
    plan: &CompilationPlan,
    blocks: &[Block],
    index: usize,
    instruction: &VerifiedInstruction,
) -> Result<(), BaselineCompileError> {
    let target = instruction
        .branch_target
        .expect("verified conditional target");
    if instruction.effects.contains(EffectFlags::BACKEDGE) {
        let taken_block = builder.create_block();
        builder
            .ins()
            .brif(is_taken, taken_block, &[], blocks[index + 1], &[]);
        builder.switch_to_block(taken_block);
        emit_branch_edge(
            builder,
            activation,
            backedge_poll_helper,
            helper_signature,
            bytecode,
            plan,
            blocks,
            index,
            instruction,
            target,
        )
    } else {
        builder.ins().brif(
            is_taken,
            block_for_offset(bytecode, blocks, target),
            &[],
            blocks[index + 1],
            &[],
        );
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_exact_boolean_branch(
    builder: &mut FunctionBuilder<'_>,
    activation: ClifValue,
    backedge_poll_helper: ClifValue,
    slots: ClifValue,
    helper_signature: SigRef,
    bytecode: &VerifiedBytecode<'_>,
    plan: &CompilationPlan,
    blocks: &[Block],
    index: usize,
    instruction: &VerifiedInstruction,
    condition: usize,
) -> Result<(), BaselineCompileError> {
    let condition_bits = load_slot(builder, slots, condition)?;
    let is_true = builder.ins().icmp_imm_s(
        IntCC::Equal,
        condition_bits,
        Value::bool(true).as_raw_bits() as i64,
    );
    let is_false = builder.ins().icmp_imm_s(
        IntCC::Equal,
        condition_bits,
        Value::bool(false).as_raw_bits() as i64,
    );
    let is_boolean = builder.ins().bor(is_true, is_false);
    let valid_block = builder.create_block();
    let slow_exit_block = builder.create_block();
    builder
        .ins()
        .brif(is_boolean, valid_block, &[], slow_exit_block, &[]);

    builder.switch_to_block(slow_exit_block);
    emit_side_exit(builder, activation, instruction.offset)?;

    builder.switch_to_block(valid_block);
    let is_taken = if matches!(instruction.opcode, OpCode::JumpTrue | OpCode::JumpTrueConstant) {
        is_true
    } else {
        is_false
    };
    emit_conditional_edge(
        builder,
        is_taken,
        activation,
        backedge_poll_helper,
        helper_signature,
        bytecode,
        plan,
        blocks,
        index,
        instruction,
    )
}

fn fast_to_boolean(builder: &mut FunctionBuilder<'_>, raw: ClifValue) -> (ClifValue, ClifValue) {
    let is_undefined =
        builder
            .ins()
            .icmp_imm_s(IntCC::Equal, raw, Value::undefined().as_raw_bits() as i64);
    let is_null = builder
        .ins()
        .icmp_imm_s(IntCC::Equal, raw, Value::null().as_raw_bits() as i64);
    let is_false =
        builder
            .ins()
            .icmp_imm_s(IntCC::Equal, raw, Value::bool(false).as_raw_bits() as i64);
    let is_true =
        builder
            .ins()
            .icmp_imm_s(IntCC::Equal, raw, Value::bool(true).as_raw_bits() as i64);
    let tag = builder.ins().ushr_imm_u(raw, VALUE_TAG_SHIFT);
    let is_smi = builder.ins().icmp_imm_u(IntCC::Equal, tag, SMI_TAG as i64);
    let payload = builder.ins().ireduce(types::I32, raw);
    let smi_nonzero = builder.ins().icmp_imm_s(IntCC::NotEqual, payload, 0);
    let truthy_smi = builder.ins().band(is_smi, smi_nonzero);
    let truthy = builder.ins().bor(is_true, truthy_smi);
    let false_or_null = builder.ins().bor(is_false, is_null);
    let false_null_or_undefined = builder.ins().bor(false_or_null, is_undefined);
    let supported_immediate = builder.ins().bor(false_null_or_undefined, is_true);
    let supported = builder.ins().bor(supported_immediate, is_smi);
    (supported, truthy)
}

#[allow(clippy::too_many_arguments)]
fn emit_to_boolean_branch(
    builder: &mut FunctionBuilder<'_>,
    activation: ClifValue,
    backedge_poll_helper: ClifValue,
    slots: ClifValue,
    helper_signature: SigRef,
    bytecode: &VerifiedBytecode<'_>,
    plan: &CompilationPlan,
    blocks: &[Block],
    index: usize,
    instruction: &VerifiedInstruction,
) -> Result<(), BaselineCompileError> {
    let Some(condition) = local_index(instruction.operands[0], instruction.width) else {
        emit_side_exit(builder, activation, instruction.offset)?;
        return Ok(());
    };
    let raw = load_slot(builder, slots, condition)?;
    let (supported, truthy) = fast_to_boolean(builder, raw);
    let valid_block = builder.create_block();
    let slow_exit_block = builder.create_block();
    builder
        .ins()
        .brif(supported, valid_block, &[], slow_exit_block, &[]);

    builder.switch_to_block(slow_exit_block);
    emit_side_exit(builder, activation, instruction.offset)?;

    builder.switch_to_block(valid_block);
    let is_taken = if matches!(
        instruction.opcode,
        OpCode::JumpToBooleanTrue | OpCode::JumpToBooleanTrueConstant
    ) {
        truthy
    } else {
        builder.ins().icmp_imm_s(IntCC::Equal, truthy, 0)
    };
    emit_conditional_edge(
        builder,
        is_taken,
        activation,
        backedge_poll_helper,
        helper_signature,
        bytecode,
        plan,
        blocks,
        index,
        instruction,
    )
}

#[allow(clippy::too_many_arguments)]
fn emit_simple_value_branch(
    builder: &mut FunctionBuilder<'_>,
    activation: ClifValue,
    backedge_poll_helper: ClifValue,
    slots: ClifValue,
    helper_signature: SigRef,
    bytecode: &VerifiedBytecode<'_>,
    plan: &CompilationPlan,
    blocks: &[Block],
    index: usize,
    instruction: &VerifiedInstruction,
    proven_js: bool,
) -> Result<(), BaselineCompileError> {
    let Some(condition) = local_index(instruction.operands[0], instruction.width) else {
        emit_side_exit(builder, activation, instruction.offset)?;
        return Ok(());
    };
    let raw = load_slot(builder, slots, condition)?;
    if !proven_js {
        guard_unproven_js_value(builder, activation, raw, instruction.offset)?;
    }
    let is_undefined =
        builder
            .ins()
            .icmp_imm_s(IntCC::Equal, raw, Value::undefined().as_raw_bits() as i64);
    let is_taken = match instruction.opcode {
        OpCode::JumpNotUndefined | OpCode::JumpNotUndefinedConstant => {
            builder.ins().icmp_imm_s(IntCC::Equal, is_undefined, 0)
        }
        OpCode::JumpNullish | OpCode::JumpNullishConstant => {
            let is_null =
                builder
                    .ins()
                    .icmp_imm_s(IntCC::Equal, raw, Value::null().as_raw_bits() as i64);
            builder.ins().bor(is_undefined, is_null)
        }
        OpCode::JumpNotNullish | OpCode::JumpNotNullishConstant => {
            let is_null =
                builder
                    .ins()
                    .icmp_imm_s(IntCC::Equal, raw, Value::null().as_raw_bits() as i64);
            let is_nullish = builder.ins().bor(is_undefined, is_null);
            builder.ins().icmp_imm_s(IntCC::Equal, is_nullish, 0)
        }
        _ => unreachable!(),
    };
    emit_conditional_edge(
        builder,
        is_taken,
        activation,
        backedge_poll_helper,
        helper_signature,
        bytecode,
        plan,
        blocks,
        index,
        instruction,
    )
}

#[allow(clippy::too_many_arguments)]
fn emit_new_object_zero(
    builder: &mut FunctionBuilder<'_>,
    activation: ClifValue,
    frame: ClifValue,
    new_object_helper: ClifValue,
    slots: ClifValue,
    helper_signature: SigRef,
    blocks: &[Block],
    plan: &CompilationPlan,
    index: usize,
    instruction: &VerifiedInstruction,
) -> Result<(), BaselineCompileError> {
    if instruction.operands[1].as_unsigned() != 0 {
        emit_side_exit(builder, activation, instruction.offset)?;
        return Ok(());
    }
    let Some(dest) = local_index(instruction.operands[0], instruction.width) else {
        emit_side_exit(builder, activation, instruction.offset)?;
        return Ok(());
    };
    let Some(safepoint_index) = plan.safepoint_for_instruction[index] else {
        return Err(BaselineCompileError::InvalidSafepointSourceLocation(u32::MAX));
    };
    let safepoint_usize = usize::try_from(safepoint_index)
        .map_err(|_| BaselineCompileError::InvalidSafepointSourceLocation(safepoint_index))?;
    let Some(record) = plan.records.get(safepoint_usize) else {
        return Err(BaselineCompileError::InvalidSafepointSourceLocation(safepoint_index));
    };
    if usize::try_from(record.result_slot).ok() != Some(dest) {
        return Err(BaselineCompileError::InvalidSafepointSourceLocation(safepoint_index));
    }

    let bytecode_offset = u32::try_from(instruction.offset)
        .map_err(|_| BaselineCompileError::BytecodeOffsetTooLarge(instruction.offset))?;
    let memory_flags = plain_mem_flags();
    let bytecode_offset_value = builder.ins().iconst(types::I32, bytecode_offset as i64);
    builder
        .ins()
        .store(memory_flags, bytecode_offset_value, frame, SHADOW_FRAME_BYTECODE_OFFSET);
    let safepoint_index_value = builder.ins().iconst(types::I32, safepoint_index as i64);
    builder.ins().store(
        memory_flags,
        safepoint_index_value,
        frame,
        SHADOW_FRAME_SAFEPOINT_INDEX_OFFSET,
    );

    let source_bits = SAFEPOINT_SOURCE_LOC_BASE
        .checked_add(safepoint_index)
        .filter(|bits| *bits != u32::MAX)
        .ok_or(BaselineCompileError::SafepointSourceLocationOverflow(safepoint_usize))?;
    builder.set_srcloc(SourceLoc::new(source_bits));
    let call = builder
        .ins()
        .call_indirect(helper_signature, new_object_helper, &[activation]);
    builder.set_srcloc(SourceLoc::default());
    let helper_status = builder.inst_results(call)[0];

    let result_check_block = builder.create_block();
    let success_block = builder.create_block();
    let check_interrupted_block = builder.create_block();
    let interrupted_block = builder.create_block();
    let check_allocation_failed_block = builder.create_block();
    let allocation_failed_block = builder.create_block();
    let check_poisoned_block = builder.create_block();
    let poisoned_block = builder.create_block();
    let invalid_helper_block = builder.create_block();

    let helper_succeeded = builder
        .ins()
        .icmp_imm_s(IntCC::Equal, helper_status, HELPER_STATUS_OK);
    builder
        .ins()
        .brif(helper_succeeded, result_check_block, &[], check_interrupted_block, &[]);

    builder.switch_to_block(result_check_block);
    let result_bits = load_slot(builder, slots, dest)?;
    let result_is_nonzero = builder.ins().icmp_imm_s(IntCC::NotEqual, result_bits, 0);
    builder
        .ins()
        .brif(result_is_nonzero, success_block, &[], invalid_helper_block, &[]);

    builder.switch_to_block(success_block);
    clear_published_safepoint(builder, frame);
    jump_to_next(builder, blocks, index);

    builder.switch_to_block(check_interrupted_block);
    let was_interrupted =
        builder
            .ins()
            .icmp_imm_s(IntCC::Equal, helper_status, HELPER_STATUS_INTERRUPTED);
    builder
        .ins()
        .brif(was_interrupted, interrupted_block, &[], check_allocation_failed_block, &[]);

    builder.switch_to_block(interrupted_block);
    emit_helper_terminal(builder, activation, frame, bytecode_offset, STATUS_INTERRUPTED);

    builder.switch_to_block(check_allocation_failed_block);
    let allocation_failed =
        builder
            .ins()
            .icmp_imm_s(IntCC::Equal, helper_status, HELPER_STATUS_ALLOCATION_FAILED);
    builder
        .ins()
        .brif(allocation_failed, allocation_failed_block, &[], check_poisoned_block, &[]);

    builder.switch_to_block(allocation_failed_block);
    emit_helper_terminal(builder, activation, frame, bytecode_offset, STATUS_ALLOCATION_FAILED);

    builder.switch_to_block(check_poisoned_block);
    let helper_poisoned =
        builder
            .ins()
            .icmp_imm_s(IntCC::Equal, helper_status, HELPER_STATUS_POISONED);
    builder
        .ins()
        .brif(helper_poisoned, poisoned_block, &[], invalid_helper_block, &[]);

    builder.switch_to_block(poisoned_block);
    emit_helper_terminal(builder, activation, frame, bytecode_offset, STATUS_POISONED);

    builder.switch_to_block(invalid_helper_block);
    let poisoned = builder.ins().iconst(types::I32, 1);
    builder
        .ins()
        .store(memory_flags, poisoned, activation, ACTIVATION_POISONED_OFFSET);
    emit_helper_terminal(builder, activation, frame, bytecode_offset, STATUS_POISONED);

    Ok(())
}

fn clear_published_safepoint(builder: &mut FunctionBuilder<'_>, frame: ClifValue) {
    let memory_flags = plain_mem_flags();
    let no_safepoint = builder.ins().iconst(types::I32, NO_SAFEPOINT as i32 as i64);
    builder
        .ins()
        .store(memory_flags, no_safepoint, frame, SHADOW_FRAME_SAFEPOINT_INDEX_OFFSET);
    let no_bytecode = builder
        .ins()
        .iconst(types::I32, NO_BYTECODE_OFFSET as i32 as i64);
    builder
        .ins()
        .store(memory_flags, no_bytecode, frame, SHADOW_FRAME_BYTECODE_OFFSET);
}

fn emit_helper_terminal(
    builder: &mut FunctionBuilder<'_>,
    activation: ClifValue,
    frame: ClifValue,
    bytecode_offset: u32,
    status: u32,
) {
    clear_published_safepoint(builder, frame);
    let offset = builder.ins().iconst(types::I32, bytecode_offset as i64);
    let memory_flags = plain_mem_flags();
    builder
        .ins()
        .store(memory_flags, offset, activation, ACTIVATION_SIDE_EXIT_OFFSET);
    emit_status_return(builder, status);
}

fn guard_smi_one(
    builder: &mut FunctionBuilder<'_>,
    activation: ClifValue,
    slots: ClifValue,
    source: usize,
    instruction: &VerifiedInstruction,
) -> Result<(ClifValue, Block), BaselineCompileError> {
    let raw = load_slot(builder, slots, source)?;
    let tag = builder.ins().ushr_imm_u(raw, VALUE_TAG_SHIFT);
    let is_smi = builder.ins().icmp_imm_u(IntCC::Equal, tag, SMI_TAG as i64);
    let smi_block = builder.create_block();
    let slow_exit_block = builder.create_block();
    builder
        .ins()
        .brif(is_smi, smi_block, &[], slow_exit_block, &[]);

    builder.switch_to_block(slow_exit_block);
    emit_side_exit(builder, activation, instruction.offset)?;

    builder.switch_to_block(smi_block);
    Ok((builder.ins().ireduce(types::I32, raw), slow_exit_block))
}

fn guard_smi_pair(
    builder: &mut FunctionBuilder<'_>,
    activation: ClifValue,
    slots: ClifValue,
    left: usize,
    right: usize,
    instruction: &VerifiedInstruction,
) -> Result<(ClifValue, ClifValue, Block), BaselineCompileError> {
    let left_raw = load_slot(builder, slots, left)?;
    let right_raw = load_slot(builder, slots, right)?;
    let left_tag = builder.ins().ushr_imm_u(left_raw, VALUE_TAG_SHIFT);
    let right_tag = builder.ins().ushr_imm_u(right_raw, VALUE_TAG_SHIFT);
    let left_is_smi = builder
        .ins()
        .icmp_imm_u(IntCC::Equal, left_tag, SMI_TAG as i64);
    let right_is_smi = builder
        .ins()
        .icmp_imm_u(IntCC::Equal, right_tag, SMI_TAG as i64);
    let both_smi = builder.ins().band(left_is_smi, right_is_smi);
    let smi_block = builder.create_block();
    let slow_exit_block = builder.create_block();
    builder
        .ins()
        .brif(both_smi, smi_block, &[], slow_exit_block, &[]);

    builder.switch_to_block(slow_exit_block);
    emit_side_exit(builder, activation, instruction.offset)?;

    builder.switch_to_block(smi_block);
    Ok((
        builder.ins().ireduce(types::I32, left_raw),
        builder.ins().ireduce(types::I32, right_raw),
        slow_exit_block,
    ))
}

fn box_smi(builder: &mut FunctionBuilder<'_>, value: ClifValue) -> ClifValue {
    let payload = builder.ins().uextend(types::I64, value);
    let tag_bits = builder
        .ins()
        .iconst(types::I64, ((SMI_TAG as u64) << VALUE_TAG_SHIFT) as i64);
    builder.ins().bor(tag_bits, payload)
}

fn store_smi_and_continue(
    builder: &mut FunctionBuilder<'_>,
    slots: ClifValue,
    dest: usize,
    result: ClifValue,
    blocks: &[Block],
    index: usize,
) -> Result<(), BaselineCompileError> {
    let boxed = box_smi(builder, result);
    store_slot(builder, slots, dest, boxed)?;
    jump_to_next(builder, blocks, index);
    Ok(())
}

fn emit_checked_i64_smi_result(
    builder: &mut FunctionBuilder<'_>,
    slots: ClifValue,
    dest: usize,
    result_i64: ClifValue,
    additional_validity: Option<ClifValue>,
    slow_exit_block: Block,
    blocks: &[Block],
    index: usize,
) -> Result<(), BaselineCompileError> {
    let at_least_min =
        builder
            .ins()
            .icmp_imm_s(IntCC::SignedGreaterThanOrEqual, result_i64, i32::MIN as i64);
    let at_most_max =
        builder
            .ins()
            .icmp_imm_s(IntCC::SignedLessThanOrEqual, result_i64, i32::MAX as i64);
    let mut fits_smi = builder.ins().band(at_least_min, at_most_max);
    if let Some(additional_validity) = additional_validity {
        fits_smi = builder.ins().band(fits_smi, additional_validity);
    }
    let range_block = builder.create_block();
    builder
        .ins()
        .brif(fits_smi, range_block, &[], slow_exit_block, &[]);

    builder.switch_to_block(range_block);
    let result_i32 = builder.ins().ireduce(types::I32, result_i64);
    store_smi_and_continue(builder, slots, dest, result_i32, blocks, index)
}

fn emit_smi_binary(
    builder: &mut FunctionBuilder<'_>,
    activation: ClifValue,
    slots: ClifValue,
    blocks: &[Block],
    index: usize,
    instruction: &VerifiedInstruction,
) -> Result<(), BaselineCompileError> {
    let (Some(dest), Some(left), Some(right)) = (
        local_index(instruction.operands[0], instruction.width),
        local_index(instruction.operands[1], instruction.width),
        local_index(instruction.operands[2], instruction.width),
    ) else {
        emit_side_exit(builder, activation, instruction.offset)?;
        return Ok(());
    };
    let (left_i32, right_i32, slow_exit_block) =
        guard_smi_pair(builder, activation, slots, left, right, instruction)?;

    match instruction.opcode {
        OpCode::Add | OpCode::Sub | OpCode::Mul => {
            let left_i64 = builder.ins().sextend(types::I64, left_i32);
            let right_i64 = builder.ins().sextend(types::I64, right_i32);
            let result_i64 = match instruction.opcode {
                OpCode::Add => builder.ins().iadd(left_i64, right_i64),
                OpCode::Sub => builder.ins().isub(left_i64, right_i64),
                OpCode::Mul => builder.ins().imul(left_i64, right_i64),
                _ => unreachable!(),
            };
            let additional_validity = if instruction.opcode == OpCode::Mul {
                let result_is_zero = builder.ins().icmp_imm_s(IntCC::Equal, result_i64, 0);
                let left_negative = builder.ins().icmp_imm_s(IntCC::SignedLessThan, left_i32, 0);
                let right_negative = builder
                    .ins()
                    .icmp_imm_s(IntCC::SignedLessThan, right_i32, 0);
                let has_negative_operand = builder.ins().bor(left_negative, right_negative);
                let negative_zero = builder.ins().band(result_is_zero, has_negative_operand);
                Some(builder.ins().icmp_imm_s(IntCC::Equal, negative_zero, 0))
            } else {
                None
            };
            emit_checked_i64_smi_result(
                builder,
                slots,
                dest,
                result_i64,
                additional_validity,
                slow_exit_block,
                blocks,
                index,
            )
        }
        OpCode::BitAnd | OpCode::BitOr | OpCode::BitXor => {
            let result = match instruction.opcode {
                OpCode::BitAnd => builder.ins().band(left_i32, right_i32),
                OpCode::BitOr => builder.ins().bor(left_i32, right_i32),
                OpCode::BitXor => builder.ins().bxor(left_i32, right_i32),
                _ => unreachable!(),
            };
            store_smi_and_continue(builder, slots, dest, result, blocks, index)
        }
        OpCode::ShiftLeft | OpCode::ShiftRightArithmetic | OpCode::ShiftRightLogical => {
            let mask = builder.ins().iconst(types::I32, 31);
            let shift = builder.ins().band(right_i32, mask);
            let result = match instruction.opcode {
                OpCode::ShiftLeft => builder.ins().ishl(left_i32, shift),
                OpCode::ShiftRightArithmetic => builder.ins().sshr(left_i32, shift),
                OpCode::ShiftRightLogical => builder.ins().ushr(left_i32, shift),
                _ => unreachable!(),
            };
            if instruction.opcode == OpCode::ShiftRightLogical {
                let fits_smi = builder
                    .ins()
                    .icmp_imm_s(IntCC::SignedGreaterThanOrEqual, result, 0);
                let result_block = builder.create_block();
                builder
                    .ins()
                    .brif(fits_smi, result_block, &[], slow_exit_block, &[]);
                builder.switch_to_block(result_block);
            }
            store_smi_and_continue(builder, slots, dest, result, blocks, index)
        }
        _ => unreachable!(),
    }
}

fn emit_smi_immediate(
    builder: &mut FunctionBuilder<'_>,
    activation: ClifValue,
    slots: ClifValue,
    blocks: &[Block],
    index: usize,
    instruction: &VerifiedInstruction,
) -> Result<(), BaselineCompileError> {
    let (Some(dest), Some(left)) = (
        local_index(instruction.operands[0], instruction.width),
        local_index(instruction.operands[1], instruction.width),
    ) else {
        emit_side_exit(builder, activation, instruction.offset)?;
        return Ok(());
    };
    let immediate = instruction.operands[2].as_signed(instruction.width) as i64;
    let (left_i32, slow_exit_block) = guard_smi_one(builder, activation, slots, left, instruction)?;

    match instruction.opcode {
        OpCode::AddImm | OpCode::SubImm | OpCode::MulImm => {
            let left_i64 = builder.ins().sextend(types::I64, left_i32);
            let result_i64 = match instruction.opcode {
                OpCode::AddImm => builder.ins().iadd_imm_s(left_i64, immediate),
                OpCode::SubImm => builder.ins().iadd_imm_s(left_i64, -immediate),
                OpCode::MulImm => builder.ins().imul_imm_s(left_i64, immediate),
                _ => unreachable!(),
            };
            let additional_validity = if instruction.opcode == OpCode::MulImm {
                let result_is_zero = builder.ins().icmp_imm_s(IntCC::Equal, result_i64, 0);
                let left_negative = builder.ins().icmp_imm_s(IntCC::SignedLessThan, left_i32, 0);
                let immediate_negative = builder
                    .ins()
                    .iconst(types::I8, if immediate < 0 { 1 } else { 0 });
                let has_negative_operand = builder.ins().bor(left_negative, immediate_negative);
                let negative_zero = builder.ins().band(result_is_zero, has_negative_operand);
                Some(builder.ins().icmp_imm_s(IntCC::Equal, negative_zero, 0))
            } else {
                None
            };
            emit_checked_i64_smi_result(
                builder,
                slots,
                dest,
                result_i64,
                additional_validity,
                slow_exit_block,
                blocks,
                index,
            )
        }
        OpCode::BitAndImm | OpCode::BitOrImm | OpCode::BitXorImm => {
            let immediate = builder.ins().iconst(types::I32, immediate);
            let result = match instruction.opcode {
                OpCode::BitAndImm => builder.ins().band(left_i32, immediate),
                OpCode::BitOrImm => builder.ins().bor(left_i32, immediate),
                OpCode::BitXorImm => builder.ins().bxor(left_i32, immediate),
                _ => unreachable!(),
            };
            store_smi_and_continue(builder, slots, dest, result, blocks, index)
        }
        OpCode::ShiftLeftImm | OpCode::ShiftRightArithmeticImm | OpCode::ShiftRightLogicalImm => {
            let shift = builder.ins().iconst(types::I32, immediate & 31);
            let result = match instruction.opcode {
                OpCode::ShiftLeftImm => builder.ins().ishl(left_i32, shift),
                OpCode::ShiftRightArithmeticImm => builder.ins().sshr(left_i32, shift),
                OpCode::ShiftRightLogicalImm => builder.ins().ushr(left_i32, shift),
                _ => unreachable!(),
            };
            if instruction.opcode == OpCode::ShiftRightLogicalImm {
                let fits_smi = builder
                    .ins()
                    .icmp_imm_s(IntCC::SignedGreaterThanOrEqual, result, 0);
                let result_block = builder.create_block();
                builder
                    .ins()
                    .brif(fits_smi, result_block, &[], slow_exit_block, &[]);
                builder.switch_to_block(result_block);
            }
            store_smi_and_continue(builder, slots, dest, result, blocks, index)
        }
        _ => unreachable!(),
    }
}

fn emit_smi_unary(
    builder: &mut FunctionBuilder<'_>,
    activation: ClifValue,
    slots: ClifValue,
    blocks: &[Block],
    index: usize,
    instruction: &VerifiedInstruction,
) -> Result<(), BaselineCompileError> {
    let (dest_operand, source_operand) = if matches!(instruction.opcode, OpCode::Inc | OpCode::Dec)
    {
        (0, 0)
    } else {
        (0, 1)
    };
    let (Some(dest), Some(source)) = (
        local_index(instruction.operands[dest_operand], instruction.width),
        local_index(instruction.operands[source_operand], instruction.width),
    ) else {
        emit_side_exit(builder, activation, instruction.offset)?;
        return Ok(());
    };
    let (value_i32, slow_exit_block) =
        guard_smi_one(builder, activation, slots, source, instruction)?;

    match instruction.opcode {
        OpCode::BitNot => {
            let result = builder.ins().bnot(value_i32);
            store_smi_and_continue(builder, slots, dest, result, blocks, index)
        }
        OpCode::Neg => {
            let nonzero = builder.ins().icmp_imm_s(IntCC::NotEqual, value_i32, 0);
            let not_min = builder
                .ins()
                .icmp_imm_s(IntCC::NotEqual, value_i32, i32::MIN as i64);
            let remains_smi = builder.ins().band(nonzero, not_min);
            let result_block = builder.create_block();
            builder
                .ins()
                .brif(remains_smi, result_block, &[], slow_exit_block, &[]);
            builder.switch_to_block(result_block);
            let result = builder.ins().ineg(value_i32);
            store_smi_and_continue(builder, slots, dest, result, blocks, index)
        }
        OpCode::Inc | OpCode::Dec => {
            let value_i64 = builder.ins().sextend(types::I64, value_i32);
            let delta = if instruction.opcode == OpCode::Inc {
                1
            } else {
                -1
            };
            let result_i64 = builder.ins().iadd_imm_s(value_i64, delta);
            emit_checked_i64_smi_result(
                builder,
                slots,
                dest,
                result_i64,
                None,
                slow_exit_block,
                blocks,
                index,
            )
        }
        _ => unreachable!(),
    }
}

fn emit_smi_comparison(
    builder: &mut FunctionBuilder<'_>,
    activation: ClifValue,
    slots: ClifValue,
    blocks: &[Block],
    index: usize,
    instruction: &VerifiedInstruction,
) -> Result<(), BaselineCompileError> {
    let (Some(dest), Some(left), Some(right)) = (
        local_index(instruction.operands[0], instruction.width),
        local_index(instruction.operands[1], instruction.width),
        local_index(instruction.operands[2], instruction.width),
    ) else {
        emit_side_exit(builder, activation, instruction.offset)?;
        return Ok(());
    };
    let (left_i32, right_i32, _slow_exit_block) =
        guard_smi_pair(builder, activation, slots, left, right, instruction)?;
    let condition = builder.ins().icmp(
        match instruction.opcode {
            OpCode::StrictEqual => IntCC::Equal,
            OpCode::StrictNotEqual => IntCC::NotEqual,
            OpCode::LessThan => IntCC::SignedLessThan,
            OpCode::LessThanOrEqual => IntCC::SignedLessThanOrEqual,
            OpCode::GreaterThan => IntCC::SignedGreaterThan,
            OpCode::GreaterThanOrEqual => IntCC::SignedGreaterThanOrEqual,
            _ => unreachable!(),
        },
        left_i32,
        right_i32,
    );
    let true_bits = builder
        .ins()
        .iconst(types::I64, Value::bool(true).as_raw_bits() as i64);
    let false_bits = builder
        .ins()
        .iconst(types::I64, Value::bool(false).as_raw_bits() as i64);
    let result = builder.ins().select(condition, true_bits, false_bits);
    store_slot(builder, slots, dest, result)?;
    jump_to_next(builder, blocks, index);
    Ok(())
}

fn emit_logical_not(
    builder: &mut FunctionBuilder<'_>,
    activation: ClifValue,
    slots: ClifValue,
    blocks: &[Block],
    index: usize,
    instruction: &VerifiedInstruction,
) -> Result<(), BaselineCompileError> {
    let (Some(dest), Some(source)) = (
        local_index(instruction.operands[0], instruction.width),
        local_index(instruction.operands[1], instruction.width),
    ) else {
        emit_side_exit(builder, activation, instruction.offset)?;
        return Ok(());
    };
    let raw = load_slot(builder, slots, source)?;
    let (supported, truthy) = fast_to_boolean(builder, raw);
    let valid_block = builder.create_block();
    let slow_exit_block = builder.create_block();
    builder
        .ins()
        .brif(supported, valid_block, &[], slow_exit_block, &[]);
    builder.switch_to_block(slow_exit_block);
    emit_side_exit(builder, activation, instruction.offset)?;

    builder.switch_to_block(valid_block);
    let is_falsy = builder.ins().icmp_imm_s(IntCC::Equal, truthy, 0);
    let true_bits = builder
        .ins()
        .iconst(types::I64, Value::bool(true).as_raw_bits() as i64);
    let false_bits = builder
        .ins()
        .iconst(types::I64, Value::bool(false).as_raw_bits() as i64);
    let result = builder.ins().select(is_falsy, true_bits, false_bits);
    store_slot(builder, slots, dest, result)?;
    jump_to_next(builder, blocks, index);
    Ok(())
}

fn compare_loaded_imm(
    builder: &mut FunctionBuilder<'_>,
    ty: cranelift_codegen::ir::Type,
    base: ClifValue,
    offset: i32,
    expected: i64,
) -> ClifValue {
    let memory_flags = plain_mem_flags();
    let value = builder.ins().load(ty, memory_flags, base, offset);
    builder.ins().icmp_imm_s(IntCC::Equal, value, expected)
}

fn and_condition(
    builder: &mut FunctionBuilder<'_>,
    left: ClifValue,
    right: ClifValue,
) -> ClifValue {
    builder.ins().band(left, right)
}

fn and_loaded_imm(
    builder: &mut FunctionBuilder<'_>,
    condition: ClifValue,
    ty: cranelift_codegen::ir::Type,
    base: ClifValue,
    offset: i32,
    expected: i64,
) -> ClifValue {
    let next = compare_loaded_imm(builder, ty, base, offset, expected);
    and_condition(builder, condition, next)
}

fn local_index(operand: DecodedOperand, width: WidthEnum) -> Option<usize> {
    let raw = operand.as_signed(width);
    if raw >= 0 {
        return None;
    }
    (-1_isize)
        .checked_sub(raw)
        .and_then(|index| usize::try_from(index).ok())
}

fn plain_mem_flags() -> MemFlagsData {
    // `LoadedPrototype::call` accepts only a lifetime-branded activation whose activation, frame,
    // and slot storage remain live for the entire synchronous call. Marking accesses
    // non-trapping records that embedding proof without granting alignment or code-motion flags.
    MemFlagsData::new().with_notrap()
}

fn slot_offset(slot: usize) -> Result<i32, BaselineCompileError> {
    let byte_offset = slot
        .checked_mul(SLOT_BYTES)
        .ok_or(BaselineCompileError::SlotOffsetTooLarge(slot))?;
    i32::try_from(byte_offset).map_err(|_| BaselineCompileError::SlotOffsetTooLarge(slot))
}

fn load_slot(
    builder: &mut FunctionBuilder<'_>,
    slots: ClifValue,
    slot: usize,
) -> Result<ClifValue, BaselineCompileError> {
    let memory_flags = plain_mem_flags();
    Ok(builder
        .ins()
        .load(types::I64, memory_flags, slots, slot_offset(slot)?))
}

fn store_slot(
    builder: &mut FunctionBuilder<'_>,
    slots: ClifValue,
    slot: usize,
    value: ClifValue,
) -> Result<(), BaselineCompileError> {
    let memory_flags = plain_mem_flags();
    builder
        .ins()
        .store(memory_flags, value, slots, slot_offset(slot)?);
    Ok(())
}

fn store_raw_constant(
    builder: &mut FunctionBuilder<'_>,
    slots: ClifValue,
    slot: usize,
    raw: u64,
) -> Result<(), BaselineCompileError> {
    let value = builder.ins().iconst(types::I64, raw as i64);
    store_slot(builder, slots, slot, value)
}

fn emit_side_exit(
    builder: &mut FunctionBuilder<'_>,
    activation: ClifValue,
    bytecode_offset: usize,
) -> Result<(), BaselineCompileError> {
    let offset = u32::try_from(bytecode_offset)
        .map_err(|_| BaselineCompileError::BytecodeOffsetTooLarge(bytecode_offset))?;
    let offset_value = builder.ins().iconst(types::I32, offset as i64);
    let memory_flags = plain_mem_flags();
    builder
        .ins()
        .store(memory_flags, offset_value, activation, ACTIVATION_SIDE_EXIT_OFFSET);
    emit_status_return(builder, STATUS_SIDE_EXIT);
    Ok(())
}

fn emit_status_return(builder: &mut FunctionBuilder<'_>, status: u32) {
    let status = builder.ins().iconst(types::I32, status as i64);
    builder.ins().return_(&[status]);
}

fn jump_to_next(builder: &mut FunctionBuilder<'_>, blocks: &[Block], index: usize) {
    builder.ins().jump(blocks[index + 1], &[]);
}

fn block_for_offset(bytecode: &VerifiedBytecode<'_>, blocks: &[Block], offset: usize) -> Block {
    let index = bytecode
        .instructions()
        .binary_search_by_key(&offset, |instruction| instruction.offset)
        .expect("verifier guaranteed branch target boundary");
    blocks[index]
}

#[cfg(test)]
mod tests {
    use std::{num::NonZeroU32, ptr};

    use super::*;
    use crate::runtime::{
        ContextBuilder, Value,
        bytecode::{
            instruction::{
                extra_wide_prefix_index_to_opcode_index, wide_prefix_index_to_opcode_index,
            },
            verifier::{ConstantKind, VerificationLimits, VerifiedBytecode},
        },
        jit::{
            abi::{
                ActivationOutcome, ActivationOwner, GeneratedEntry, JitActivation, JitSlot,
                ShadowFrameOwner, TestBackedgePollBehavior, TestBackedgePollObservation,
                with_test_backedge_poll_behavior,
            },
            code_cache::ExecutableCodeCache,
            hotness::DeterministicInterruptBudget,
        },
    };

    fn local(index: usize) -> u8 {
        (-1_i8 - index as i8) as u8
    }

    fn encode(opcode: OpCode, operands: &[u8]) -> Vec<u8> {
        let mut bytes = vec![opcode as u8];
        bytes.extend_from_slice(operands);
        bytes
    }

    fn append_width_encoded(
        bytes: &mut Vec<u8>,
        opcode: OpCode,
        operands: &[i32],
        width: WidthEnum,
    ) {
        match width {
            WidthEnum::Narrow => {
                bytes.push(opcode as u8);
                bytes.extend(operands.iter().map(|&operand| operand as i8 as u8));
            }
            WidthEnum::Wide => {
                let prefix_index = bytes.len();
                bytes.push(OpCode::WidePrefix as u8);
                let opcode_index = wide_prefix_index_to_opcode_index(prefix_index);
                bytes.resize(opcode_index, 0);
                bytes.push(opcode as u8);
                for &operand in operands {
                    bytes.extend_from_slice(&(operand as i16).to_ne_bytes());
                }
            }
            WidthEnum::ExtraWide => {
                let prefix_index = bytes.len();
                bytes.push(OpCode::ExtraWidePrefix as u8);
                let opcode_index = extra_wide_prefix_index_to_opcode_index(prefix_index);
                bytes.resize(opcode_index, 0);
                bytes.push(opcode as u8);
                for &operand in operands {
                    bytes.extend_from_slice(&operand.to_ne_bytes());
                }
            }
        }
    }

    fn verify(bytes: &[u8], num_locals: usize) -> VerifiedBytecode<'_> {
        VerifiedBytecode::verify(bytes, VerificationLimits::empty(num_locals, 0)).unwrap()
    }

    fn execute(
        verified: &VerifiedBytecode<'_>,
        slots: Vec<Value>,
    ) -> (ActivationOutcome, Vec<Value>, usize) {
        let (outcome, slots, code_len, _) = execute_with_poll(
            verified,
            slots,
            NonZeroU32::new(1_000).unwrap(),
            TestBackedgePollBehavior::Normal,
        );
        (outcome, slots, code_len)
    }

    fn execute_with_poll(
        verified: &VerifiedBytecode<'_>,
        slots: Vec<Value>,
        quantum: NonZeroU32,
        behavior: TestBackedgePollBehavior,
    ) -> (ActivationOutcome, Vec<Value>, usize, TestBackedgePollObservation) {
        let prepared = compile_prototype(verified).unwrap();
        assert_eq!(prepared.required_frame_slots(), slots.len());
        assert!(prepared.safepoints().records().is_empty());
        let mapped_len = ExecutableCodeCache::mapped_len_for(&prepared).unwrap();
        let mut cache = ExecutableCodeCache::new(mapped_len, 1).unwrap();
        cache.insert(1, prepared).unwrap();

        let mut owned = ContextBuilder::new().build().unwrap();
        let mut outcome = None;
        let mut returned_slots = None;
        let mut poll_observation = None;
        owned.with_jit_context(|context| {
            let mut slots: Vec<JitSlot> = slots
                .into_iter()
                .map(|value| JitSlot::try_from_value(context, value).unwrap())
                .collect();
            let loaded = cache.get(1).unwrap().unwrap();
            let code_len = loaded.code_len();
            let (validated, observation) = with_test_backedge_poll_behavior(behavior, || {
                let mut frame = ShadowFrameOwner::new(&mut slots, loaded.safepoints()).unwrap();
                let (mut budget, _) = DeterministicInterruptBudget::new(quantum);
                let mut activation =
                    ActivationOwner::new(context, &mut frame, &mut budget).unwrap();
                // SAFETY: The loaded artifact owns the exact entry bytes and maps used by this
                // activation for the complete synchronous call.
                let status = unsafe { loaded.call(&mut activation) }.unwrap();
                activation.validate_result(status).unwrap()
            });
            outcome = Some(validated);
            poll_observation = Some(observation);
            returned_slots = Some((slots.iter().map(JitSlot::value).collect::<Vec<_>>(), code_len));
        });

        let (slots, code_len) = returned_slots.unwrap();
        (outcome.unwrap(), slots, code_len, poll_observation.unwrap())
    }

    #[test]
    fn generated_arithmetic_matches_boxed_reference_subset() {
        for opcode in [OpCode::AddImm, OpCode::SubImm] {
            for left in [-100_i8, -7, 0, 9, 100] {
                for immediate in [-20_i8, -1, 0, 1, 20] {
                    let mut bytes = encode(OpCode::LoadImmediate, &[local(0), left as u8]);
                    bytes.extend(encode(opcode, &[local(1), local(0), immediate as u8]));
                    bytes.extend(encode(OpCode::Ret, &[local(1)]));
                    let verified = verify(&bytes, 2);
                    let seed = Value::undefined();
                    let (outcome, slots, code_len) = execute(&verified, vec![seed; 2]);

                    let expected = if opcode == OpCode::AddImm {
                        i32::from(left) + i32::from(immediate)
                    } else {
                        i32::from(left) - i32::from(immediate)
                    };
                    let expected_bits = Value::raw_smi(expected).as_raw_bits();
                    assert_eq!(outcome, ActivationOutcome::Returned(expected_bits));
                    assert_eq!(slots[1].as_raw_bits(), expected_bits);
                    assert!(code_len > 0);
                }
            }
        }
    }

    #[test]
    fn generated_forward_control_flow_takes_exact_boolean_edges() {
        fn program(load_condition: OpCode) -> Vec<u8> {
            let mut bytes = encode(load_condition, &[local(0)]); // offset 0
            bytes.extend(encode(OpCode::JumpTrue, &[local(0), 8])); // offset 2 -> 10
            bytes.extend(encode(OpCode::LoadImmediate, &[local(1), 1])); // offset 5
            bytes.extend(encode(OpCode::Jump, &[5])); // offset 8 -> 13
            bytes.extend(encode(OpCode::LoadImmediate, &[local(1), 7])); // offset 10
            bytes.extend(encode(OpCode::Ret, &[local(1)])); // offset 13
            bytes
        }

        let seed = Value::undefined();
        let true_bytes = program(OpCode::LoadTrue);
        let true_code = verify(&true_bytes, 2);
        assert_eq!(
            execute(&true_code, vec![seed; 2]).0,
            ActivationOutcome::Returned(Value::raw_smi(7).as_raw_bits())
        );

        let false_bytes = program(OpCode::LoadFalse);
        let false_code = verify(&false_bytes, 2);
        assert_eq!(
            execute(&false_code, vec![seed; 2]).0,
            ActivationOutcome::Returned(Value::raw_smi(1).as_raw_bits())
        );
    }

    fn finite_native_loop(limit: i8) -> (Vec<u8>, usize, usize) {
        let mut bytes = encode(OpCode::LoadImmediate, &[local(0), 0]);
        bytes.extend(encode(OpCode::LoadImmediate, &[local(1), limit as u8]));
        let loop_offset = bytes.len();
        bytes.extend(encode(OpCode::AddImm, &[local(0), local(0), 1]));
        bytes.extend(encode(OpCode::LessThan, &[local(2), local(0), local(1)]));
        let branch_offset = bytes.len();
        bytes.extend(encode(
            OpCode::JumpTrue,
            &[
                local(2),
                (loop_offset as isize - branch_offset as isize) as i8 as u8,
            ],
        ));
        bytes.extend(encode(OpCode::Ret, &[local(0)]));
        (bytes, loop_offset, branch_offset)
    }

    #[test]
    fn generated_native_cfg_executes_finite_loop_and_counts_every_taken_edge() {
        let (bytes, _loop_offset, _branch_offset) = finite_native_loop(8);
        let verified = verify(&bytes, 3);
        let (outcome, slots, code_len, polls) = execute_with_poll(
            &verified,
            vec![Value::undefined(); 3],
            NonZeroU32::new(100).unwrap(),
            TestBackedgePollBehavior::Normal,
        );
        assert_eq!(outcome, ActivationOutcome::Returned(Value::raw_smi(8).as_raw_bits()));
        assert_eq!(slots[0].as_raw_bits(), Value::raw_smi(8).as_raw_bits());
        assert_eq!(polls.calls, 7, "one helper call per taken nonpositive edge");
        assert!(code_len > 0);
    }

    #[test]
    fn generated_backedge_terminals_preserve_exact_target_and_committed_slots() {
        let (bytes, loop_offset, _branch_offset) = finite_native_loop(8);
        let verified = verify(&bytes, 3);

        for (behavior, expected) in [
            (
                TestBackedgePollBehavior::PolicySideExit,
                ActivationOutcome::SideExit(loop_offset),
            ),
            (
                TestBackedgePollBehavior::PolicyFailure,
                ActivationOutcome::Poisoned(loop_offset),
            ),
            (TestBackedgePollBehavior::Panic, ActivationOutcome::Poisoned(loop_offset)),
        ] {
            let (outcome, slots, _, polls) = execute_with_poll(
                &verified,
                vec![Value::undefined(); 3],
                NonZeroU32::new(100).unwrap(),
                behavior,
            );
            assert_eq!(outcome, expected);
            assert_eq!(slots[0].as_raw_bits(), Value::raw_smi(1).as_raw_bits());
            assert_eq!(slots[2].as_raw_bits(), Value::bool(true).as_raw_bits());
            assert_eq!(polls.calls, 1);
        }

        let (outcome, slots, _, polls) = execute_with_poll(
            &verified,
            vec![Value::undefined(); 3],
            NonZeroU32::new(1).unwrap(),
            TestBackedgePollBehavior::Normal,
        );
        assert_eq!(outcome, ActivationOutcome::Interrupted(loop_offset));
        assert_eq!(slots[0].as_raw_bits(), Value::raw_smi(1).as_raw_bits());
        assert_eq!(polls.calls, 1);
    }

    #[test]
    fn generated_branches_guard_exact_boolean_and_fast_to_boolean_types() {
        let mut exact = encode(OpCode::JumpTrue, &[local(0), 5]); // 0 -> 5
        exact.extend(encode(OpCode::Ret, &[local(0)])); // 3
        exact.extend(encode(OpCode::Ret, &[local(0)])); // 5
        let exact = verify(&exact, 1);
        assert_eq!(
            execute(&exact, vec![Value::raw_smi(1)]).0,
            ActivationOutcome::SideExit(0),
            "an exact-boolean branch must not silently treat a number as false"
        );

        for (seed, expected) in [
            (Value::undefined(), Value::raw_smi(2)),
            (Value::null(), Value::raw_smi(2)),
            (Value::bool(false), Value::raw_smi(2)),
            (Value::raw_smi(0), Value::raw_smi(2)),
            (Value::bool(true), Value::raw_smi(7)),
            (Value::raw_smi(-3), Value::raw_smi(7)),
        ] {
            let mut coercing = encode(OpCode::JumpToBooleanTrue, &[local(0), 8]); // 0 -> 8
            coercing.extend(encode(OpCode::LoadImmediate, &[local(1), 2])); // 3
            coercing.extend(encode(OpCode::Jump, &[5])); // 6 -> 11
            coercing.extend(encode(OpCode::LoadImmediate, &[local(1), 7])); // 8
            coercing.extend(encode(OpCode::Ret, &[local(1)])); // 11
            let coercing = verify(&coercing, 2);
            assert_eq!(
                execute(&coercing, vec![seed, Value::undefined()]).0,
                ActivationOutcome::Returned(expected.as_raw_bits())
            );
        }
    }

    #[test]
    fn generated_execution_handles_canonical_wide_and_extra_wide_operands() {
        let seed = Value::undefined();

        let mut wide = Vec::new();
        append_width_encoded(&mut wide, OpCode::LoadImmediate, &[-129, 321], WidthEnum::Wide);
        append_width_encoded(&mut wide, OpCode::Ret, &[-129], WidthEnum::Wide);
        let wide_code = verify(&wide, 129);
        assert!(
            wide_code
                .instructions()
                .iter()
                .all(|instruction| instruction.width == WidthEnum::Wide)
        );
        let (wide_outcome, wide_slots, _) = execute(&wide_code, vec![seed; 129]);
        assert_eq!(wide_outcome, ActivationOutcome::Returned(Value::raw_smi(321).as_raw_bits()));
        assert_eq!(wide_slots[128].as_raw_bits(), Value::raw_smi(321).as_raw_bits());

        let mut extra_wide = Vec::new();
        append_width_encoded(
            &mut extra_wide,
            OpCode::LoadImmediate,
            &[-1, 70_000],
            WidthEnum::ExtraWide,
        );
        append_width_encoded(&mut extra_wide, OpCode::Ret, &[-1], WidthEnum::Narrow);
        let extra_wide_code = verify(&extra_wide, 1);
        assert_eq!(extra_wide_code.instructions()[0].width, WidthEnum::ExtraWide);
        assert_eq!(extra_wide_code.instructions()[1].width, WidthEnum::Narrow);
        assert_eq!(
            execute(&extra_wide_code, vec![seed]).0,
            ActivationOutcome::Returned(Value::raw_smi(70_000).as_raw_bits())
        );
    }

    #[test]
    fn generated_conditional_branch_uses_verified_constant_offset() {
        fn program(load_condition: OpCode) -> Vec<u8> {
            let mut bytes = encode(load_condition, &[local(0)]); // offset 0
            bytes.extend(encode(OpCode::JumpTrueConstant, &[local(0), 0])); // offset 2 -> 10
            bytes.extend(encode(OpCode::LoadImmediate, &[local(1), 1])); // offset 5
            bytes.extend(encode(OpCode::Jump, &[5])); // offset 8 -> 13
            bytes.extend(encode(OpCode::LoadImmediate, &[local(1), 7])); // offset 10
            bytes.extend(encode(OpCode::Ret, &[local(1)])); // offset 13
            bytes
        }

        let constants = [ConstantKind::JumpOffset(8)];
        let seed = Value::undefined();
        for (condition, expected) in [(OpCode::LoadTrue, 7), (OpCode::LoadFalse, 1)] {
            let bytes = program(condition);
            let mut limits = VerificationLimits::empty(2, 0);
            limits.constants = &constants;
            let verified = VerifiedBytecode::verify(&bytes, limits).unwrap();
            assert_eq!(verified.instructions()[1].branch_target, Some(10));
            assert_eq!(
                execute(&verified, vec![seed; 2]).0,
                ActivationOutcome::Returned(Value::raw_smi(expected).as_raw_bits())
            );
        }
    }

    #[test]
    fn generated_wide_and_extra_wide_branches_use_prefix_start_as_base() {
        let seed = Value::undefined();

        // The inline displacement is relative to the prefix at offset 0. A displacement of 129
        // requires Wide encoding and lands on the LoadImmediate at exactly that byte boundary.
        let mut wide = Vec::new();
        append_width_encoded(&mut wide, OpCode::JumpTrue, &[-1, 129], WidthEnum::Wide);
        wide.extend(encode(OpCode::LoadImmediate, &[local(1), 1])); // offset 6
        wide.extend(encode(OpCode::Ret, &[local(1)])); // offset 9
        for _ in 0..59 {
            wide.extend(encode(OpCode::LoadUndefined, &[local(1)])); // offsets 11..129
        }
        wide.extend(encode(OpCode::LoadImmediate, &[local(1), 7])); // offset 129
        wide.extend(encode(OpCode::Ret, &[local(1)])); // offset 132

        let wide_code = verify(&wide, 2);
        assert_eq!(wide_code.instructions()[0].width, WidthEnum::Wide);
        assert_eq!(wide_code.instructions()[0].branch_target, Some(129));
        assert_eq!(
            execute(&wide_code, vec![Value::bool(true), seed]).0,
            ActivationOutcome::Returned(Value::raw_smi(7).as_raw_bits())
        );
        assert_eq!(
            execute(&wide_code, vec![Value::bool(false), seed]).0,
            ActivationOutcome::Returned(Value::raw_smi(1).as_raw_bits())
        );

        // A large constant-table index forces ExtraWide encoding without manufacturing a huge
        // bytecode body. The stored displacement is still relative to the prefix at offset 0.
        let mut constants = vec![ConstantKind::AnyValue; 65_537];
        constants[65_536] = ConstantKind::JumpOffset(17);
        let mut extra_wide = Vec::new();
        append_width_encoded(
            &mut extra_wide,
            OpCode::JumpTrueConstant,
            &[-1, 65_536],
            WidthEnum::ExtraWide,
        );
        extra_wide.extend(encode(OpCode::LoadImmediate, &[local(1), 1])); // offset 12
        extra_wide.extend(encode(OpCode::Jump, &[5])); // offset 15 -> 20
        extra_wide.extend(encode(OpCode::LoadImmediate, &[local(1), 7])); // offset 17
        extra_wide.extend(encode(OpCode::Ret, &[local(1)])); // offset 20

        let mut limits = VerificationLimits::empty(2, 0);
        limits.constants = &constants;
        let extra_wide_code = VerifiedBytecode::verify(&extra_wide, limits).unwrap();
        assert_eq!(extra_wide_code.instructions()[0].width, WidthEnum::ExtraWide);
        assert_eq!(extra_wide_code.instructions()[0].branch_target, Some(17));
        assert_eq!(
            execute(&extra_wide_code, vec![Value::bool(true), seed]).0,
            ActivationOutcome::Returned(Value::raw_smi(7).as_raw_bits())
        );
        assert_eq!(
            execute(&extra_wide_code, vec![Value::bool(false), seed]).0,
            ActivationOutcome::Returned(Value::raw_smi(1).as_raw_bits())
        );
    }

    #[test]
    fn raw_entry_rejects_null_and_invalid_headers_before_frame_access() {
        let bytes = encode(OpCode::Ret, &[local(0)]);
        let verified = verify(&bytes, 1);
        let prepared = compile_prototype(&verified).unwrap();
        let mapped_len = ExecutableCodeCache::mapped_len_for(&prepared).unwrap();
        let mut cache = ExecutableCodeCache::new(mapped_len, 1).unwrap();
        cache.insert(1, prepared).unwrap();
        let loaded = cache.get(1).unwrap().unwrap();

        // SAFETY: These calls deliberately exercise the private raw ABI. The null pointer is
        // rejected before any load. The second pointer identifies a live `JitActivation` header;
        // its invalid version must branch to rejection before its deliberately dangling frame is
        // read. Arbitrary non-null addresses are not covered by this test or the raw ABI contract.
        unsafe {
            let entry: GeneratedEntry = std::mem::transmute(loaded.start_address_for_test());
            assert_eq!(entry(ptr::null_mut()), STATUS_INVALID_ACTIVATION);

            let mut invalid = JitActivation::invalid_header_with_dangling_frame_for_test();
            assert_eq!(entry(ptr::from_mut(&mut invalid)), STATUS_INVALID_ACTIVATION);
        }
    }

    #[test]
    fn non_smi_overflow_and_unsupported_paths_exit_before_effects() {
        let mut overflow = encode(OpCode::AddImm, &[local(0), local(0), 1]);
        overflow.extend(encode(OpCode::Ret, &[local(0)]));
        let overflow_code = verify(&overflow, 1);
        assert_eq!(
            execute(&overflow_code, vec![Value::raw_smi(i32::MAX)]).0,
            ActivationOutcome::SideExit(0)
        );
        assert_eq!(execute(&overflow_code, vec![Value::null()]).0, ActivationOutcome::SideExit(0));

        let mut unsupported = encode(OpCode::LoadImmediate, &[local(0), 4]);
        let unsupported_offset = unsupported.len();
        unsupported.extend(encode(OpCode::DivImm, &[local(0), local(0), 2]));
        unsupported.extend(encode(OpCode::Ret, &[local(0)]));
        let unsupported_code = verify(&unsupported, 1);
        assert_eq!(
            execute(&unsupported_code, vec![Value::raw_smi(1)]).0,
            ActivationOutcome::SideExit(unsupported_offset)
        );

        let backedge = encode(OpCode::Jump, &[0]);
        let backedge_code = verify(&backedge, 0);
        assert_eq!(
            execute(&backedge_code, Vec::new()).0,
            ActivationOutcome::Interrupted(0),
            "an unconditional native cycle polls until deterministic quantum expiry"
        );
    }

    #[test]
    fn native_return_never_exposes_empty_from_load_move_or_entry() {
        let mut loaded_empty = encode(OpCode::LoadEmpty, &[local(0)]);
        let loaded_ret_offset = loaded_empty.len();
        loaded_empty.extend(encode(OpCode::Ret, &[local(0)]));
        assert_eq!(
            execute(&verify(&loaded_empty, 1), vec![Value::undefined()]).0,
            ActivationOutcome::SideExit(loaded_ret_offset)
        );

        let direct_ret = encode(OpCode::Ret, &[local(0)]);
        assert_eq!(
            execute(&verify(&direct_ret, 1), vec![Value::empty()]).0,
            ActivationOutcome::SideExit(0)
        );
        assert_eq!(
            execute(&verify(&direct_ret, 1), vec![Value::raw_smi(7)]).0,
            ActivationOutcome::Returned(Value::raw_smi(7).as_raw_bits()),
            "an unproven but canonical non-Empty immediate remains a safe native return"
        );

        let mut moved_empty = encode(OpCode::Mov, &[local(1), local(0)]);
        let moved_ret_offset = moved_empty.len();
        moved_empty.extend(encode(OpCode::Ret, &[local(1)]));
        assert_eq!(
            execute(&verify(&moved_empty, 2), vec![Value::empty(), Value::undefined()],).0,
            ActivationOutcome::SideExit(moved_ret_offset)
        );

        let mut overwritten = encode(OpCode::LoadEmpty, &[local(0)]);
        overwritten.extend(encode(OpCode::LoadUndefined, &[local(0)]));
        overwritten.extend(encode(OpCode::Ret, &[local(0)]));
        assert_eq!(
            execute(&verify(&overwritten, 1), vec![Value::undefined()]).0,
            ActivationOutcome::Returned(Value::undefined().as_raw_bits()),
            "a definite JS-producing overwrite restores proven native provenance"
        );
    }

    #[test]
    fn allocating_stack_map_is_exact_across_forward_control_flow() {
        let mut bytes = encode(OpCode::NewObject, &[local(4), 0]); // offset 0
        bytes.extend(encode(OpCode::JumpTrue, &[local(3), 8])); // offset 3 -> 11
        bytes.extend(encode(OpCode::Mov, &[local(5), local(0)])); // offset 6
        bytes.extend(encode(OpCode::Jump, &[5])); // offset 9 -> 14
        bytes.extend(encode(OpCode::Mov, &[local(5), local(1)])); // offset 11
        bytes.extend(encode(OpCode::Ret, &[local(4)])); // offset 14
        let verified = verify(&bytes, 6);
        let compiled = compile_prototype(&verified).unwrap();

        assert_eq!(compiled.safepoints().records().len(), 1);
        assert_eq!(compiled.safepoints().records()[0].result_slot, 4);
        assert_eq!(
            compiled.safepoints().live_slots(),
            &[0, 1, 3],
            "both branch inputs and the condition are live; dead/result/destination slots are not"
        );
    }

    #[test]
    fn unreachable_allocating_helper_after_mandatory_side_exit_is_not_promised() {
        let mut bytes = encode(OpCode::DivImm, &[local(0), local(0), 1]);
        bytes.extend(encode(OpCode::NewObject, &[local(1), 0]));
        bytes.extend(encode(OpCode::Ret, &[local(1)]));
        let verified = verify(&bytes, 2);
        let compiled = compile_prototype(&verified).unwrap();
        assert!(
            compiled.safepoints().records().is_empty(),
            "the resumed VM, not unreachable native code, owns the later allocation"
        );
        assert_eq!(
            execute(&verified, vec![Value::raw_smi(8), Value::undefined()]).0,
            ActivationOutcome::SideExit(0)
        );
    }

    #[test]
    fn prototype_limits_and_verified_frame_shape_are_enforced() {
        let ret = encode(OpCode::Ret, &[local(0)]);
        let verified = VerifiedBytecode::verify(
            &ret,
            VerificationLimits::empty(MAX_PROTOTYPE_FRAME_SLOTS + 1, 0),
        )
        .unwrap();
        assert!(matches!(
            compile_prototype(&verified),
            Err(BaselineCompileError::TooManyFrameSlots { .. })
        ));

        let normal = verify(&ret, 1);
        assert_eq!(normal.num_locals(), 1);
        assert_eq!(normal.num_arguments(), 0);

        let mut work = MAX_LIVENESS_WORKLIST_DEQUEUES;
        assert!(matches!(
            charge_liveness_work(&mut work),
            Err(BaselineCompileError::LivenessAnalysisWorkLimitExceeded {
                maximum: MAX_LIVENESS_WORKLIST_DEQUEUES
            })
        ));

        let mut native_value_work = MAX_NATIVE_VALUE_WORKLIST_DEQUEUES;
        assert!(matches!(
            charge_native_value_work(&mut native_value_work),
            Err(BaselineCompileError::NativeValueAnalysisWorkLimitExceeded {
                maximum: MAX_NATIVE_VALUE_WORKLIST_DEQUEUES
            })
        ));
    }

    #[test]
    fn vm_binding_ids_are_unique_and_exhaustion_never_wraps() {
        let next = AtomicU64::new(1);
        assert_eq!(allocate_vm_binding_id_from(&next).unwrap().get(), 1);
        assert_eq!(allocate_vm_binding_id_from(&next).unwrap().get(), 2);

        let exhausted = AtomicU64::new(u64::MAX);
        assert!(matches!(
            allocate_vm_binding_id_from(&exhausted),
            Err(BaselineCompileError::VmBindingIdsExhausted)
        ));
        assert_eq!(exhausted.load(Ordering::Relaxed), u64::MAX);
    }
}
