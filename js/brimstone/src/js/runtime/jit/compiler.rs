//! Minimal Cranelift baseline prototype for a non-allocating bytecode subset.
//!
//! This is deliberately not a VM dispatch tier. It accepts only checked, trusted in-process
//! bytecode; emits no calls, relocations, traps, allocation points, or native safepoints; and exits
//! to the interpreter before every unsupported operation and backedge. Boxed values are reloaded
//! from and committed to the lifetime-owned shadow slot array at each supported operation, so no
//! moving pointer is embedded in code or carried across a safepoint.

use std::mem::size_of;

use cranelift_codegen::{
    Context,
    control::ControlPlane,
    ir::{
        AbiParam, Block, Function, InstBuilder, MemFlagsData, Signature, UserFuncName,
        Value as ClifValue, condcodes::IntCC, types,
    },
    isa::{CallConv, OwnedTargetIsa},
    settings::{self, Configurable},
};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};

use super::abi::{
    ACTIVATION_ABI_VERSION_OFFSET, ACTIVATION_FRAME_OFFSET, ACTIVATION_HELPERS_OFFSET,
    ACTIVATION_RESERVED_OFFSET, ACTIVATION_RETURN_VALUE_OFFSET, ACTIVATION_SIDE_EXIT_OFFSET,
    ACTIVATION_STRUCT_SIZE_OFFSET, GENERATED_CODE_ABI_VERSION, JIT_ACTIVATION_SIZE, NO_SAFEPOINT,
    SHADOW_FRAME_BYTECODE_OFFSET, SHADOW_FRAME_PREVIOUS_OFFSET,
    SHADOW_FRAME_SAFEPOINT_INDEX_OFFSET, SHADOW_FRAME_SLOT_COUNT_OFFSET, SHADOW_FRAME_SLOTS_OFFSET,
    STATUS_INVALID_ACTIVATION, STATUS_RETURNED, STATUS_SIDE_EXIT, SafepointRecord,
};
use crate::runtime::{
    Value,
    bytecode::{
        WidthEnum,
        instruction::OpCode,
        metadata::EffectFlags,
        verifier::{DecodedOperand, VerifiedBytecode, VerifiedInstruction},
    },
    value::SMI_TAG,
};

pub(crate) const MAX_PROTOTYPE_INSTRUCTIONS: usize = 100_000;
pub(crate) const MAX_PROTOTYPE_FRAME_SLOTS: usize = 1 << 20;
pub(crate) const MAX_PROTOTYPE_CODE_BYTES: usize = 8 * 1024 * 1024;

const VALUE_TAG_SHIFT: i64 = 48;
const SLOT_BYTES: usize = size_of::<u64>();

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
    AllocationFailed,
}

/// Relocation-free machine code plus the deliberately empty first-gate safepoint table.
pub(crate) struct CompiledPrototype {
    machine_code: Vec<u8>,
    required_frame_slots: usize,
    safepoints: Vec<SafepointRecord>,
}

impl CompiledPrototype {
    pub(crate) fn machine_code(&self) -> &[u8] {
        &self.machine_code
    }

    pub(crate) const fn required_frame_slots(&self) -> usize {
        self.required_frame_slots
    }

    pub(crate) fn safepoints(&self) -> &[SafepointRecord] {
        &self.safepoints
    }
}

pub(crate) fn compile_prototype(
    bytecode: &VerifiedBytecode<'_>,
) -> Result<CompiledPrototype, BaselineCompileError> {
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
        build_function(&mut builder, bytecode)?;
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

    let mut machine_code = Vec::new();
    machine_code
        .try_reserve_exact(compiled.code_buffer().len())
        .map_err(|_| BaselineCompileError::AllocationFailed)?;
    machine_code.extend_from_slice(compiled.code_buffer());

    Ok(CompiledPrototype {
        machine_code,
        required_frame_slots: bytecode.num_locals(),
        // No helper calls or backedges are emitted, so there are no native safepoints yet.
        safepoints: Vec::new(),
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
) -> Result<(), BaselineCompileError> {
    let entry_block = builder.create_block();
    let activation_header_block = builder.create_block();
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
    let slots_var = builder.declare_var(types::I64);

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
    activation_valid = and_loaded_imm(
        builder,
        activation_valid,
        types::I64,
        activation,
        ACTIVATION_HELPERS_OFFSET,
        0,
    );
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
    builder
        .ins()
        .brif(activation_valid, frame_header_block, &[], invalid_activation_block, &[]);

    builder.switch_to_block(frame_header_block);
    let mut frame_valid =
        compare_loaded_imm(builder, types::I64, frame, SHADOW_FRAME_PREVIOUS_OFFSET, 0);
    let memory_flags = plain_mem_flags();
    let slots = builder
        .ins()
        .load(types::I64, memory_flags, frame, SHADOW_FRAME_SLOTS_OFFSET);
    let slots_are_nonnull = builder.ins().icmp_imm_s(IntCC::NotEqual, slots, 0);
    frame_valid = and_condition(builder, frame_valid, slots_are_nonnull);
    frame_valid = and_loaded_imm(
        builder,
        frame_valid,
        types::I64,
        frame,
        SHADOW_FRAME_SLOT_COUNT_OFFSET,
        bytecode.num_locals() as i64,
    );
    frame_valid =
        and_loaded_imm(builder, frame_valid, types::I32, frame, SHADOW_FRAME_BYTECODE_OFFSET, 0);
    frame_valid = and_loaded_imm(
        builder,
        frame_valid,
        types::I32,
        frame,
        SHADOW_FRAME_SAFEPOINT_INDEX_OFFSET,
        NO_SAFEPOINT as i32 as i64,
    );
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
            slots_var,
            bytecode,
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
    slots_var: Variable,
    bytecode: &VerifiedBytecode<'_>,
    blocks: &[Block],
    index: usize,
    instruction: &VerifiedInstruction,
) -> Result<(), BaselineCompileError> {
    let activation = builder.use_var(activation_var);
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
    // `ExecutableMemory::call` accepts only a lifetime-branded activation whose activation,
    // frame, and slot storage remain live for the entire synchronous call. Marking accesses
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
    use std::ptr;

    use super::*;
    use crate::runtime::{
        Value,
        bytecode::{
            instruction::{
                extra_wide_prefix_index_to_opcode_index, wide_prefix_index_to_opcode_index,
            },
            verifier::{ConstantKind, VerificationLimits, VerifiedBytecode},
        },
        jit::{
            abi::{
                ActivationOutcome, ActivationOwner, GeneratedEntry, JitActivation, ShadowFrameOwner,
            },
            code_cache::{ExecutableCodeCache, ExecutableMemory},
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
        mut slots: Vec<u64>,
    ) -> (ActivationOutcome, Vec<u64>, usize) {
        let compiled = compile_prototype(verified).unwrap();
        assert_eq!(compiled.required_frame_slots(), slots.len());
        assert!(compiled.safepoints().is_empty());
        let mapped_len = ExecutableMemory::mapped_len_for(compiled.machine_code().len()).unwrap();
        let mut cache = ExecutableCodeCache::new(mapped_len, 1).unwrap();
        cache.insert(1, compiled.machine_code()).unwrap();
        let code_len = cache.get(1).unwrap().unwrap().code_len();

        let outcome = {
            let mut frame = ShadowFrameOwner::new(&mut slots).unwrap();
            let mut activation = ActivationOwner::new(&mut frame);
            let status = {
                let code = cache.get(1).unwrap().unwrap();
                // SAFETY: `compile_prototype` emits exactly the documented generated-entry ABI;
                // the RX mapping owns those unchanged bytes through this synchronous call.
                unsafe { code.call(&mut activation) }.unwrap()
            };
            activation.validate_result(status, verified).unwrap()
        };

        (outcome, slots, code_len)
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
                    let seed = Value::undefined().as_raw_bits();
                    let (outcome, slots, code_len) = execute(&verified, vec![seed; 2]);

                    let expected = if opcode == OpCode::AddImm {
                        i32::from(left) + i32::from(immediate)
                    } else {
                        i32::from(left) - i32::from(immediate)
                    };
                    let expected_bits = Value::raw_smi(expected).as_raw_bits();
                    assert_eq!(outcome, ActivationOutcome::Returned(expected_bits));
                    assert_eq!(slots[1], expected_bits);
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

        let seed = Value::undefined().as_raw_bits();
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
        let seed = Value::undefined().as_raw_bits();

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
        assert_eq!(wide_slots[128], Value::raw_smi(321).as_raw_bits());

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
        let seed = Value::undefined().as_raw_bits();
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
        let seed = Value::undefined().as_raw_bits();

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
            execute(&wide_code, vec![Value::bool(true).as_raw_bits(), seed],).0,
            ActivationOutcome::Returned(Value::raw_smi(7).as_raw_bits())
        );
        assert_eq!(
            execute(&wide_code, vec![Value::bool(false).as_raw_bits(), seed],).0,
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
            execute(&extra_wide_code, vec![Value::bool(true).as_raw_bits(), seed],).0,
            ActivationOutcome::Returned(Value::raw_smi(7).as_raw_bits())
        );
        assert_eq!(
            execute(&extra_wide_code, vec![Value::bool(false).as_raw_bits(), seed],).0,
            ActivationOutcome::Returned(Value::raw_smi(1).as_raw_bits())
        );
    }

    #[test]
    fn raw_entry_rejects_null_and_invalid_headers_before_frame_access() {
        let bytes = encode(OpCode::Ret, &[local(0)]);
        let verified = verify(&bytes, 1);
        let compiled = compile_prototype(&verified).unwrap();
        let mapped_len = ExecutableMemory::mapped_len_for(compiled.machine_code().len()).unwrap();
        let mut cache = ExecutableCodeCache::new(mapped_len, 1).unwrap();
        cache.insert(1, compiled.machine_code()).unwrap();
        let code = cache.get(1).unwrap().unwrap();

        // SAFETY: These calls deliberately exercise the private raw ABI. The null pointer is
        // rejected before any load. The second pointer identifies a live `JitActivation` header;
        // its invalid version must branch to rejection before its deliberately dangling frame is
        // read. Arbitrary non-null addresses are not covered by this test or the raw ABI contract.
        unsafe {
            let entry: GeneratedEntry = std::mem::transmute(code.start_address());
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
            execute(&overflow_code, vec![Value::raw_smi(i32::MAX).as_raw_bits()]).0,
            ActivationOutcome::SideExit(0)
        );
        assert_eq!(
            execute(&overflow_code, vec![Value::null().as_raw_bits()]).0,
            ActivationOutcome::SideExit(0)
        );

        let mut unsupported = encode(OpCode::LoadImmediate, &[local(0), 4]);
        let unsupported_offset = unsupported.len();
        unsupported.extend(encode(OpCode::Neg, &[local(0), local(0)]));
        unsupported.extend(encode(OpCode::Ret, &[local(0)]));
        let unsupported_code = verify(&unsupported, 1);
        assert_eq!(
            execute(&unsupported_code, vec![Value::raw_smi(1).as_raw_bits()]).0,
            ActivationOutcome::SideExit(unsupported_offset)
        );

        let backedge = encode(OpCode::Jump, &[0]);
        let backedge_code = verify(&backedge, 0);
        assert_eq!(execute(&backedge_code, Vec::new()).0, ActivationOutcome::SideExit(0));
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
