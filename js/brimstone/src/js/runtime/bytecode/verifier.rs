//! Checked bytecode decoding and structural JIT-admission verification.
//!
//! This module intentionally does not use `InstructionIterator`: that iterator is for already
//! trusted, compiler-produced bytecode and performs typed pointer casts before validating bounds.
//!
//! This is a defense-in-depth gate for bytecode produced by Brimstone's trusted in-process
//! compiler. It is **not** a safe serialized- or untrusted-bytecode loader. In particular, dynamic
//! scope-index and parent-depth validity depends on scope metadata that is not present in
//! `VerificationLimits`, and side-exit interpreter paths still assume compiler-produced bytecode.

#![allow(dead_code)]

use super::{
    instruction::{
        OpCode, extra_wide_prefix_index_to_opcode_index, wide_prefix_index_to_opcode_index,
    },
    metadata::{ControlFlow, EffectFlags, OperandAccess},
    operand::OperandType,
    stack_frame::{
        CLOSURE_SLOT_INDEX, FIRST_ARGUMENT_SLOT_INDEX, RECEIVER_SLOT_INDEX, SCOPE_SLOT_INDEX,
    },
    width::WidthEnum,
};

pub(crate) const DEFAULT_MAX_BYTECODE_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const DEFAULT_MAX_INSTRUCTIONS: usize = 1_000_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ConstantKind {
    AnyValue,
    PropertyKey,
    String,
    BytecodeFunction,
    CompiledRegExp,
    ScopeNames,
    ClassNames { num_arguments: usize },
    JumpOffset(isize),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CacheKind {
    Global,
    NamedProperty,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct VerificationLimits<'a> {
    pub(crate) num_locals: usize,
    pub(crate) num_arguments: usize,
    pub(crate) constants: &'a [ConstantKind],
    pub(crate) caches: &'a [CacheKind],
    pub(crate) max_bytecode_bytes: usize,
    pub(crate) max_instructions: usize,
}

impl VerificationLimits<'_> {
    pub(crate) const fn empty(num_locals: usize, num_arguments: usize) -> Self {
        Self {
            num_locals,
            num_arguments,
            constants: &[],
            caches: &[],
            max_bytecode_bytes: DEFAULT_MAX_BYTECODE_BYTES,
            max_instructions: DEFAULT_MAX_INSTRUCTIONS,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DecodedOperand {
    raw: u32,
    kind: OperandType,
}

impl DecodedOperand {
    pub(crate) const fn as_unsigned(self) -> usize {
        self.raw as usize
    }

    pub(crate) const fn as_signed(self, width: WidthEnum) -> isize {
        match width {
            WidthEnum::Narrow => (self.raw as u8 as i8) as isize,
            WidthEnum::Wide => (self.raw as u16 as i16) as isize,
            WidthEnum::ExtraWide => (self.raw as i32) as isize,
        }
    }

    pub(crate) const fn kind(self) -> OperandType {
        self.kind
    }
}

#[derive(Clone, Debug)]
pub(crate) struct VerifiedInstruction {
    pub(crate) offset: usize,
    pub(crate) opcode_index: usize,
    pub(crate) next_offset: usize,
    pub(crate) width: WidthEnum,
    pub(crate) opcode: OpCode,
    pub(crate) operands: Vec<DecodedOperand>,
    pub(crate) branch_target: Option<usize>,
    /// Exact constant-table index and raw signed offset for a constant-backed branch.
    pub(crate) branch_constant: Option<(usize, isize)>,
    pub(crate) effects: EffectFlags,
}

#[derive(Debug)]
pub(crate) struct VerifiedBytecode<'a> {
    bytes: &'a [u8],
    instructions: Vec<VerifiedInstruction>,
    num_locals: usize,
    num_arguments: usize,
    num_constants: usize,
    num_caches: usize,
}

impl<'a> VerifiedBytecode<'a> {
    pub(crate) fn verify(
        bytes: &'a [u8],
        limits: VerificationLimits<'_>,
    ) -> Result<Self, VerificationError> {
        if bytes.is_empty() {
            return Err(VerificationError::EmptyBytecode);
        }
        if bytes.len() > limits.max_bytecode_bytes {
            return Err(VerificationError::BytecodeTooLarge {
                bytecode_len: bytes.len(),
                maximum: limits.max_bytecode_bytes,
            });
        }

        let mut instructions = Vec::new();
        instructions
            .try_reserve(bytes.len().min(limits.max_instructions))
            .map_err(|_| VerificationError::AllocationFailed)?;
        let mut offset = 0;

        while offset < bytes.len() {
            if instructions.len() == limits.max_instructions {
                return Err(VerificationError::TooManyInstructions {
                    maximum: limits.max_instructions,
                });
            }
            let instruction = decode_one(bytes, offset, limits)?;
            offset = instruction.next_offset;
            instructions.push(instruction);
        }

        debug_assert_eq!(offset, bytes.len());

        let mut starts = Vec::new();
        starts
            .try_reserve_exact(instructions.len())
            .map_err(|_| VerificationError::AllocationFailed)?;
        starts.extend(instructions.iter().map(|instruction| instruction.offset));

        for instruction in &mut instructions {
            let metadata = instruction.opcode.metadata();
            let target_operand = match metadata.control_flow {
                ControlFlow::Jump => Some(0),
                ControlFlow::ConditionalJump => Some(1),
                _ => None,
            };

            if let Some(target_operand) = target_operand {
                let operand = instruction.operands[target_operand];
                let (relative, branch_constant) = if operand.kind() == OperandType::ConstantIndex {
                    let index = operand.as_unsigned();
                    match limits.constants[index] {
                        ConstantKind::JumpOffset(relative) => (relative, Some((index, relative))),
                        _ => {
                            return Err(VerificationError::WrongConstantKind {
                                offset: instruction.offset,
                                operand: target_operand,
                                index,
                                expected: ConstantKindName::JumpOffset,
                            });
                        }
                    }
                } else {
                    (operand.as_signed(instruction.width), None)
                };

                let target = instruction.offset.checked_add_signed(relative).ok_or(
                    VerificationError::BranchTargetOverflow {
                        offset: instruction.offset,
                        relative,
                    },
                )?;

                if starts.binary_search(&target).is_err() {
                    return Err(VerificationError::BranchTargetNotInstruction {
                        offset: instruction.offset,
                        target,
                    });
                }

                instruction.branch_target = Some(target);
                instruction.branch_constant = branch_constant;
                if target <= instruction.offset {
                    instruction.effects = instruction
                        .effects
                        .union(EffectFlags::BACKEDGE)
                        .union(EffectFlags::SAFEPOINT);
                }
            }

            if matches!(metadata.control_flow, ControlFlow::Fallthrough | ControlFlow::Suspend)
                && instruction.next_offset == bytes.len()
            {
                return Err(VerificationError::FallsOffEnd { offset: instruction.offset });
            }

            if metadata.control_flow == ControlFlow::ConditionalJump
                && instruction.next_offset == bytes.len()
            {
                return Err(VerificationError::FallsOffEnd { offset: instruction.offset });
            }
        }

        Ok(Self {
            bytes,
            instructions,
            num_locals: limits.num_locals,
            num_arguments: limits.num_arguments,
            num_constants: limits.constants.len(),
            num_caches: limits.caches.len(),
        })
    }

    pub(crate) const fn bytes(&self) -> &'a [u8] {
        self.bytes
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

    pub(crate) const fn num_constants(&self) -> usize {
        self.num_constants
    }

    pub(crate) const fn num_caches(&self) -> usize {
        self.num_caches
    }

    pub(crate) fn is_instruction_start(&self, offset: usize) -> bool {
        self.instructions
            .binary_search_by_key(&offset, |instruction| instruction.offset)
            .is_ok()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ConstantKindName {
    AnyValue,
    PropertyKey,
    String,
    BytecodeFunction,
    CompiledRegExp,
    ScopeNames,
    ClassNames,
    JumpOffset,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum VerificationError {
    EmptyBytecode,
    BytecodeTooLarge {
        bytecode_len: usize,
        maximum: usize,
    },
    TooManyInstructions {
        maximum: usize,
    },
    AllocationFailed,
    InvalidOpcode {
        offset: usize,
        raw: u8,
    },
    TruncatedPrefix {
        offset: usize,
        width: WidthEnum,
    },
    NonZeroPrefixPadding {
        offset: usize,
        padding_offset: usize,
    },
    NestedWidthPrefix {
        offset: usize,
        opcode_index: usize,
    },
    TruncatedOperands {
        offset: usize,
        required_end: usize,
        bytecode_len: usize,
    },
    NonCanonicalWidth {
        offset: usize,
        encoded: WidthEnum,
        required: WidthEnum,
    },
    InvalidRegister {
        offset: usize,
        operand: usize,
        raw: isize,
    },
    RegisterOutOfBounds {
        offset: usize,
        operand: usize,
        raw: isize,
    },
    ReadOnlyRegisterWrite {
        offset: usize,
        operand: usize,
        raw: isize,
    },
    RequiredLocalRegister {
        offset: usize,
        operand: usize,
        raw: isize,
    },
    InvalidRegisterRange {
        offset: usize,
        operand: usize,
        raw: isize,
        count: usize,
    },
    ConstantOutOfBounds {
        offset: usize,
        operand: usize,
        index: usize,
    },
    WrongConstantKind {
        offset: usize,
        operand: usize,
        index: usize,
        expected: ConstantKindName,
    },
    CacheOutOfBounds {
        offset: usize,
        operand: usize,
        index: usize,
    },
    WrongCacheKind {
        offset: usize,
        operand: usize,
        index: usize,
        expected: CacheKind,
    },
    InvalidFlags {
        offset: usize,
        operand: usize,
        raw: usize,
        valid_mask: usize,
    },
    InvalidFlagCombination {
        offset: usize,
        operand: usize,
        raw: usize,
    },
    InvalidEnum {
        offset: usize,
        operand: usize,
        raw: usize,
    },
    BranchTargetOverflow {
        offset: usize,
        relative: isize,
    },
    BranchTargetNotInstruction {
        offset: usize,
        target: usize,
    },
    FallsOffEnd {
        offset: usize,
    },
}

fn decode_one(
    bytes: &[u8],
    offset: usize,
    limits: VerificationLimits<'_>,
) -> Result<VerifiedInstruction, VerificationError> {
    let raw = bytes[offset];
    let first = OpCode::try_from_u8(raw).ok_or(VerificationError::InvalidOpcode { offset, raw })?;

    let (width, opcode_index, opcode) = match first {
        OpCode::WidePrefix | OpCode::ExtraWidePrefix => {
            let width = if first == OpCode::WidePrefix {
                WidthEnum::Wide
            } else {
                WidthEnum::ExtraWide
            };
            let opcode_index = if width == WidthEnum::Wide {
                wide_prefix_index_to_opcode_index(offset)
            } else {
                extra_wide_prefix_index_to_opcode_index(offset)
            };

            if opcode_index >= bytes.len() {
                return Err(VerificationError::TruncatedPrefix { offset, width });
            }

            for (padding_offset, &padding) in
                bytes.iter().enumerate().take(opcode_index).skip(offset + 1)
            {
                if padding != 0 {
                    return Err(VerificationError::NonZeroPrefixPadding { offset, padding_offset });
                }
            }

            let raw = bytes[opcode_index];
            let opcode = OpCode::try_from_u8(raw)
                .ok_or(VerificationError::InvalidOpcode { offset: opcode_index, raw })?;
            if matches!(opcode, OpCode::WidePrefix | OpCode::ExtraWidePrefix) {
                return Err(VerificationError::NestedWidthPrefix { offset, opcode_index });
            }

            (width, opcode_index, opcode)
        }
        opcode => (WidthEnum::Narrow, offset, opcode),
    };

    let metadata = opcode.metadata();
    let operand_width = width.num_bytes();
    let operand_bytes = metadata
        .operands
        .len()
        .checked_mul(operand_width)
        .and_then(|len| opcode_index.checked_add(1 + len))
        .ok_or(VerificationError::TruncatedOperands {
            offset,
            required_end: usize::MAX,
            bytecode_len: bytes.len(),
        })?;

    if operand_bytes > bytes.len() {
        return Err(VerificationError::TruncatedOperands {
            offset,
            required_end: operand_bytes,
            bytecode_len: bytes.len(),
        });
    }

    let mut operands = Vec::new();
    operands
        .try_reserve_exact(metadata.operands.len())
        .map_err(|_| VerificationError::AllocationFailed)?;
    let mut required_width = WidthEnum::Narrow;
    for (operand_index, &kind) in metadata.operands.iter().enumerate() {
        let start = opcode_index + 1 + operand_index * operand_width;
        let raw = read_unsigned(bytes, start, width);
        let operand = DecodedOperand { raw, kind };
        required_width = required_width.max(minimum_width(operand, width));
        validate_operand(opcode, offset, operand_index, operand, width, limits)?;
        operands.push(operand);
    }

    if required_width != width {
        return Err(VerificationError::NonCanonicalWidth {
            offset,
            encoded: width,
            required: required_width,
        });
    }

    validate_register_ranges(opcode, offset, width, &operands, limits)?;
    validate_immediates(opcode, offset, &operands)?;

    Ok(VerifiedInstruction {
        offset,
        opcode_index,
        next_offset: operand_bytes,
        width,
        opcode,
        operands,
        branch_target: None,
        branch_constant: None,
        effects: metadata.effects,
    })
}

fn read_unsigned(bytes: &[u8], start: usize, width: WidthEnum) -> u32 {
    match width {
        WidthEnum::Narrow => bytes[start] as u32,
        WidthEnum::Wide => u16::from_ne_bytes([bytes[start], bytes[start + 1]]) as u32,
        WidthEnum::ExtraWide => u32::from_ne_bytes([
            bytes[start],
            bytes[start + 1],
            bytes[start + 2],
            bytes[start + 3],
        ]),
    }
}

fn minimum_width(operand: DecodedOperand, encoded_width: WidthEnum) -> WidthEnum {
    match operand.kind() {
        OperandType::Register | OperandType::SInt => {
            let value = operand.as_signed(encoded_width);
            if i8::try_from(value).is_ok() {
                WidthEnum::Narrow
            } else if i16::try_from(value).is_ok() {
                WidthEnum::Wide
            } else {
                WidthEnum::ExtraWide
            }
        }
        OperandType::UInt | OperandType::ConstantIndex | OperandType::CacheIndex => {
            let value = operand.as_unsigned();
            if u8::try_from(value).is_ok() {
                WidthEnum::Narrow
            } else if u16::try_from(value).is_ok() {
                WidthEnum::Wide
            } else {
                WidthEnum::ExtraWide
            }
        }
    }
}

fn validate_operand(
    opcode: OpCode,
    offset: usize,
    operand_index: usize,
    operand: DecodedOperand,
    width: WidthEnum,
    limits: VerificationLimits<'_>,
) -> Result<(), VerificationError> {
    match operand.kind() {
        OperandType::Register => validate_register(
            opcode,
            offset,
            operand_index,
            operand.as_signed(width),
            opcode.metadata().operand_accesses[operand_index],
            limits,
        ),
        OperandType::ConstantIndex => {
            let index = operand.as_unsigned();
            let Some(actual) = limits.constants.get(index) else {
                return Err(VerificationError::ConstantOutOfBounds {
                    offset,
                    operand: operand_index,
                    index,
                });
            };
            let expected = expected_constant_kind(opcode, operand_index)
                .expect("every ConstantIndex operand has an explicit expected kind");
            if !constant_kind_matches(*actual, expected) {
                return Err(VerificationError::WrongConstantKind {
                    offset,
                    operand: operand_index,
                    index,
                    expected,
                });
            }
            Ok(())
        }
        OperandType::CacheIndex => {
            let index = operand.as_unsigned();
            let Some(&actual) = limits.caches.get(index) else {
                return Err(VerificationError::CacheOutOfBounds {
                    offset,
                    operand: operand_index,
                    index,
                });
            };
            let expected = match opcode {
                OpCode::LoadGlobal | OpCode::LoadGlobalOrUnresolved | OpCode::StoreGlobal => {
                    CacheKind::Global
                }
                OpCode::GetNamedProperty | OpCode::SetNamedProperty => CacheKind::NamedProperty,
                _ => unreachable!("cache operands are exhaustively defined"),
            };
            if actual != expected {
                return Err(VerificationError::WrongCacheKind {
                    offset,
                    operand: operand_index,
                    index,
                    expected,
                });
            }
            Ok(())
        }
        OperandType::UInt | OperandType::SInt => Ok(()),
    }
}

fn expected_constant_kind(opcode: OpCode, operand: usize) -> Option<ConstantKindName> {
    use ConstantKindName::*;
    use OpCode::*;

    match (opcode, operand) {
        (LoadConstant, 1) => Some(AnyValue),
        (
            LoadGlobal
            | LoadGlobalOrUnresolved
            | StoreGlobal
            | LoadDynamic
            | LoadDynamicOrUnresolved
            | StoreDynamic,
            1,
        ) => Some(String),
        (JumpConstant, 0)
        | (
            JumpTrueConstant
            | JumpToBooleanTrueConstant
            | JumpFalseConstant
            | JumpToBooleanFalseConstant
            | JumpNotUndefinedConstant
            | JumpNullishConstant
            | JumpNotNullishConstant,
            1,
        ) => Some(JumpOffset),
        (NewClosure | NewAsyncClosure | NewGenerator | NewAsyncGenerator, 1) | (NewClass, 2) => {
            Some(BytecodeFunction)
        }
        (NewRegExp, 1) => Some(CompiledRegExp),
        (NewClass, 1) => Some(ClassNames),
        (NewPrivateSymbol, 1)
        | (DeleteBinding, 1)
        | (CheckTdz, 1)
        | (ErrorConst, 0)
        | (ThrowNewError, 1) => Some(String),
        (GetNamedProperty, 2)
        | (SetNamedProperty | DefineNamedProperty, 1)
        | (GetNamedSuperProperty, 3)
        | (GetMethod, 2) => Some(PropertyKey),
        (PushLexicalScope | PushFunctionScope, 0) | (PushWithScope, 1) => Some(ScopeNames),
        _ => None,
    }
}

fn constant_kind_matches(actual: ConstantKind, expected: ConstantKindName) -> bool {
    match expected {
        // Only descriptors whose table entry is an ordinary JavaScript register value may be
        // loaded. Function bytecode, regexp programs, scope/class metadata, and jump offsets are
        // internal pointers/data even though some happen to occupy the same physical table.
        ConstantKindName::AnyValue => matches!(
            actual,
            ConstantKind::AnyValue | ConstantKind::PropertyKey | ConstantKind::String
        ),
        ConstantKindName::PropertyKey => {
            matches!(actual, ConstantKind::PropertyKey | ConstantKind::String)
        }
        ConstantKindName::String => actual == ConstantKind::String,
        ConstantKindName::BytecodeFunction => actual == ConstantKind::BytecodeFunction,
        ConstantKindName::CompiledRegExp => actual == ConstantKind::CompiledRegExp,
        ConstantKindName::ScopeNames => actual == ConstantKind::ScopeNames,
        ConstantKindName::ClassNames => matches!(actual, ConstantKind::ClassNames { .. }),
        ConstantKindName::JumpOffset => matches!(actual, ConstantKind::JumpOffset(_)),
    }
}

fn validate_register(
    opcode: OpCode,
    offset: usize,
    operand_index: usize,
    raw: isize,
    access: OperandAccess,
    limits: VerificationLimits<'_>,
) -> Result<(), VerificationError> {
    if opcode == OpCode::GeneratorStart && operand_index == 0 && raw >= 0 {
        return Err(VerificationError::RequiredLocalRegister {
            offset,
            operand: operand_index,
            raw,
        });
    }

    if raw < 0 {
        let local = (-1_isize)
            .checked_sub(raw)
            .and_then(|value| usize::try_from(value).ok())
            .ok_or(VerificationError::InvalidRegister { offset, operand: operand_index, raw })?;
        if local >= limits.num_locals {
            return Err(VerificationError::RegisterOutOfBounds {
                offset,
                operand: operand_index,
                raw,
            });
        }
        return Ok(());
    }

    let raw_usize = raw as usize;
    if raw_usize >= FIRST_ARGUMENT_SLOT_INDEX {
        let argument = raw_usize - FIRST_ARGUMENT_SLOT_INDEX;
        if argument >= limits.num_arguments {
            return Err(VerificationError::RegisterOutOfBounds {
                offset,
                operand: operand_index,
                raw,
            });
        }
        return Ok(());
    }

    if !matches!(raw_usize, SCOPE_SLOT_INDEX | CLOSURE_SLOT_INDEX | RECEIVER_SLOT_INDEX) {
        return Err(VerificationError::InvalidRegister { offset, operand: operand_index, raw });
    }

    if access.may_write() {
        let write_allowed = match raw_usize {
            SCOPE_SLOT_INDEX => opcode == OpCode::Mov,
            RECEIVER_SLOT_INDEX => matches!(opcode, OpCode::Mov | OpCode::LoadEmpty),
            CLOSURE_SLOT_INDEX => false,
            _ => false,
        };
        if !write_allowed {
            return Err(VerificationError::ReadOnlyRegisterWrite {
                offset,
                operand: operand_index,
                raw,
            });
        }
    }

    Ok(())
}

fn validate_register_ranges(
    opcode: OpCode,
    offset: usize,
    width: WidthEnum,
    operands: &[DecodedOperand],
    limits: VerificationLimits<'_>,
) -> Result<(), VerificationError> {
    for (operand_index, &access) in opcode.metadata().operand_accesses.iter().enumerate() {
        let count = match access {
            OperandAccess::ReadRange { length_operand } => {
                operands[length_operand as usize].as_unsigned()
            }
            OperandAccess::ReadClassMethods { class_names_operand } => {
                let constant_index = operands[class_names_operand as usize].as_unsigned();
                match limits.constants[constant_index] {
                    ConstantKind::ClassNames { num_arguments } => num_arguments,
                    _ => unreachable!("NewClass ClassNames kind was validated first"),
                }
            }
            _ => continue,
        };
        let raw = operands[operand_index].as_signed(width);
        if raw >= 0 {
            return Err(VerificationError::InvalidRegisterRange {
                offset,
                operand: operand_index,
                raw,
                count,
            });
        }
        let first = (-1_isize)
            .checked_sub(raw)
            .and_then(|value| usize::try_from(value).ok())
            .ok_or(VerificationError::InvalidRegisterRange {
                offset,
                operand: operand_index,
                raw,
                count,
            })?;
        if first
            .checked_add(count)
            .is_none_or(|end| end > limits.num_locals)
        {
            return Err(VerificationError::InvalidRegisterRange {
                offset,
                operand: operand_index,
                raw,
                count,
            });
        }
    }
    Ok(())
}

fn validate_immediates(
    opcode: OpCode,
    offset: usize,
    operands: &[DecodedOperand],
) -> Result<(), VerificationError> {
    let flags = match opcode {
        OpCode::DefineProperty | OpCode::DefinePrivateProperty => Some((3, 0b111)),
        OpCode::CallMaybeEval => Some((4, 0b111_1111)),
        OpCode::CallMaybeEvalVarargs => Some((3, 0b111_1111)),
        _ => None,
    };
    if let Some((operand, valid_mask)) = flags {
        let raw = operands[operand].as_unsigned();
        if raw & !valid_mask != 0 {
            return Err(VerificationError::InvalidFlags { offset, operand, raw, valid_mask });
        }

        let contradictory = match opcode {
            OpCode::DefineProperty => raw & 0b110 == 0b110,
            // METHOD is exclusive, while GETTER|SETTER is the valid paired-accessor encoding used
            // by `execute_define_private_property`.
            OpCode::DefinePrivateProperty => raw & 0b001 != 0 && raw & 0b110 != 0,
            _ => false,
        };
        if contradictory {
            return Err(VerificationError::InvalidFlagCombination { offset, operand, raw });
        }
    }

    if opcode == OpCode::ThrowNewError {
        let raw = operands[0].as_unsigned();
        if raw > 1 {
            return Err(VerificationError::InvalidEnum { offset, operand: 0, raw });
        }
    }

    // The VM stores this capacity in a u8. Wider bytecode must not silently truncate it.
    if opcode == OpCode::NewObject {
        let raw = operands[1].as_unsigned();
        if raw > u8::MAX as usize {
            return Err(VerificationError::InvalidEnum { offset, operand: 1, raw });
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode_narrow(opcode: OpCode, operands: &[u8]) -> Vec<u8> {
        let mut bytes = vec![opcode as u8];
        bytes.extend_from_slice(operands);
        bytes
    }

    #[test]
    fn verifies_simple_program_without_using_typed_decoder() {
        let mut bytes = encode_narrow(OpCode::LoadImmediate, &[u8::MAX, 7]);
        bytes.extend(encode_narrow(OpCode::Ret, &[u8::MAX]));

        let verified = VerifiedBytecode::verify(&bytes, VerificationLimits::empty(1, 0)).unwrap();
        assert_eq!(verified.bytes(), bytes);
        assert_eq!(verified.instructions().len(), 2);
        assert_eq!(verified.instructions()[0].opcode, OpCode::LoadImmediate);
    }

    #[test]
    fn rejects_invalid_opcode_and_truncated_operands() {
        assert!(matches!(
            VerifiedBytecode::verify(&[u8::MAX], VerificationLimits::empty(0, 0)),
            Err(VerificationError::InvalidOpcode { offset: 0, raw: u8::MAX })
        ));
        assert!(matches!(
            VerifiedBytecode::verify(
                &[OpCode::Mov as u8, u8::MAX],
                VerificationLimits::empty(1, 0)
            ),
            Err(VerificationError::TruncatedOperands {
                offset: 0,
                required_end: 3,
                bytecode_len: 2,
            })
        ));
    }

    #[test]
    fn validates_prefix_padding_nested_prefix_and_canonical_width() {
        let nonzero_padding = [
            OpCode::LoadImmediate as u8,
            u8::MAX,
            0,
            OpCode::WidePrefix as u8,
            1,
            OpCode::Ret as u8,
            u8::MAX,
            u8::MAX,
        ];
        assert!(matches!(
            VerifiedBytecode::verify(&nonzero_padding, VerificationLimits::empty(1, 0)),
            Err(VerificationError::NonZeroPrefixPadding { .. })
        ));

        let nested = [OpCode::WidePrefix as u8, OpCode::ExtraWidePrefix as u8];
        assert!(matches!(
            VerifiedBytecode::verify(&nested, VerificationLimits::empty(0, 0)),
            Err(VerificationError::NestedWidthPrefix { .. })
        ));

        let unnecessary_wide = [
            OpCode::WidePrefix as u8,
            OpCode::Ret as u8,
            u8::MAX,
            u8::MAX,
        ];
        assert!(matches!(
            VerifiedBytecode::verify(&unnecessary_wide, VerificationLimits::empty(1, 0)),
            Err(VerificationError::NonCanonicalWidth { .. })
        ));
    }

    #[test]
    fn validates_registers_and_write_roles_before_formatting() {
        let invalid_slot = encode_narrow(OpCode::Ret, &[0]);
        assert!(matches!(
            VerifiedBytecode::verify(&invalid_slot, VerificationLimits::empty(0, 0)),
            Err(VerificationError::InvalidRegister { raw: 0, .. })
        ));

        let write_closure = encode_narrow(OpCode::LoadImmediate, &[CLOSURE_SLOT_INDEX as u8, 1]);
        assert!(matches!(
            VerifiedBytecode::verify(&write_closure, VerificationLimits::empty(0, 0)),
            Err(VerificationError::ReadOnlyRegisterWrite { .. })
        ));

        let out_of_bounds = encode_narrow(OpCode::Ret, &[u8::MAX - 1]);
        assert!(matches!(
            VerifiedBytecode::verify(&out_of_bounds, VerificationLimits::empty(1, 0)),
            Err(VerificationError::RegisterOutOfBounds { .. })
        ));
    }

    #[test]
    fn validates_constant_and_cache_kind_and_bounds() {
        let mut load_constant = encode_narrow(OpCode::LoadConstant, &[u8::MAX, 0]);
        load_constant.extend(encode_narrow(OpCode::Ret, &[u8::MAX]));
        assert!(matches!(
            VerifiedBytecode::verify(&load_constant, VerificationLimits::empty(1, 0)),
            Err(VerificationError::ConstantOutOfBounds { .. })
        ));

        let mut load_global = encode_narrow(OpCode::LoadGlobal, &[u8::MAX, 0, 0]);
        load_global.extend(encode_narrow(OpCode::Ret, &[u8::MAX]));
        let limits = VerificationLimits {
            constants: &[ConstantKind::String],
            caches: &[CacheKind::NamedProperty],
            ..VerificationLimits::empty(1, 0)
        };
        assert!(matches!(
            VerifiedBytecode::verify(&load_global, limits),
            Err(VerificationError::WrongCacheKind { expected: CacheKind::Global, .. })
        ));
    }

    #[test]
    fn branch_targets_must_be_instruction_starts_and_backedges_are_safepoints() {
        let mut bytes = encode_narrow(OpCode::LoadTrue, &[u8::MAX]);
        bytes.extend(encode_narrow(OpCode::JumpTrue, &[u8::MAX, 0]));
        bytes.extend(encode_narrow(OpCode::Ret, &[u8::MAX]));
        let jump_offset = 2;
        bytes[jump_offset + 2] = (0_isize - jump_offset as isize) as i8 as u8;

        let verified = VerifiedBytecode::verify(&bytes, VerificationLimits::empty(1, 0)).unwrap();
        let jump = &verified.instructions()[1];
        assert_eq!(jump.branch_target, Some(0));
        assert!(jump.effects.contains(EffectFlags::BACKEDGE));
        assert!(jump.effects.contains(EffectFlags::SAFEPOINT));

        bytes[jump_offset + 2] = u8::MAX;
        assert!(matches!(
            VerifiedBytecode::verify(&bytes, VerificationLimits::empty(1, 0)),
            Err(VerificationError::BranchTargetNotInstruction { .. })
        ));
    }

    #[test]
    fn validates_call_register_span_and_enum_immediates() {
        let mut call = encode_narrow(OpCode::Call, &[u8::MAX, u8::MAX, u8::MAX - 1, 2]);
        call.extend(encode_narrow(OpCode::Ret, &[u8::MAX]));
        assert!(matches!(
            VerifiedBytecode::verify(&call, VerificationLimits::empty(2, 0)),
            Err(VerificationError::InvalidRegisterRange { .. })
        ));

        let throw_error = encode_narrow(OpCode::ThrowNewError, &[2, 0]);
        let limits = VerificationLimits {
            constants: &[ConstantKind::String],
            ..VerificationLimits::empty(0, 0)
        };
        assert!(matches!(
            VerifiedBytecode::verify(&throw_error, limits),
            Err(VerificationError::InvalidEnum { .. })
        ));
    }

    #[test]
    fn every_constant_operand_has_an_exact_expected_type() {
        for &opcode in OpCode::ALL {
            for (operand, &kind) in opcode.metadata().operands.iter().enumerate() {
                assert_eq!(
                    expected_constant_kind(opcode, operand).is_some(),
                    kind == OperandType::ConstantIndex,
                    "{opcode:?} operand {operand}"
                );
            }
        }
    }

    #[test]
    fn rejects_wrong_typed_heap_constants_before_fallback() {
        let mut bytes = encode_narrow(OpCode::NewClosure, &[u8::MAX, 0]);
        bytes.extend(encode_narrow(OpCode::Ret, &[u8::MAX]));
        let limits = VerificationLimits {
            constants: &[ConstantKind::String],
            ..VerificationLimits::empty(1, 0)
        };
        assert!(matches!(
            VerifiedBytecode::verify(&bytes, limits),
            Err(VerificationError::WrongConstantKind {
                expected: ConstantKindName::BytecodeFunction,
                ..
            })
        ));
    }

    #[test]
    fn load_constant_rejects_internal_metadata_descriptors() {
        let mut bytes = encode_narrow(OpCode::LoadConstant, &[u8::MAX, 0]);
        bytes.extend(encode_narrow(OpCode::Ret, &[u8::MAX]));

        for internal in [
            ConstantKind::BytecodeFunction,
            ConstantKind::CompiledRegExp,
            ConstantKind::ScopeNames,
            ConstantKind::ClassNames { num_arguments: 0 },
            ConstantKind::JumpOffset(0),
        ] {
            let constants = [internal];
            let limits =
                VerificationLimits { constants: &constants, ..VerificationLimits::empty(1, 0) };
            assert!(matches!(
                VerifiedBytecode::verify(&bytes, limits),
                Err(VerificationError::WrongConstantKind {
                    expected: ConstantKindName::AnyValue,
                    ..
                })
            ));
        }

        for ordinary in [
            ConstantKind::AnyValue,
            ConstantKind::String,
            ConstantKind::PropertyKey,
        ] {
            let constants = [ordinary];
            let limits =
                VerificationLimits { constants: &constants, ..VerificationLimits::empty(1, 0) };
            assert!(VerifiedBytecode::verify(&bytes, limits).is_ok());
        }
    }

    #[test]
    fn validates_class_method_range_from_class_names_descriptor() {
        let mut bytes = encode_narrow(OpCode::NewClass, &[u8::MAX, 0, 1, u8::MAX - 1, u8::MAX - 2]);
        bytes.extend(encode_narrow(OpCode::Ret, &[u8::MAX]));
        let constants = [
            ConstantKind::ClassNames { num_arguments: 2 },
            ConstantKind::BytecodeFunction,
        ];
        let limits =
            VerificationLimits { constants: &constants, ..VerificationLimits::empty(3, 0) };
        assert!(matches!(
            VerifiedBytecode::verify(&bytes, limits),
            Err(VerificationError::InvalidRegisterRange { operand: 4, count: 2, .. })
        ));

        let limits =
            VerificationLimits { constants: &constants, ..VerificationLimits::empty(4, 0) };
        assert!(VerifiedBytecode::verify(&bytes, limits).is_ok());
    }

    #[test]
    fn generator_start_requires_a_local_register() {
        let mut bytes = encode_narrow(OpCode::GeneratorStart, &[RECEIVER_SLOT_INDEX as u8]);
        bytes.extend(encode_narrow(OpCode::Ret, &[u8::MAX]));
        assert!(matches!(
            VerifiedBytecode::verify(&bytes, VerificationLimits::empty(1, 0)),
            Err(VerificationError::RequiredLocalRegister { .. })
        ));
    }

    #[test]
    fn verifier_resources_are_bounded_and_empty_runtime_stubs_are_not_jit_input() {
        assert!(matches!(
            VerifiedBytecode::verify(&[], VerificationLimits::empty(0, 0)),
            Err(VerificationError::EmptyBytecode)
        ));

        let ret = encode_narrow(OpCode::Ret, &[u8::MAX]);
        let too_small =
            VerificationLimits { max_bytecode_bytes: 1, ..VerificationLimits::empty(1, 0) };
        assert!(matches!(
            VerifiedBytecode::verify(&ret, too_small),
            Err(VerificationError::BytecodeTooLarge { .. })
        ));

        let mut two_instructions = encode_narrow(OpCode::LoadTrue, &[u8::MAX]);
        two_instructions.extend(ret);
        let one_instruction =
            VerificationLimits { max_instructions: 1, ..VerificationLimits::empty(1, 0) };
        assert!(matches!(
            VerifiedBytecode::verify(&two_instructions, one_instruction),
            Err(VerificationError::TooManyInstructions { maximum: 1 })
        ));
    }

    #[test]
    fn rejects_contradictory_property_flags() {
        let mut define =
            encode_narrow(OpCode::DefineProperty, &[u8::MAX, u8::MAX - 1, u8::MAX - 2, 0b110]);
        define.extend(encode_narrow(OpCode::Ret, &[u8::MAX]));
        assert!(matches!(
            VerifiedBytecode::verify(&define, VerificationLimits::empty(3, 0)),
            Err(VerificationError::InvalidFlagCombination { .. })
        ));

        define[4] = 0b001;
        assert!(VerifiedBytecode::verify(&define, VerificationLimits::empty(3, 0)).is_ok());
    }

    #[test]
    fn private_accessor_pair_is_valid_but_method_accessor_mix_is_not() {
        let mut define_private = encode_narrow(
            OpCode::DefinePrivateProperty,
            &[u8::MAX, u8::MAX - 1, u8::MAX - 2, 0b110],
        );
        define_private.extend(encode_narrow(OpCode::Ret, &[u8::MAX]));
        assert!(VerifiedBytecode::verify(&define_private, VerificationLimits::empty(3, 0)).is_ok());

        for contradictory in [0b011, 0b101, 0b111] {
            define_private[4] = contradictory;
            assert!(matches!(
                VerifiedBytecode::verify(&define_private, VerificationLimits::empty(3, 0)),
                Err(VerificationError::InvalidFlagCombination { .. })
            ));
        }
    }

    #[test]
    fn new_object_property_capacity_does_not_truncate_to_u8() {
        let bytes = [
            OpCode::WidePrefix as u8,
            OpCode::NewObject as u8,
            u8::MAX,
            u8::MAX,
            0,
            1,
            OpCode::Ret as u8,
            u8::MAX,
        ];
        assert!(matches!(
            VerifiedBytecode::verify(&bytes, VerificationLimits::empty(1, 0)),
            Err(VerificationError::InvalidEnum { operand: 1, raw: 256, .. })
        ));
    }
}
