//! Minimal Cranelift baseline prototype with one audited allocating helper.
//!
//! This is deliberately not a VM dispatch tier. It accepts only checked, trusted in-process
//! bytecode and exits to a checked contained continuation before unsupported operations and
//! backedges. The sole generated call is zero-capacity `NewObject`. Compiler-derived liveness
//! spills every live boxed value into the context-registered frame before that call; no moving
//! pointer is embedded in code or retained in a native temporary across the safepoint.

use std::mem::size_of;

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
    ACTIVATION_HELPERS_OFFSET, ACTIVATION_INTERRUPT_BUDGET_OFFSET, ACTIVATION_POISONED_OFFSET,
    ACTIVATION_RESERVED_OFFSET, ACTIVATION_RESERVED_TAIL_OFFSET, ACTIVATION_RETURN_VALUE_OFFSET,
    ACTIVATION_SIDE_EXIT_OFFSET, ACTIVATION_STRUCT_SIZE_OFFSET, GENERATED_CODE_ABI_VERSION,
    HELPER_TABLE_ABI_VERSION_OFFSET, HELPER_TABLE_NEW_OBJECT_ZERO_OFFSET,
    HELPER_TABLE_RESERVED_OFFSET, HELPER_TABLE_STRUCT_SIZE_OFFSET, JIT_ACTIVATION_SIZE,
    JIT_HELPER_TABLE_SIZE, MAX_LIVE_ROOT_ENTRIES, MAX_SAFEPOINT_RECORDS, NO_BYTECODE_OFFSET,
    NO_SAFEPOINT, SAFEPOINT_FLAG_ALLOCATING_HELPER, SHADOW_FRAME_BYTECODE_OFFSET,
    SHADOW_FRAME_LIVE_SLOT_COUNT_OFFSET, SHADOW_FRAME_LIVE_SLOTS_OFFSET,
    SHADOW_FRAME_RECORD_COUNT_OFFSET, SHADOW_FRAME_RECORDS_OFFSET,
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

const VALUE_TAG_SHIFT: i64 = 48;
const SLOT_BYTES: usize = size_of::<u64>();
const SAFEPOINT_SOURCE_LOC_BASE: u32 = 0x6000_0000;

const HELPER_STATUS_OK: i64 = 0;
const HELPER_STATUS_INTERRUPTED: i64 = 2;
const HELPER_STATUS_ALLOCATION_FAILED: i64 = 3;
const HELPER_STATUS_POISONED: i64 = 4;

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
    UnsupportedNonLocalRegister { bytecode_offset: usize },
    UnsupportedClassMethodLiveness { bytecode_offset: usize },
    TooManySafepoints { actual: usize, maximum: usize },
    TooManyLiveRoots { actual: usize, maximum: usize },
    SafepointSourceLocationOverflow(usize),
    SafepointCallCountMismatch { expected: usize, actual: usize },
    MissingSafepointSourceLocation { native_return_offset: u32 },
    InvalidSafepointSourceLocation(u32),
    DuplicateSafepointCall(usize),
    SafepointMetadata(SafepointMetadataError),
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
                effects: instruction.effects,
            });
        }

        Ok(Self {
            bytes: bytes.into_boxed_slice(),
            instructions: instructions.into_boxed_slice(),
            num_locals: bytecode.num_locals(),
            num_arguments: bytecode.num_arguments(),
        })
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub(crate) fn instructions(&self) -> &[VerifiedInstruction] {
        &self.instructions
    }

    pub(crate) const fn num_locals(&self) -> usize {
        self.num_locals
    }

    pub(crate) const fn num_arguments(&self) -> usize {
        self.num_arguments
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
}

struct CompilationPlan {
    safepoint_for_instruction: Vec<Option<u32>>,
    records: Vec<SafepointRecord>,
    live_slots: Vec<u32>,
}

impl CompilationPlan {
    fn analyze(bytecode: &VerifiedBytecode<'_>) -> Result<Self, BaselineCompileError> {
        let instruction_count = bytecode.instructions().len();
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

        while let Some(index) = worklist.pop() {
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
            if instruction.opcode != OpCode::NewObject || instruction.operands[1].as_unsigned() != 0
            {
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

        Ok(Self { safepoint_for_instruction, records, live_slots })
    }
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
    let mut call_count = 0_usize;
    for call_site in compiled.buffer.call_sites() {
        call_count =
            call_count
                .checked_add(1)
                .ok_or(BaselineCompileError::SafepointCallCountMismatch {
                    expected: plan.records.len(),
                    actual: usize::MAX,
                })?;
        let native_return_offset = call_site.ret_addr;
        let Some(call_byte_offset) = native_return_offset.checked_sub(1) else {
            return Err(BaselineCompileError::MissingSafepointSourceLocation {
                native_return_offset,
            });
        };
        let Some(source_range) =
            compiled.buffer.get_srclocs_sorted().iter().find(|mapping| {
                mapping.start <= call_byte_offset && call_byte_offset < mapping.end
            })
        else {
            return Err(BaselineCompileError::MissingSafepointSourceLocation {
                native_return_offset,
            });
        };
        let source_bits = source_range.loc.bits();
        let Some(raw_index) = source_bits.checked_sub(SAFEPOINT_SOURCE_LOC_BASE) else {
            return Err(BaselineCompileError::InvalidSafepointSourceLocation(source_bits));
        };
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
    }
    if call_count != plan.records.len() || seen_safepoints.iter().any(|seen| !seen) {
        return Err(BaselineCompileError::SafepointCallCountMismatch {
            expected: plan.records.len(),
            actual: call_count,
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
        ACTIVATION_RESERVED_OFFSET,
        0,
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
    let slots = builder.use_var(slots_var);

    // Backedges are deterministic interrupt/safepoint boundaries in the interpreter. This gate
    // never emits one without a poll; it exits before evaluating the branch instead.
    if instruction.effects.contains(EffectFlags::BACKEDGE) {
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
        OpCode::AddImm | OpCode::SubImm => {
            emit_smi_arithmetic(builder, activation, slots, blocks, index, instruction)?
        }
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
            builder
                .ins()
                .jump(block_for_offset(bytecode, blocks, target), &[]);
        }
        OpCode::JumpTrue
        | OpCode::JumpTrueConstant
        | OpCode::JumpFalse
        | OpCode::JumpFalseConstant => {
            let Some(condition) = local_index(instruction.operands[0], instruction.width) else {
                emit_side_exit(builder, activation, instruction.offset)?;
                return Ok(());
            };
            let condition_bits = load_slot(builder, slots, condition)?;
            let expected =
                if matches!(instruction.opcode, OpCode::JumpTrue | OpCode::JumpTrueConstant) {
                    Value::bool(true).as_raw_bits()
                } else {
                    Value::bool(false).as_raw_bits()
                };
            let is_taken = builder
                .ins()
                .icmp_imm_s(IntCC::Equal, condition_bits, expected as i64);
            let target = instruction
                .branch_target
                .expect("verified conditional target");
            builder.ins().brif(
                is_taken,
                block_for_offset(bytecode, blocks, target),
                &[],
                blocks[index + 1],
                &[],
            );
        }
        OpCode::Ret => {
            let Some(src) = local_index(instruction.operands[0], instruction.width) else {
                emit_side_exit(builder, activation, instruction.offset)?;
                return Ok(());
            };
            let raw = load_slot(builder, slots, src)?;
            let memory_flags = plain_mem_flags();
            builder
                .ins()
                .store(memory_flags, raw, activation, ACTIVATION_RETURN_VALUE_OFFSET);
            emit_status_return(builder, STATUS_RETURNED);
        }
        _ => emit_side_exit(builder, activation, instruction.offset)?,
    }

    Ok(())
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

fn emit_smi_arithmetic(
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
    let left_bits = load_slot(builder, slots, left)?;
    let tag = builder.ins().ushr_imm_u(left_bits, VALUE_TAG_SHIFT);
    let is_smi = builder.ins().icmp_imm_u(IntCC::Equal, tag, SMI_TAG as i64);
    let smi_block = builder.create_block();
    let range_block = builder.create_block();
    let slow_exit_block = builder.create_block();
    builder
        .ins()
        .brif(is_smi, smi_block, &[], slow_exit_block, &[]);

    builder.switch_to_block(slow_exit_block);
    emit_side_exit(builder, activation, instruction.offset)?;

    builder.switch_to_block(smi_block);
    let left_i32 = builder.ins().ireduce(types::I32, left_bits);
    let left_i64 = builder.ins().sextend(types::I64, left_i32);
    let result_i64 = if instruction.opcode == OpCode::AddImm {
        builder.ins().iadd_imm_s(left_i64, immediate)
    } else {
        builder.ins().iadd_imm_s(left_i64, -immediate)
    };
    let at_least_min =
        builder
            .ins()
            .icmp_imm_s(IntCC::SignedGreaterThanOrEqual, result_i64, i32::MIN as i64);
    let at_most_max =
        builder
            .ins()
            .icmp_imm_s(IntCC::SignedLessThanOrEqual, result_i64, i32::MAX as i64);
    let fits_smi = builder.ins().band(at_least_min, at_most_max);
    builder
        .ins()
        .brif(fits_smi, range_block, &[], slow_exit_block, &[]);

    builder.switch_to_block(range_block);
    let result_i32 = builder.ins().ireduce(types::I32, result_i64);
    let payload = builder.ins().uextend(types::I64, result_i32);
    let tag_bits = builder
        .ins()
        .iconst(types::I64, ((SMI_TAG as u64) << VALUE_TAG_SHIFT) as i64);
    let boxed = builder.ins().bor(tag_bits, payload);
    store_slot(builder, slots, dest, boxed)?;
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
                ShadowFrameOwner,
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
        let prepared = compile_prototype(verified).unwrap();
        assert_eq!(prepared.required_frame_slots(), slots.len());
        assert!(prepared.safepoints().records().is_empty());
        let mapped_len = ExecutableCodeCache::mapped_len_for(&prepared).unwrap();
        let mut cache = ExecutableCodeCache::new(mapped_len, 1).unwrap();
        cache.insert(1, prepared).unwrap();

        let mut owned = ContextBuilder::new().build().unwrap();
        let mut outcome = None;
        let mut returned_slots = None;
        owned.with_jit_context(|context| {
            let mut slots: Vec<JitSlot> = slots
                .into_iter()
                .map(|value| JitSlot::try_from_value(context, value).unwrap())
                .collect();
            let loaded = cache.get(1).unwrap().unwrap();
            let code_len = loaded.code_len();
            let validated = {
                let mut frame = ShadowFrameOwner::new(&mut slots, loaded.safepoints()).unwrap();
                let (mut budget, _) =
                    DeterministicInterruptBudget::new(NonZeroU32::new(1_000).unwrap());
                let mut activation =
                    ActivationOwner::new(context, &mut frame, &mut budget).unwrap();
                // SAFETY: The loaded artifact owns the exact entry bytes and maps used by this
                // activation for the complete synchronous call.
                let status = unsafe { loaded.call(&mut activation) }.unwrap();
                activation.validate_result(status).unwrap()
            };
            outcome = Some(validated);
            returned_slots = Some((slots.iter().map(JitSlot::value).collect::<Vec<_>>(), code_len));
        });

        let (slots, code_len) = returned_slots.unwrap();
        (outcome.unwrap(), slots, code_len)
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
    fn non_smi_overflow_unsupported_and_backedge_paths_side_exit_exactly() {
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
        unsupported.extend(encode(OpCode::Neg, &[local(0), local(0)]));
        unsupported.extend(encode(OpCode::Ret, &[local(0)]));
        let unsupported_code = verify(&unsupported, 1);
        assert_eq!(
            execute(&unsupported_code, vec![Value::raw_smi(1)]).0,
            ActivationOutcome::SideExit(unsupported_offset)
        );

        let backedge = encode(OpCode::Jump, &[0]);
        let backedge_code = verify(&backedge, 0);
        assert_eq!(execute(&backedge_code, Vec::new()).0, ActivationOutcome::SideExit(0));
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
    }
}
