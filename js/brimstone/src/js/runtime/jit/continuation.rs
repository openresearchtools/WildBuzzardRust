//! Checked, contained side-exit continuation proof.
//!
//! This is deliberately not Brimstone VM integration. It proves that an exact verified bytecode
//! offset and the complete local-slot array can be handed off after native execution without
//! replaying an allocating instruction. Brimstone is a register VM, so there is no accumulator to
//! materialize. The only admitted continuation operation is the numeric fast path for `Neg`,
//! followed by `Ret`; it does not allocate in Brimstone's moving JavaScript heap, and every other
//! operation fails closed without executing it. Final value validation may reserve temporary host
//! bookkeeping and fails closed if that allocation cannot be made.

use crate::runtime::{
    JitContextScope, Value,
    bytecode::{WidthEnum, instruction::OpCode, verifier::DecodedOperand},
    jit::{
        abi::{
            ActivationCreateError, ActivationOutcome, ActivationOwner, ActivationResultError,
            JitSlot, JitSlotValidationError, ShadowFrameError, ShadowFrameOwner,
        },
        code_cache::{CodeMemoryError, LoadedPrototype},
        compiler::PreparedProgram,
        hotness::DeterministicInterruptBudget,
    },
};

/// Terminal result of the contained native-plus-continuation proof.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ContainedOutcome {
    NativeReturned(u64),
    ResumedReturned(u64),
    UnsupportedAt(usize),
    InterruptedAt(usize),
    AllocationFailedAt(usize),
    PoisonedAt(usize),
    InvalidActivation,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum ContainedRunError {
    InitialRealm,
    Frame(ShadowFrameError),
    Activation(ActivationCreateError),
    Code(CodeMemoryError),
    Result(ActivationResultError),
    Resume(ResumeError),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ResumeError {
    FrameSlotCountMismatch { actual: usize, expected: usize },
    InvalidBytecodeOffset(usize),
    InvalidLocalRegister(usize),
    InvalidReturnedValue(JitSlotValidationError),
    FellOffBytecode,
}

/// Borrowed continuation state with an exact verified instruction boundary.
pub(crate) struct ResumeState<'program, 'slots> {
    program: &'program PreparedProgram,
    slots: &'slots mut [JitSlot],
    next_instruction: usize,
}

impl<'program, 'slots> ResumeState<'program, 'slots> {
    pub(in crate::runtime::jit) fn new(
        program: &'program PreparedProgram,
        slots: &'slots mut [JitSlot],
        bytecode_offset: usize,
    ) -> Result<Self, ResumeError> {
        if slots.len() != program.num_locals() {
            return Err(ResumeError::FrameSlotCountMismatch {
                actual: slots.len(),
                expected: program.num_locals(),
            });
        }
        let next_instruction = program
            .instructions()
            .binary_search_by_key(&bytecode_offset, |instruction| instruction.offset)
            .map_err(|_| ResumeError::InvalidBytecodeOffset(bytecode_offset))?;
        Ok(Self { program, slots, next_instruction })
    }
}

/// Run generated code with a registered root frame, then enter the minimal checked continuation
/// only for a validated ordinary side exit. Terminal helper failures never resume.
pub(in crate::runtime::jit) fn run_contained(
    context: &mut JitContextScope<'_>,
    loaded: &LoadedPrototype,
    slots: &mut [JitSlot],
    budget: &mut DeterministicInterruptBudget,
) -> Result<ContainedOutcome, ContainedRunError> {
    let mut native_result = None;
    context
        .with_initial_realm(|context| {
            native_result = Some(run_native(context, loaded, slots, budget));
            Ok(())
        })
        .map_err(|_| ContainedRunError::InitialRealm)?;
    let native_result = native_result.expect("initial-realm closure ran synchronously")?;

    match native_result {
        ActivationOutcome::Returned(bits) => Ok(ContainedOutcome::NativeReturned(bits)),
        ActivationOutcome::SideExit(offset) => {
            let state = ResumeState::new(loaded.program(), slots, offset)
                .map_err(ContainedRunError::Resume)?;
            run_checked_continuation(context, state).map_err(ContainedRunError::Resume)
        }
        ActivationOutcome::Interrupted(offset) => Ok(ContainedOutcome::InterruptedAt(offset)),
        ActivationOutcome::AllocationFailed(offset) => {
            Ok(ContainedOutcome::AllocationFailedAt(offset))
        }
        ActivationOutcome::Poisoned(offset) => Ok(ContainedOutcome::PoisonedAt(offset)),
        ActivationOutcome::InvalidActivation => Ok(ContainedOutcome::InvalidActivation),
    }
}

fn run_native(
    context: &mut JitContextScope<'_>,
    loaded: &LoadedPrototype,
    slots: &mut [JitSlot],
    budget: &mut DeterministicInterruptBudget,
) -> Result<ActivationOutcome, ContainedRunError> {
    let mut frame =
        ShadowFrameOwner::new(slots, loaded.safepoints()).map_err(ContainedRunError::Frame)?;
    let mut activation =
        ActivationOwner::new(context, &mut frame, budget).map_err(ContainedRunError::Activation)?;
    // SAFETY: `LoadedPrototype` inseparably owns the exact generated entry, metadata, and prepared
    // program. The activation uses that same artifact's maps for this complete synchronous call.
    let status = unsafe { loaded.call(&mut activation) }.map_err(ContainedRunError::Code)?;
    activation
        .validate_result(status)
        .map_err(ContainedRunError::Result)
}

fn run_checked_continuation(
    context: &JitContextScope<'_>,
    mut state: ResumeState<'_, '_>,
) -> Result<ContainedOutcome, ResumeError> {
    loop {
        let Some(instruction) = state.program.instructions().get(state.next_instruction) else {
            return Err(ResumeError::FellOffBytecode);
        };
        match instruction.opcode {
            OpCode::Neg => {
                let dest = local_index(instruction.operands[0], instruction.width)
                    .ok_or(ResumeError::InvalidLocalRegister(instruction.offset))?;
                let source = local_index(instruction.operands[1], instruction.width)
                    .ok_or(ResumeError::InvalidLocalRegister(instruction.offset))?;
                let value = state.slots[source].value();
                let result = if value.is_smi() {
                    let smi = value.as_smi();
                    if smi != 0 {
                        smi.checked_neg().map(Value::raw_smi)
                    } else {
                        None
                    }
                    .unwrap_or_else(|| Value::number(-(f64::from(smi))))
                } else if value.is_double() {
                    Value::number(-value.as_double())
                } else {
                    return Ok(ContainedOutcome::UnsupportedAt(instruction.offset));
                };
                // SAFETY: Every arm above constructs a canonical number, and this operation does
                // not allocate in Brimstone's moving heap, so no moving collection can stale a
                // pointer between validation and this write. Return validation may later reserve
                // host bookkeeping, but it cannot move this heap.
                unsafe { state.slots[dest].write_trusted_value(result) };
                state.next_instruction += 1;
            }
            OpCode::Ret => {
                let source = local_index(instruction.operands[0], instruction.width)
                    .ok_or(ResumeError::InvalidLocalRegister(instruction.offset))?;
                let value = state.slots[source].value();
                JitSlot::try_from_value(context, value)
                    .map_err(ResumeError::InvalidReturnedValue)?;
                return Ok(ContainedOutcome::ResumedReturned(value.as_raw_bits()));
            }
            _ => return Ok(ContainedOutcome::UnsupportedAt(instruction.offset)),
        }
    }
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

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use super::*;
    use crate::runtime::{
        ContextBuilder,
        bytecode::{
            instruction::{
                extra_wide_prefix_index_to_opcode_index, wide_prefix_index_to_opcode_index,
            },
            verifier::{VerificationLimits, VerifiedBytecode},
        },
        gc::HandleScopeGuard,
        jit::{
            abi::{TestHelperBehavior, with_test_helper_behavior},
            code_cache::ExecutableCodeCache,
            compiler::{PreparedPrototype, compile_prototype},
        },
        string_value::StringValue,
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

    fn map_code(prepared: PreparedPrototype) -> ExecutableCodeCache {
        let mapped_len = ExecutableCodeCache::mapped_len_for(&prepared).unwrap();
        let mut cache = ExecutableCodeCache::new(mapped_len, 1).unwrap();
        cache.insert(1, prepared).unwrap();
        cache
    }

    fn invoke(
        owned: &mut crate::runtime::OwnedContext,
        cache: &mut ExecutableCodeCache,
        slots: &mut [JitSlot],
        budget: &mut DeterministicInterruptBudget,
    ) -> Result<ContainedOutcome, ContainedRunError> {
        let mut outcome = None;
        owned.with_jit_context(|context| {
            let loaded = cache.get(1).unwrap().unwrap();
            outcome = Some(run_contained(context, loaded, slots, budget));
            assert!(!context.has_registered_jit_frame());
        });
        outcome.unwrap()
    }

    fn unrooted_string_pair(
        context: &mut JitContextScope<'_>,
        first: &str,
        second: &str,
    ) -> (JitSlot, JitSlot) {
        let mut raw = context.raw();
        let guard = HandleScopeGuard::new(raw);
        let first = match raw.alloc_string(first) {
            Ok(string) => string,
            Err(_) => panic!("test string allocation failed"),
        };
        let second = match raw.alloc_string(second) {
            Ok(string) => string,
            Err(_) => panic!("test string allocation failed"),
        };
        let slots = (
            JitSlot::try_from_value(context, *first.as_value()).unwrap(),
            JitSlot::try_from_value(context, *second.as_value()).unwrap(),
        );
        drop(guard);
        slots
    }

    #[test]
    fn allocating_helper_maps_return_pc_and_resumes_without_replay() {
        let mut bytes = encode(OpCode::NewObject, &[local(2), 0]);
        bytes.extend(encode(OpCode::LoadImmediate, &[local(0), 5]));
        let resume_offset = bytes.len();
        bytes.extend(encode(OpCode::Neg, &[local(1), local(0)]));
        bytes.extend(encode(OpCode::Ret, &[local(1)]));
        let verified = VerifiedBytecode::verify(&bytes, VerificationLimits::empty(3, 0)).unwrap();
        let prepared = compile_prototype(&verified).unwrap();
        assert_eq!(prepared.safepoints().records().len(), 1);
        let record = prepared.safepoints().records()[0];
        assert!(record.native_return_offset > 0);
        assert!(record.native_return_offset <= prepared.safepoints().native_code_len());
        assert_eq!(prepared.safepoints().bytecode_len(), bytes.len() as u32);
        assert_eq!(record.bytecode_offset, 0);
        assert!(prepared.safepoints().live_slots().is_empty());

        let mut cache = map_code(prepared);
        let mut slots = vec![JitSlot::undefined(); 3];
        let mut owned = ContextBuilder::new().build().unwrap();
        let (mut budget, _) = DeterministicInterruptBudget::new(NonZeroU32::new(100).unwrap());
        let (outcome, observation) = with_test_helper_behavior(TestHelperBehavior::Normal, || {
            invoke(&mut owned, &mut cache, &mut slots, &mut budget).unwrap()
        });

        assert_eq!(outcome, ContainedOutcome::ResumedReturned(Value::raw_smi(-5).as_raw_bits()));
        assert_eq!(observation.calls, 1, "allocating bytecode must not replay");
        assert_eq!(slots[0].value().as_raw_bits(), Value::raw_smi(5).as_raw_bits());
        assert_eq!(slots[1].value().as_raw_bits(), Value::raw_smi(-5).as_raw_bits());
        assert!(slots[2].value().is_object());
        assert_eq!(resume_offset, 6);
    }

    #[test]
    fn forced_collection_updates_only_compiler_live_slots_and_rooted_result() {
        let mut bytes = encode(OpCode::NewObject, &[local(1), 0]);
        bytes.extend(encode(OpCode::Mov, &[local(3), local(0)]));
        bytes.extend(encode(OpCode::Ret, &[local(1)]));
        let verified = VerifiedBytecode::verify(&bytes, VerificationLimits::empty(4, 0)).unwrap();
        let prepared = compile_prototype(&verified).unwrap();
        assert_eq!(prepared.safepoints().live_slots(), &[0]);
        assert_eq!(prepared.safepoints().records()[0].result_slot, 1);

        let mut cache = map_code(prepared);
        let mut slots = vec![JitSlot::undefined(); 4];
        let mut owned = ContextBuilder::new().build().unwrap();
        #[cfg(feature = "gc_stress_test")]
        owned.enable_gc_stress_test();
        owned.with_jit_context(|context| {
            (slots[0], slots[2]) =
                unrooted_string_pair(context, "live native root", "dead native slot");
        });
        let live_before = slots[0].value().as_raw_bits();
        let dead_before = slots[2].value().as_raw_bits();
        let (mut budget, _) = DeterministicInterruptBudget::new(NonZeroU32::new(100).unwrap());
        let (outcome, observation) =
            with_test_helper_behavior(TestHelperBehavior::ForceCollectionAfterAllocation, || {
                invoke(&mut owned, &mut cache, &mut slots, &mut budget).unwrap()
            });

        assert_eq!(outcome, ContainedOutcome::NativeReturned(slots[1].value().as_raw_bits()));
        assert_ne!(
            slots[0].value().as_raw_bits(),
            live_before,
            "the published live pointer must move and update"
        );
        assert_eq!(
            slots[2].value().as_raw_bits(),
            dead_before,
            "dead slots must not be visited or rewritten"
        );
        assert_eq!(
            slots[3].value().as_raw_bits(),
            slots[0].value().as_raw_bits(),
            "execution must reload the post-GC slot value"
        );
        assert_ne!(observation.object_before, observation.object_after);
        assert_eq!(observation.object_after as u64, slots[1].value().as_raw_bits());
        let live_value = slots[0].value();
        assert!(live_value.is::<StringValue>());
        assert_eq!(live_value.as_string().len(), "live native root".len() as u32);
        assert!(slots[1].value().is_object());
    }

    #[test]
    fn helper_failures_are_terminal_and_never_continue() {
        let mut bytes = encode(OpCode::NewObject, &[local(0), 0]);
        bytes.extend(encode(OpCode::LoadImmediate, &[local(1), 9]));
        bytes.extend(encode(OpCode::Ret, &[local(1)]));
        let verified = VerifiedBytecode::verify(&bytes, VerificationLimits::empty(2, 0)).unwrap();
        for (behavior, expected) in [
            (TestHelperBehavior::AllocationFailure, ContainedOutcome::AllocationFailedAt(0)),
            (TestHelperBehavior::PanicBeforeAllocation, ContainedOutcome::PoisonedAt(0)),
        ] {
            let mut cache = map_code(compile_prototype(&verified).unwrap());
            let seed = JitSlot::undefined();
            let mut slots = vec![seed; 2];
            let mut owned = ContextBuilder::new().build().unwrap();
            let (mut budget, _) = DeterministicInterruptBudget::new(NonZeroU32::new(100).unwrap());
            let (outcome, observation) = with_test_helper_behavior(behavior, || {
                invoke(&mut owned, &mut cache, &mut slots, &mut budget).unwrap()
            });
            assert_eq!(outcome, expected);
            assert_eq!(observation.calls, 1);
            assert_eq!(slots, [seed, seed], "terminal failures must not continue bytecode");
        }

        let mut cache = map_code(compile_prototype(&verified).unwrap());
        let seed = JitSlot::undefined();
        let mut slots = vec![seed; 2];
        let mut owned = ContextBuilder::new().build().unwrap();
        let (mut budget, _) = DeterministicInterruptBudget::new(NonZeroU32::new(1).unwrap());
        let (outcome, observation) = with_test_helper_behavior(TestHelperBehavior::Normal, || {
            invoke(&mut owned, &mut cache, &mut slots, &mut budget).unwrap()
        });
        assert_eq!(outcome, ContainedOutcome::InterruptedAt(0));
        assert_eq!(observation.calls, 1);
        assert_eq!(observation.object_before, 0, "interrupt must poll before allocation");
        assert_eq!(slots, [seed, seed]);

        let mut excluded = encode(OpCode::NewObject, &[local(0), 1]);
        excluded.extend(encode(OpCode::Ret, &[local(0)]));
        let excluded_verified =
            VerifiedBytecode::verify(&excluded, VerificationLimits::empty(1, 0)).unwrap();
        let excluded_prepared = compile_prototype(&excluded_verified).unwrap();
        assert!(excluded_prepared.safepoints().records().is_empty());
        let mut excluded_cache = map_code(excluded_prepared);
        let mut excluded_slots = vec![seed];
        let (mut excluded_budget, _) =
            DeterministicInterruptBudget::new(NonZeroU32::new(100).unwrap());
        let (excluded_outcome, excluded_observation) =
            with_test_helper_behavior(TestHelperBehavior::Normal, || {
                invoke(&mut owned, &mut excluded_cache, &mut excluded_slots, &mut excluded_budget)
                    .unwrap()
            });
        assert_eq!(excluded_outcome, ContainedOutcome::UnsupportedAt(0));
        assert_eq!(excluded_observation.calls, 0);
        assert_eq!(excluded_slots, [seed]);
    }

    #[test]
    fn allocating_helper_supports_canonical_wide_and_extra_wide_destinations() {
        for (width, encoded_dest, num_locals, result_slot) in [
            (WidthEnum::Wide, -129, 129, 128),
            (WidthEnum::ExtraWide, -65_537, 65_537, 65_536),
        ] {
            let mut bytes = Vec::new();
            append_width_encoded(&mut bytes, OpCode::NewObject, &[encoded_dest, 0], width);
            append_width_encoded(&mut bytes, OpCode::Ret, &[encoded_dest], width);
            let verified =
                VerifiedBytecode::verify(&bytes, VerificationLimits::empty(num_locals, 0)).unwrap();
            let prepared = compile_prototype(&verified).unwrap();
            assert_eq!(prepared.safepoints().records()[0].result_slot as usize, result_slot);

            let mut cache = map_code(prepared);
            let mut slots = vec![JitSlot::undefined(); num_locals];
            let mut owned = ContextBuilder::new().build().unwrap();
            let (mut budget, _) = DeterministicInterruptBudget::new(NonZeroU32::new(100).unwrap());
            let outcome = invoke(&mut owned, &mut cache, &mut slots, &mut budget).unwrap();
            assert_eq!(
                outcome,
                ContainedOutcome::NativeReturned(slots[result_slot].value().as_raw_bits())
            );
            assert!(slots[result_slot].value().is_object());
        }
    }

    #[test]
    fn activation_unlinks_before_executable_cache_eviction() {
        let mut bytes = encode(OpCode::NewObject, &[local(0), 0]);
        bytes.extend(encode(OpCode::Ret, &[local(0)]));
        let verified = VerifiedBytecode::verify(&bytes, VerificationLimits::empty(1, 0)).unwrap();
        let prepared = compile_prototype(&verified).unwrap();
        let mapped_len = ExecutableCodeCache::mapped_len_for(&prepared).unwrap();
        let mut cache = ExecutableCodeCache::new(mapped_len, 1).unwrap();
        cache.insert(1, prepared).unwrap();

        let mut slots = vec![JitSlot::undefined()];
        let mut owned = ContextBuilder::new().build().unwrap();
        let (mut budget, _) = DeterministicInterruptBudget::new(NonZeroU32::new(100).unwrap());
        assert!(matches!(
            invoke(&mut owned, &mut cache, &mut slots, &mut budget).unwrap(),
            ContainedOutcome::NativeReturned(_)
        ));

        cache
            .insert(2, compile_prototype(&verified).unwrap())
            .unwrap();
        assert!(cache.get(1).unwrap().is_none());
        assert!(cache.get(2).unwrap().is_some());
    }

    #[test]
    fn resume_state_rejects_non_boundary_offsets_and_nonnumeric_negation() {
        let mut bytes = encode(OpCode::Neg, &[local(1), local(0)]);
        bytes.extend(encode(OpCode::Ret, &[local(1)]));
        let verified = VerifiedBytecode::verify(&bytes, VerificationLimits::empty(2, 0)).unwrap();
        let prepared = compile_prototype(&verified).unwrap();
        let seed = JitSlot::undefined();
        let mut slots = vec![JitSlot::null(), seed];
        let mut owned = ContextBuilder::new().build().unwrap();
        owned.with_jit_context(|context| {
            assert!(matches!(
                ResumeState::new(prepared.program(), &mut slots, 1),
                Err(ResumeError::InvalidBytecodeOffset(1))
            ));
            let state = ResumeState::new(prepared.program(), &mut slots, 0).unwrap();
            assert_eq!(
                run_checked_continuation(context, state).unwrap(),
                ContainedOutcome::UnsupportedAt(0)
            );
        });
        assert_eq!(slots[1], seed);
    }
}
