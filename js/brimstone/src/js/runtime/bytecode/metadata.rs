//! Machine-readable bytecode metadata used by the checked verifier and the contained baseline-JIT
//! prototype. This metadata describes conservative *may* effects: an instruction marked as
//! allocating or calling is a safepoint even when its common interpreter fast path does neither.

use super::{instruction::OpCode, operand::OperandType};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OperandAccess {
    None,
    Read,
    Write,
    ReadWrite,
    /// The register is the first element of a contiguous local-register range. The referenced
    /// unsigned operand contains the number of registers in the range.
    ReadRange {
        length_operand: u8,
    },
    /// `NewClass` method arguments form a local-register range whose length is recorded in the
    /// validated `ClassNames` constant descriptor.
    ReadClassMethods {
        class_names_operand: u8,
    },
}

impl OperandAccess {
    #[inline]
    #[allow(dead_code)]
    pub(crate) const fn may_read(self) -> bool {
        matches!(
            self,
            Self::Read | Self::ReadWrite | Self::ReadRange { .. } | Self::ReadClassMethods { .. }
        )
    }

    #[inline]
    pub(crate) const fn may_write(self) -> bool {
        matches!(self, Self::Write | Self::ReadWrite)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ControlFlow {
    Prefix,
    Fallthrough,
    Return,
    Throw,
    Jump,
    ConditionalJump,
    Suspend,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct EffectFlags(u16);

impl EffectFlags {
    pub(crate) const EMPTY: Self = Self(0);
    pub(crate) const CAN_THROW: Self = Self(1 << 0);
    pub(crate) const MAY_ALLOCATE: Self = Self(1 << 1);
    pub(crate) const MAY_CALL: Self = Self(1 << 2);
    pub(crate) const BACKEDGE: Self = Self(1 << 3);
    pub(crate) const SAFEPOINT: Self = Self(1 << 4);
    pub(crate) const SUSPENDS: Self = Self(1 << 5);

    #[inline]
    #[cfg_attr(not(any(feature = "baseline_jit", test)), allow(dead_code))]
    pub(crate) const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    #[inline]
    pub(crate) const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct OpCodeMetadata {
    pub(crate) operands: &'static [OperandType],
    pub(crate) operand_accesses: &'static [OperandAccess],
    pub(crate) control_flow: ControlFlow,
    pub(crate) effects: EffectFlags,
}

#[derive(Clone, Copy)]
enum OpClass {
    Prefix,
    Simple,
    Allocating,
    Runtime,
    Call,
    Return,
    Jump,
    ConditionalJump,
    TerminalThrow,
    Suspend,
}

impl OpCode {
    /// Return complete metadata for an opcode. The match deliberately has no wildcard: adding an
    /// opcode requires an explicit classification before the crate can compile.
    pub(crate) const fn metadata(self) -> OpCodeMetadata {
        use OpCode::*;

        let class = match self {
            WidePrefix | ExtraWidePrefix => OpClass::Prefix,

            Mov | LoadImmediate | LoadConstant | LoadUndefined | LoadNull | LoadEmpty
            | LoadTrue | LoadFalse | LogNot | TypeOf => OpClass::Simple,

            // Strict equality cannot invoke user code or throw a JavaScript exception, but the
            // slow path compares rope strings through `StringValue::equals`, which may flatten and
            // allocate. The VM publishes its PC before entering that path.
            StrictEqual | StrictNotEqual => OpClass::Allocating,

            Call | CallWithReceiver | CallVarargs | CallMaybeEval | CallMaybeEvalVarargs
            | Construct | ConstructVarargs | DefaultSuperCall => OpClass::Call,

            Ret => OpClass::Return,

            Jump | JumpConstant => OpClass::Jump,

            JumpTrue
            | JumpTrueConstant
            | JumpToBooleanTrue
            | JumpToBooleanTrueConstant
            | JumpFalse
            | JumpFalseConstant
            | JumpToBooleanFalse
            | JumpToBooleanFalseConstant
            | JumpNotUndefined
            | JumpNotUndefinedConstant
            | JumpNullish
            | JumpNullishConstant
            | JumpNotNullish
            | JumpNotNullishConstant => OpClass::ConditionalJump,

            Throw | Rethrow | ErrorConst | ThrowNewError => OpClass::TerminalThrow,

            GeneratorStart | Yield | Await => OpClass::Suspend,

            LoadGlobal
            | LoadGlobalOrUnresolved
            | StoreGlobal
            | LoadDynamic
            | LoadDynamicOrUnresolved
            | StoreDynamic
            | Add
            | Sub
            | Mul
            | Div
            | Rem
            | Exp
            | BitAnd
            | BitOr
            | BitXor
            | ShiftLeft
            | ShiftRightArithmetic
            | ShiftRightLogical
            | AddImm
            | SubImm
            | MulImm
            | DivImm
            | RemImm
            | BitAndImm
            | BitOrImm
            | BitXorImm
            | ShiftLeftImm
            | ShiftRightArithmeticImm
            | ShiftRightLogicalImm
            | LooseEqual
            | LooseNotEqual
            | LessThan
            | LessThanOrEqual
            | GreaterThan
            | GreaterThanOrEqual
            | Neg
            | Inc
            | Dec
            | BitNot
            | In
            | InstanceOf
            | ToNumber
            | ToNumeric
            | ToString
            | ToPropertyKey
            | ToObject
            | NewClosure
            | NewAsyncClosure
            | NewGenerator
            | NewAsyncGenerator
            | NewObject
            | NewArray
            | NewRegExp
            | NewMappedArguments
            | NewUnmappedArguments
            | NewClass
            | NewAccessor
            | NewPrivateSymbol
            | GetProperty
            | SetProperty
            | DefineProperty
            | GetNamedProperty
            | SetNamedProperty
            | DefineNamedProperty
            | GetSuperProperty
            | GetNamedSuperProperty
            | SetSuperProperty
            | DeleteProperty
            | DeleteBinding
            | GetPrivateProperty
            | SetPrivateProperty
            | DefinePrivateProperty
            | SetArrayProperty
            | SetPrototypeOf
            | CopyDataProperties
            | GetMethod
            | PushLexicalScope
            | PushFunctionScope
            | PushWithScope
            | PopScope
            | DupScope
            | LoadFromScope
            | StoreToScope
            | LoadFromModule
            | StoreToModule
            | RestParameter
            | GetSuperConstructor
            | CheckTdz
            | CheckThisInitialized
            | CheckSuperAlreadyCalled
            | CheckIteratorResultObject
            | NewForInIterator
            | ForInNext
            | GetIterator
            | GetAsyncIterator
            | IteratorNext
            | IteratorUnpackResult
            | IteratorClose
            | AsyncIteratorCloseStart
            | AsyncIteratorCloseFinish
            | NewPromise
            | ResolvePromise
            | RejectPromise
            | ImportMeta
            | DynamicImport => OpClass::Runtime,
        };

        let (control_flow, mut effects) = match class {
            OpClass::Prefix => (ControlFlow::Prefix, EffectFlags::EMPTY),
            OpClass::Simple => (ControlFlow::Fallthrough, EffectFlags::EMPTY),
            OpClass::Allocating => (
                ControlFlow::Fallthrough,
                EffectFlags::MAY_ALLOCATE.union(EffectFlags::SAFEPOINT),
            ),
            OpClass::Runtime => (
                ControlFlow::Fallthrough,
                EffectFlags::MAY_ALLOCATE
                    .union(EffectFlags::MAY_CALL)
                    .union(EffectFlags::SAFEPOINT),
            ),
            OpClass::Call => (
                ControlFlow::Fallthrough,
                EffectFlags::CAN_THROW
                    .union(EffectFlags::MAY_ALLOCATE)
                    .union(EffectFlags::MAY_CALL)
                    .union(EffectFlags::SAFEPOINT),
            ),
            OpClass::Return => (ControlFlow::Return, EffectFlags::EMPTY),
            OpClass::Jump => (ControlFlow::Jump, EffectFlags::EMPTY),
            OpClass::ConditionalJump => (ControlFlow::ConditionalJump, EffectFlags::EMPTY),
            OpClass::TerminalThrow => (
                ControlFlow::Throw,
                EffectFlags::CAN_THROW
                    .union(EffectFlags::MAY_ALLOCATE)
                    .union(EffectFlags::SAFEPOINT),
            ),
            OpClass::Suspend => (
                ControlFlow::Suspend,
                EffectFlags::MAY_ALLOCATE
                    .union(EffectFlags::MAY_CALL)
                    .union(EffectFlags::SAFEPOINT)
                    .union(EffectFlags::SUSPENDS),
            ),
        };

        if self.definition_can_throw() {
            effects = effects
                .union(EffectFlags::CAN_THROW)
                .union(EffectFlags::MAY_ALLOCATE)
                .union(EffectFlags::SAFEPOINT);
        }

        OpCodeMetadata {
            operands: self.operand_types(),
            operand_accesses: self.operand_accesses(),
            control_flow,
            effects,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_opcode_has_complete_metadata() {
        assert_eq!(OpCode::COUNT, 151);

        for (discriminant, &opcode) in OpCode::ALL.iter().enumerate() {
            assert_eq!(opcode as usize, discriminant);
            assert_eq!(OpCode::try_from_u8(discriminant as u8), Some(opcode));

            let metadata = opcode.metadata();
            assert_eq!(metadata.operands.len(), metadata.operand_accesses.len());

            for (&operand, &access) in metadata.operands.iter().zip(metadata.operand_accesses) {
                if operand == OperandType::Register {
                    assert_ne!(access, OperandAccess::None, "{opcode:?}");
                } else {
                    assert_eq!(access, OperandAccess::None, "{opcode:?}");
                }
            }

            if metadata.effects.contains(EffectFlags::MAY_ALLOCATE)
                || metadata.effects.contains(EffectFlags::MAY_CALL)
            {
                assert!(metadata.effects.contains(EffectFlags::SAFEPOINT), "{opcode:?}");
            }
        }

        for invalid in (OpCode::COUNT as u8)..=u8::MAX {
            assert_eq!(OpCode::try_from_u8(invalid), None);
        }
    }

    #[test]
    fn use_def_edge_cases_are_explicit() {
        assert_eq!(OpCode::Inc.metadata().operand_accesses, &[OperandAccess::ReadWrite]);
        assert_eq!(OpCode::CopyDataProperties.metadata().operand_accesses[0], OperandAccess::Read);
        assert_eq!(OpCode::GetIterator.metadata().operand_accesses[0], OperandAccess::Write);
        assert_eq!(OpCode::IteratorNext.metadata().operand_accesses[1], OperandAccess::Write);
        assert_eq!(
            OpCode::Call.metadata().operand_accesses[2],
            OperandAccess::ReadRange { length_operand: 3 }
        );
    }

    #[test]
    fn comparison_and_conversion_slow_paths_are_safepoints() {
        for opcode in [OpCode::StrictEqual, OpCode::StrictNotEqual] {
            let effects = opcode.metadata().effects;
            assert!(effects.contains(EffectFlags::MAY_ALLOCATE), "{opcode:?}");
            assert!(effects.contains(EffectFlags::SAFEPOINT), "{opcode:?}");
            assert!(!effects.contains(EffectFlags::MAY_CALL), "{opcode:?}");
            assert!(!effects.contains(EffectFlags::CAN_THROW), "{opcode:?}");
        }

        // These adjacent slow paths can perform coercion, invoke user code, flatten strings, or
        // allocate result values. Their existing Runtime classification must remain conservative.
        for opcode in [
            OpCode::LooseEqual,
            OpCode::LooseNotEqual,
            OpCode::LessThan,
            OpCode::LessThanOrEqual,
            OpCode::GreaterThan,
            OpCode::GreaterThanOrEqual,
            OpCode::ToNumber,
            OpCode::ToNumeric,
            OpCode::ToString,
            OpCode::ToPropertyKey,
            OpCode::ToObject,
        ] {
            let effects = opcode.metadata().effects;
            assert!(effects.contains(EffectFlags::MAY_ALLOCATE), "{opcode:?}");
            assert!(effects.contains(EffectFlags::MAY_CALL), "{opcode:?}");
            assert!(effects.contains(EffectFlags::SAFEPOINT), "{opcode:?}");
            assert!(effects.contains(EffectFlags::CAN_THROW), "{opcode:?}");
        }

        // Boolean coercion and typeof use only already-interned values and remain non-allocating.
        for opcode in [OpCode::LogNot, OpCode::TypeOf] {
            let effects = opcode.metadata().effects;
            assert!(!effects.contains(EffectFlags::MAY_ALLOCATE), "{opcode:?}");
            assert!(!effects.contains(EffectFlags::SAFEPOINT), "{opcode:?}");
        }
    }
}
