//! Rooted, contained baseline-JIT side-exit continuation.
//!
//! A VM-capable artifact is compiled from one exact rooted closure. Private fresh handles keep the
//! closure, bytecode function, scope, constants/caches, and realm live and updated by moving GC.
//! A checked non-reused token is stored in both that branded binding and the inseparable loaded
//! artifact, so code for identical bytecode cannot be rebound to another function.
//!
//! Generated code still has no admitted product dispatch path. The ordinary hot-call hook can
//! exercise one bounded native CFG only when its `cfg(test)` policy is enabled, over
//! guarded SMI arithmetic, local moves/constants, simple predicates, branches, loops, `NewObject`,
//! and `Ret`. Every taken nonpositive edge publishes its exact target and consumes stable shared
//! poll state inline, entering Rust only at an interrupt, quantum, hard-cap, or policy boundary.
//! On an admitted ordinary side exit, every validated
//! native slot is copied into Brimstone's handle stack. The native activation is then unlinked,
//! identity and immutable bytecode metadata are checked again, and the rooted values are
//! materialized into a real Brimstone VM frame. Existing dispatch, return, and exception-unwind
//! machinery continues from the exact verified prefix-start offset. Helper interruption,
//! allocation failure, and panic outcomes never resume. A bounded abstract-CFG proof controls the
//! VM continuation; every operation outside either exact proof remains fail-closed.

use std::{
    marker::PhantomData,
    mem::size_of,
    panic::{AssertUnwindSafe, catch_unwind, resume_unwind},
};

#[cfg(test)]
use crate::runtime::EvalResult;
#[cfg(test)]
use crate::runtime::bytecode::vm::with_test_jit_resume_collection;
use crate::runtime::{
    Handle, HeapPtr, JitContextScope, Value,
    bytecode::{
        WidthEnum,
        constant_table::ConstantTable,
        function::{BytecodeFunction, CacheArray, ClosureObject},
        instruction::OpCode,
        stack_frame::{FIRST_ARGUMENT_SLOT_INDEX, RECEIVER_SLOT_INDEX},
        verifier::{
            ConstantKind, DecodedOperand, VerificationError, VerificationLimits, VerifiedBytecode,
            VerifiedInstruction,
        },
        vm::{JitResumeOutcome, JitResumeSetupError, VM},
    },
    eval_result::EvalError,
    gc::IsHeapItem,
    jit::{
        abi::{
            ActivationCreateError, ActivationOutcome, ActivationOwner, ActivationResultError,
            JitSlot, JitSlotValidationError, RootedSlotSyncError, ShadowFrameError,
            ShadowFrameOwner,
        },
        code_cache::{CodeMemoryError, LoadedPrototype},
        compiler::{
            BaselineCompileError, PreparedProgram, PreparedPrototype, VmBindingId,
            allocate_vm_binding_id, compile_prototype,
        },
        hotness::DeterministicInterruptBudget,
    },
    realm::Realm,
    scope::Scope,
};

/// A completion value held in the higher-ranked JIT handle scope.
///
/// The invariant lifetime prevents the handle, or an outcome containing it, from escaping
/// `OwnedContext::with_jit_context`. There is deliberately no non-test raw-value accessor.
#[derive(Clone, Copy)]
pub(crate) struct RootedCompletion<'scope> {
    value: Handle<Value>,
    _brand: PhantomData<&'scope mut &'scope ()>,
}

impl std::fmt::Debug for RootedCompletion<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("RootedCompletion(..)")
    }
}

impl PartialEq for RootedCompletion<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.value.as_raw_bits() == other.value.as_raw_bits()
    }
}

impl Eq for RootedCompletion<'_> {}

impl<'scope> RootedCompletion<'scope> {
    fn new(value: Handle<Value>) -> Self {
        Self { value, _brand: PhantomData }
    }

    /// Copy the rooted value for immediate re-rooting in the caller's enclosing handle scope.
    /// No allocation or collection may occur before the returned value is rooted again.
    pub(in crate::runtime::jit) fn value_for_dispatch(self) -> Value {
        *self.value
    }

    #[cfg(test)]
    pub(crate) fn bits_for_test(
        self,
        context: &JitContextScope<'_>,
    ) -> Result<u64, JitSlotValidationError> {
        JitSlot::try_from_value(context, *self.value).map(|slot| slot.value().as_raw_bits())
    }
}

/// Terminal result of the contained native-plus-VM continuation gate.
///
/// Even terminal variants carry the scope-selected type lifetime, so a caller cannot selectively
/// unwrap an outcome and let a moving completion value escape through a generic return type.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ContainedOutcome<'scope> {
    NativeReturned(RootedCompletion<'scope>),
    VmReturned(RootedCompletion<'scope>),
    VmThrew(RootedCompletion<'scope>),
    VmInterruptedAt(usize),
    UnsupportedAt(usize),
    InterruptedAt(usize),
    AllocationFailedAt(usize),
    VmAllocationFailedAt(usize),
    PoisonedAt(usize),
    InvalidActivation,
}

#[derive(Debug)]
pub(crate) enum ContainedRunError {
    InitialRealm,
    Binding(VmBindingError),
    Frame(ShadowFrameError),
    Activation(ActivationCreateError),
    Code(CodeMemoryError),
    Result(ActivationResultError),
    Resume(VmResumeError),
    VmSetup(JitResumeSetupError),
    SlotSync(RootedSlotSyncError),
    CompletionValue(JitSlotValidationError),
}

/// Distinguish a clean failure before generated code can run from any failure after the hot-call
/// boundary has committed to native execution. Only `PreEntry` permits interpreter fallback.
#[derive(Debug)]
pub(crate) enum HotCallRunError {
    PreEntry(ContainedRunError),
    PostEntry(ContainedRunError),
}

#[derive(Debug)]
pub(crate) enum VmCompileError {
    Binding(VmBindingError),
    Verification(VerificationError),
    Compiler(BaselineCompileError),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum VmBindingError {
    ForeignClosure(JitSlotValidationError),
    InvalidBridgeRoot(JitSlotValidationError),
    ArtifactIsUnbound,
    ArtifactIdentityMismatch,
    ClosureFunctionChanged,
    ClosureScopeChanged,
    FunctionRealmChanged,
    ConstantTableChanged,
    CacheArrayChanged,
    RegisterCountChanged { actual: usize, expected: usize },
    CapturedSlotCountOverflow,
    ArgumentCountChanged { actual: usize, expected: usize },
    ConstantCountChanged { actual: usize, expected: usize },
    CacheCountChanged { actual: usize, expected: usize },
    ConstantJumpKindChanged { index: usize },
    ConstantJumpChanged { index: usize, actual: isize, expected: isize },
    ValueConstantUnsupported { index: usize },
    CacheArrayUnsupported { actual: usize },
    BytecodeChanged,
    SafepointBytecodeChanged,
    RuntimeFunctionUnsupported,
    ExceptionHandlersUnsupported,
    NonInitialRealmUnsupported,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum VmResumeError {
    InvalidBytecodeOffset(usize),
    SlotCountMismatch { actual: usize, expected: usize },
    UnsupportedAt(usize),
    NonLocalTailOperand { offset: usize, operand: usize },
    EmptyValueConsumed { offset: usize, operand: usize, local: usize },
    InternalValueConsumed { offset: usize, operand: usize, local: usize },
    NonNumericOperand { offset: usize, operand: usize, local: usize },
    NonBooleanCondition { offset: usize, local: usize },
    MissingReachableTerminal,
    AnalysisSizeOverflow,
    AnalysisTooLarge { bytes: usize, maximum: usize },
    AnalysisWorkLimitExceeded { maximum: usize },
    AnalysisAllocationFailed,
}

/// Unforgeable proof that one exact rooted binding and loaded program admit this VM resume.
///
/// Fields and construction are private to this module. The safe VM primitive accepts no raw
/// closure/program/offset tuple, so another crate-internal caller cannot bypass the exact-arity,
/// exact-artifact, captured-register, or already-proven type checks.
pub(crate) struct AdmittedVmResume<'scope, 'program> {
    closure: Handle<ClosureObject>,
    program: &'program PreparedProgram,
    offset: usize,
    _brand: PhantomData<&'scope mut &'scope ()>,
}

impl AdmittedVmResume<'_, '_> {
    pub(crate) fn closure(&self) -> Handle<ClosureObject> {
        self.closure
    }

    pub(crate) fn program(&self) -> &PreparedProgram {
        self.program
    }

    pub(crate) fn offset(&self) -> usize {
        self.offset
    }
}

/// Non-escaping rooted identity for one exact closure and its VM-owned execution metadata.
///
/// Every handle occupies a fresh private handle-stack cell. Callers cannot mutate one through an
/// alias to the input handle, and the higher-ranked JIT scope prevents the binding from escaping.
pub(crate) struct VmFunctionBinding<'scope> {
    id: VmBindingId,
    closure: Handle<ClosureObject>,
    function: Handle<BytecodeFunction>,
    scope: Handle<Scope>,
    realm: Handle<Realm>,
    constant_table: Option<Handle<ConstantTable>>,
    caches: Option<Handle<CacheArray>>,
    _brand: PhantomData<&'scope mut &'scope ()>,
}

impl<'scope> VmFunctionBinding<'scope> {
    fn new(
        context: &mut JitContextScope<'scope>,
        closure: Handle<ClosureObject>,
    ) -> Result<Self, VmCompileError> {
        let id = allocate_vm_binding_id().map_err(VmCompileError::Compiler)?;
        Self::new_with_id(context, closure, id)
    }

    fn new_with_id(
        context: &mut JitContextScope<'scope>,
        closure: Handle<ClosureObject>,
        id: VmBindingId,
    ) -> Result<Self, VmCompileError> {
        JitSlot::try_from_value(context, *closure.as_value())
            .map_err(VmBindingError::ForeignClosure)
            .map_err(VmCompileError::Binding)?;

        let closure_ptr = *closure;
        let function_ptr = closure_ptr.function_ptr();
        let binding = Self {
            id,
            // Each conversion allocates a distinct handle cell in JitContextScope's guard.
            closure: closure_ptr.to_handle(),
            function: function_ptr.to_handle(),
            scope: closure_ptr.scope_ptr().to_handle(),
            realm: function_ptr.realm_ptr().to_handle(),
            constant_table: function_ptr
                .constant_table_ptr()
                .map(|table| table.to_handle()),
            caches: function_ptr.caches_ptr().map(|caches| caches.to_handle()),
            _brand: PhantomData,
        };
        binding
            .validate_identity(context)
            .map_err(VmCompileError::Binding)?;
        Ok(binding)
    }

    fn validate_identity(&self, context: &JitContextScope<'_>) -> Result<(), VmBindingError> {
        JitSlot::try_from_value(context, *self.closure.as_value())
            .map_err(VmBindingError::ForeignClosure)?;

        let closure = *self.closure;
        if !closure.function_ptr().ptr_eq(&*self.function) {
            return Err(VmBindingError::ClosureFunctionChanged);
        }
        if !closure.scope_ptr().ptr_eq(&*self.scope) {
            return Err(VmBindingError::ClosureScopeChanged);
        }

        let function = *self.function;
        if !function.realm_ptr().ptr_eq(&*self.realm) {
            return Err(VmBindingError::FunctionRealmChanged);
        }
        if !optional_pointer_matches(function.constant_table_ptr(), self.constant_table) {
            return Err(VmBindingError::ConstantTableChanged);
        }
        if !optional_pointer_matches(function.caches_ptr(), self.caches) {
            return Err(VmBindingError::CacheArrayChanged);
        }
        if function.runtime_function_id().is_some() {
            return Err(VmBindingError::RuntimeFunctionUnsupported);
        }
        if function.exception_handlers_ptr().is_some() {
            // This slice admits only an uncaught terminal throw. A future gate must prevalidate
            // exact handler edges and stack shapes before catch/finally continuation is enabled.
            return Err(VmBindingError::ExceptionHandlersUnsupported);
        }
        if let Some(table) = function.constant_table_ptr() {
            for index in 0..table.len() {
                if table.is_value(index) {
                    return Err(VmBindingError::ValueConstantUnsupported { index });
                }
            }
        }
        if let Some(caches) = function.caches_ptr()
            && caches.len() != 0
        {
            return Err(VmBindingError::CacheArrayUnsupported { actual: caches.len() });
        }
        if !function
            .realm_ptr()
            .ptr_eq(&context.raw().initial_realm_ptr())
        {
            // Native helpers currently derive their realm from the initial VM frame. Cross-realm
            // JIT entry requires a separate exact-realm activation gate.
            return Err(VmBindingError::NonInitialRealmUnsupported);
        }
        Ok(())
    }

    /// Revalidate the rooted identity and the immutable program immediately before native entry
    /// and again immediately before publishing an interpreter frame.
    fn validate_loaded(
        &self,
        context: &JitContextScope<'_>,
        loaded: &LoadedPrototype,
        register_count: usize,
    ) -> Result<(), VmBindingError> {
        self.validate_identity(context)?;
        if !loaded.is_vm_bound() {
            return Err(VmBindingError::ArtifactIsUnbound);
        }
        if !loaded.is_bound_to_vm(self.id) {
            return Err(VmBindingError::ArtifactIdentityMismatch);
        }

        let function = *self.function;
        let program = loaded.program();
        let captured_slot_count = (function.num_registers() as usize)
            .checked_add(1)
            .and_then(|count| count.checked_add(function.num_parameters() as usize))
            .ok_or(VmBindingError::CapturedSlotCountOverflow)?;
        compare_count(captured_slot_count, register_count, |actual, expected| {
            VmBindingError::RegisterCountChanged { actual, expected }
        })?;
        compare_count(
            function.num_registers() as usize,
            program.num_locals(),
            |actual, expected| VmBindingError::RegisterCountChanged { actual, expected },
        )?;
        compare_count(
            function.num_parameters() as usize,
            program.num_arguments(),
            |actual, expected| VmBindingError::ArgumentCountChanged { actual, expected },
        )?;
        compare_count(
            function.constant_table_ptr().map_or(0, |table| table.len()),
            program.num_constants(),
            |actual, expected| VmBindingError::ConstantCountChanged { actual, expected },
        )?;
        compare_count(
            function.caches_ptr().map_or(0, |caches| caches.len()),
            program.num_caches(),
            |actual, expected| VmBindingError::CacheCountChanged { actual, expected },
        )?;
        if function.bytecode().len() != program.bytes().len()
            || function.bytecode() != program.bytes()
        {
            return Err(VmBindingError::BytecodeChanged);
        }
        if loaded.required_frame_slots() != captured_slot_count
            || loaded.safepoints().frame_slot_count() != captured_slot_count
            || loaded.safepoints().bytecode_len() as usize != function.bytecode().len()
        {
            return Err(VmBindingError::SafepointBytecodeChanged);
        }
        self.validate_constant_jumps(program.instructions())?;
        Ok(())
    }

    fn validate_constant_jumps<'a>(
        &self,
        instructions: impl IntoIterator<Item = &'a VerifiedInstruction>,
    ) -> Result<(), VmBindingError> {
        // The VM-bound facade derives descriptors from rooted raw entries and rejects every value
        // constant and nonempty cache array. The bounded emitter consumes ConstantIndex operands
        // only for the constant-backed branch opcodes it supports and embeds their verified
        // targets; the admitted VM tails contain no constants. Exact raw-kind and offset checks
        // therefore cover every constant which can affect native or resumed execution in this
        // gate.
        for (index, expected) in instructions
            .into_iter()
            .filter_map(|instruction| instruction.branch_constant)
        {
            let Some(table) = self.constant_table else {
                return Err(VmBindingError::ConstantTableChanged);
            };
            if table.is_value(index) {
                return Err(VmBindingError::ConstantJumpKindChanged { index });
            }
            let actual = table.get_constant_offset(index);
            if actual != expected {
                return Err(VmBindingError::ConstantJumpChanged { index, actual, expected });
            }
        }
        Ok(())
    }

    fn validate_bridge_roots(
        &self,
        context: &JitContextScope<'_>,
        loaded: &LoadedPrototype,
        roots: &[Handle<Value>],
    ) -> Result<(), VmBindingError> {
        self.validate_loaded(context, loaded, roots.len())?;
        for root in roots {
            JitSlot::try_from_value(context, **root).map_err(VmBindingError::InvalidBridgeRoot)?;
        }
        Ok(())
    }

    #[cfg(test)]
    fn identity_addresses(&self) -> IdentityAddresses {
        IdentityAddresses {
            closure: (*self.closure).as_ptr() as usize,
            function: (*self.function).as_ptr() as usize,
            bytecode: self.function.bytecode().as_ptr() as usize,
            scope: (*self.scope).as_ptr() as usize,
            realm: (*self.realm).as_ptr() as usize,
        }
    }
}

fn optional_pointer_matches<T: IsHeapItem>(
    actual: Option<HeapPtr<T>>,
    rooted: Option<Handle<T>>,
) -> bool {
    match (actual, rooted) {
        (None, None) => true,
        (Some(actual), Some(rooted)) => actual.ptr_eq(&*rooted),
        _ => false,
    }
}

fn compare_count(
    actual: usize,
    expected: usize,
    error: impl FnOnce(usize, usize) -> VmBindingError,
) -> Result<(), VmBindingError> {
    if actual == expected {
        Ok(())
    } else {
        Err(error(actual, expected))
    }
}

/// Verify and compile the exact bytecode stored in one rooted Brimstone function.
pub(in crate::runtime::jit) fn prepare_vm_prototype<'scope>(
    context: &mut JitContextScope<'scope>,
    closure: Handle<ClosureObject>,
) -> Result<(VmFunctionBinding<'scope>, PreparedPrototype), VmCompileError> {
    let binding = VmFunctionBinding::new(context, closure)?;
    prepare_vm_prototype_for_binding(binding)
}

pub(in crate::runtime::jit) fn prepare_vm_prototype_with_id<'scope>(
    context: &mut JitContextScope<'scope>,
    closure: Handle<ClosureObject>,
    id: VmBindingId,
) -> Result<(VmFunctionBinding<'scope>, PreparedPrototype), VmCompileError> {
    let binding = VmFunctionBinding::new_with_id(context, closure, id)?;
    prepare_vm_prototype_for_binding(binding)
}

pub(in crate::runtime::jit) fn bind_vm_function_with_id<'scope>(
    context: &mut JitContextScope<'scope>,
    closure: Handle<ClosureObject>,
    id: VmBindingId,
) -> Result<VmFunctionBinding<'scope>, VmCompileError> {
    VmFunctionBinding::new_with_id(context, closure, id)
}

fn prepare_vm_prototype_for_binding<'scope>(
    binding: VmFunctionBinding<'scope>,
) -> Result<(VmFunctionBinding<'scope>, PreparedPrototype), VmCompileError> {
    let function = *binding.function;
    let mut constants = Vec::new();
    let constant_count = function.constant_table_ptr().map_or(0, |table| table.len());
    constants
        .try_reserve_exact(constant_count)
        .map_err(|_| VmCompileError::Compiler(BaselineCompileError::AllocationFailed))?;
    if let Some(table) = function.constant_table_ptr() {
        for index in 0..table.len() {
            // `validate_identity` rejected value entries, so every admitted descriptor is derived
            // directly from the rooted table's exact raw metadata and bits.
            constants.push(ConstantKind::JumpOffset(table.get_constant_offset(index)));
        }
    }
    let limits = VerificationLimits {
        constants: &constants,
        ..VerificationLimits::empty(
            function.num_registers() as usize,
            function.num_parameters() as usize,
        )
    };

    let verified = VerifiedBytecode::verify(binding.function.bytecode(), limits)
        .map_err(VmCompileError::Verification)?;
    binding
        .validate_constant_jumps(verified.instructions())
        .map_err(VmCompileError::Binding)?;
    let prepared = compile_prototype(&verified)
        .map_err(VmCompileError::Compiler)?
        .bind_to_vm(binding.id)
        .map_err(VmCompileError::Compiler)?;
    Ok((binding, prepared))
}

/// Run generated code and, for an abstract-CFG-proven local continuation, continue in Brimstone's
/// actual VM. Terminal helper failures never resume.
pub(in crate::runtime::jit) fn run_vm_contained<'scope>(
    context: &mut JitContextScope<'scope>,
    loaded: &LoadedPrototype,
    binding: &VmFunctionBinding<'scope>,
    slots: &mut [JitSlot],
    budget: &mut DeterministicInterruptBudget,
) -> Result<ContainedOutcome<'scope>, ContainedRunError> {
    if slots.len() != loaded.required_frame_slots()
        && loaded.program().num_arguments() == 0
        && slots.len() == loaded.program().num_locals()
    {
        let mut captured = Vec::new();
        captured
            .try_reserve_exact(loaded.required_frame_slots())
            .map_err(|_| ContainedRunError::Resume(VmResumeError::AnalysisAllocationFailed))?;
        captured.extend_from_slice(slots);
        captured.push(JitSlot::undefined());
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            run_vm_contained(context, loaded, binding, &mut captured, budget)
        }));
        slots.copy_from_slice(&captured[..slots.len()]);
        return match outcome {
            Ok(outcome) => outcome,
            Err(payload) => resume_unwind(payload),
        };
    }

    binding
        .validate_loaded(context, loaded, slots.len())
        .map_err(ContainedRunError::Binding)?;

    let mut result = None;
    context
        .with_initial_realm(|context| {
            result = Some(run_native_then_vm(context, loaded, binding, slots, budget, None));
            Ok(())
        })
        .map_err(|_| ContainedRunError::InitialRealm)?;
    result.expect("initial-realm closure ran synchronously")
}

/// Execute one exact-arity ordinary VM call after proving the whole reachable function can either
/// finish natively or continue at any dynamic slow exit without replaying an effect.
pub(in crate::runtime::jit) fn run_vm_hot_call<'scope>(
    context: &mut JitContextScope<'scope>,
    vm: &mut VM,
    loaded: &LoadedPrototype,
    binding: &VmFunctionBinding<'scope>,
    slots: &mut [JitSlot],
    budget: &mut DeterministicInterruptBudget,
) -> Result<ContainedOutcome<'scope>, HotCallRunError> {
    binding
        .validate_loaded(context, loaded, slots.len())
        .map_err(ContainedRunError::Binding)
        .map_err(HotCallRunError::PreEntry)?;
    validate_hot_entry_policy(loaded.program(), slots)
        .map_err(ContainedRunError::Resume)
        .map_err(HotCallRunError::PreEntry)?;
    run_native_then_vm(context, loaded, binding, slots, budget, Some(vm))
        .map_err(HotCallRunError::PostEntry)
}

fn run_native_then_vm<'scope>(
    context: &mut JitContextScope<'scope>,
    loaded: &LoadedPrototype,
    binding: &VmFunctionBinding<'scope>,
    slots: &mut [JitSlot],
    budget: &mut DeterministicInterruptBudget,
    mut hot_vm: Option<&mut VM>,
) -> Result<ContainedOutcome<'scope>, ContainedRunError> {
    let mut frame =
        ShadowFrameOwner::new(slots, loaded.safepoints()).map_err(ContainedRunError::Frame)?;
    let mut activation =
        ActivationOwner::new(context, &mut frame, budget).map_err(ContainedRunError::Activation)?;
    // SAFETY: `LoadedPrototype` inseparably owns the exact generated entry, metadata, program, and
    // VM binding token. The activation uses that same artifact's maps for this synchronous call.
    let status = unsafe { loaded.call(&mut activation) }.map_err(ContainedRunError::Code)?;
    let native_outcome = activation
        .validate_result(status)
        .map_err(ContainedRunError::Result)?;

    match native_outcome {
        ActivationOutcome::Returned(bits) => {
            let root = activation
                .capture_validated_return_root(bits)
                .map_err(ContainedRunError::Result)?;
            drop(activation);
            rooted_completion(context, root).map(ContainedOutcome::NativeReturned)
        }
        ActivationOutcome::SideExit(offset) => {
            binding
                .validate_loaded(activation.context(), loaded, loaded.required_frame_slots())
                .map_err(ContainedRunError::Binding)?;
            let side_exit_slots = activation
                .validated_side_exit_slots()
                .map_err(ContainedRunError::Result)?;
            let admitted = match admit_vm_resume_after_binding_validation(
                binding,
                loaded,
                offset,
                side_exit_slots,
            ) {
                Ok(admitted) => admitted,
                Err(ContainedRunError::Resume(VmResumeError::UnsupportedAt(offset))) => {
                    drop(activation);
                    return Ok(ContainedOutcome::UnsupportedAt(offset));
                }
                Err(error) => {
                    drop(activation);
                    return Err(error);
                }
            };

            let bridge_roots = activation
                .capture_all_slot_roots()
                .map_err(ContainedRunError::Result)?;
            drop(activation);

            test_collect_before_vm_frame(context, binding, bridge_roots.handles());
            bridge_roots
                .sync_to_slots(slots)
                .map_err(ContainedRunError::SlotSync)?;
            binding
                .validate_bridge_roots(context, loaded, bridge_roots.handles())
                .map_err(ContainedRunError::Binding)?;

            let mut raw = context.raw();
            let completion = catch_unwind(AssertUnwindSafe(|| {
                let mut resume = || {
                    if let Some(vm) = hot_vm.as_deref_mut() {
                        vm.resume_from_jit_side_exit(&admitted, bridge_roots.handles(), budget)
                    } else {
                        raw.vm().resume_from_jit_side_exit(
                            &admitted,
                            bridge_roots.handles(),
                            budget,
                        )
                    }
                };
                #[cfg(test)]
                {
                    if let Some(before) =
                        test_prepare_after_vm_frame_collection(binding, bridge_roots.handles())
                    {
                        let (completion, ran) = with_test_jit_resume_collection(&mut resume);
                        test_finish_after_vm_frame_collection(
                            before,
                            ran,
                            binding,
                            bridge_roots.handles(),
                        );
                        return completion;
                    }
                }
                resume()
            }));
            // The VM frame, not the unlinked native slots, was traced by any dispatch-time GC.
            // Restore the rooted pre-VM snapshot (not interpreter register mutations) before these
            // roots disappear, including when a cleaned-up VM panic is about to resume unwinding.
            let sync_result = bridge_roots.sync_to_slots(slots);
            let completion = match completion {
                Ok(completion) => {
                    sync_result.map_err(ContainedRunError::SlotSync)?;
                    completion.map_err(ContainedRunError::VmSetup)?
                }
                Err(payload) => {
                    // A count mismatch already cleared every caller slot. Never expose a stale
                    // pointer suffix merely to replace the original panic payload.
                    let _ = sync_result;
                    resume_unwind(payload)
                }
            };
            vm_completion_to_outcome(context, completion, offset)
        }
        ActivationOutcome::Interrupted(offset) => Ok(ContainedOutcome::InterruptedAt(offset)),
        ActivationOutcome::AllocationFailed(offset) => {
            Ok(ContainedOutcome::AllocationFailedAt(offset))
        }
        ActivationOutcome::Poisoned(offset) => Ok(ContainedOutcome::PoisonedAt(offset)),
        ActivationOutcome::InvalidActivation => Ok(ContainedOutcome::InvalidActivation),
    }
}

fn admit_vm_resume<'scope, 'program>(
    context: &JitContextScope<'_>,
    binding: &VmFunctionBinding<'scope>,
    loaded: &'program LoadedPrototype,
    bytecode_offset: usize,
    slots: &[JitSlot],
) -> Result<AdmittedVmResume<'scope, 'program>, ContainedRunError> {
    binding
        .validate_loaded(context, loaded, slots.len())
        .map_err(ContainedRunError::Binding)?;
    admit_vm_resume_after_binding_validation(binding, loaded, bytecode_offset, slots)
}

fn admit_vm_resume_after_binding_validation<'scope, 'program>(
    binding: &VmFunctionBinding<'scope>,
    loaded: &'program LoadedPrototype,
    bytecode_offset: usize,
    slots: &[JitSlot],
) -> Result<AdmittedVmResume<'scope, 'program>, ContainedRunError> {
    validate_resume_policy(loaded.program(), bytecode_offset, slots)
        .map_err(ContainedRunError::Resume)?;
    Ok(AdmittedVmResume {
        closure: binding.closure,
        program: loaded.program(),
        offset: bytecode_offset,
        _brand: PhantomData,
    })
}

fn rooted_completion<'scope>(
    context: &JitContextScope<'scope>,
    value: Handle<Value>,
) -> Result<RootedCompletion<'scope>, ContainedRunError> {
    JitSlot::try_from_value(context, *value)
        .map(|_| RootedCompletion::new(value))
        .map_err(ContainedRunError::CompletionValue)
}

fn vm_completion_to_outcome<'scope>(
    context: &JitContextScope<'scope>,
    completion: JitResumeOutcome,
    _offset: usize,
) -> Result<ContainedOutcome<'scope>, ContainedRunError> {
    match completion {
        JitResumeOutcome::Completed(completion) => match completion {
            Ok(value) => rooted_completion(context, value).map(ContainedOutcome::VmReturned),
            Err(EvalError::Value(error)) => {
                rooted_completion(context, error).map(ContainedOutcome::VmThrew)
            }
            #[cfg(feature = "alloc_error")]
            Err(EvalError::Alloc(_)) => Ok(ContainedOutcome::VmAllocationFailedAt(_offset)),
        },
        JitResumeOutcome::InterruptedAt(offset) => Ok(ContainedOutcome::VmInterruptedAt(offset)),
    }
}

const MAX_VM_RESUME_ANALYSIS_BYTES: usize = 32 * 1024 * 1024;
const MAX_VM_RESUME_WORKLIST_STEPS: usize = 2_000_000;

const ABSTRACT_NUMBER: u8 = 1 << 0;
const ABSTRACT_BOOLEAN: u8 = 1 << 1;
const ABSTRACT_UNDEFINED: u8 = 1 << 2;
const ABSTRACT_NULL: u8 = 1 << 3;
const ABSTRACT_OTHER_JS: u8 = 1 << 4;
const ABSTRACT_EMPTY: u8 = 1 << 5;
const ABSTRACT_INTERNAL: u8 = 1 << 6;
const ABSTRACT_VALID_JS: u8 =
    ABSTRACT_NUMBER | ABSTRACT_BOOLEAN | ABSTRACT_UNDEFINED | ABSTRACT_NULL | ABSTRACT_OTHER_JS;

#[derive(Clone, Copy, Eq, PartialEq)]
enum ResumeAnalysisMode {
    ActualResume,
    NativeEntryPreflight,
}

fn validate_hot_entry_policy(
    program: &PreparedProgram,
    slots: &[JitSlot],
) -> Result<(), VmResumeError> {
    validate_resume_policy_with_mode(program, 0, slots, ResumeAnalysisMode::NativeEntryPreflight)
}

/// Prove one closed, bounded actual-VM continuation graph.
///
/// This is a monotone type analysis, not an evaluator: it never chooses a branch or computes a
/// JavaScript result. Both successors of every conditional are admitted and Brimstone's ordinary
/// VM executes all semantics. Every cycle in the finite verified graph necessarily contains a
/// taken nonpositive branch edge; the private resumed dispatch polls exactly those edges.
fn validate_resume_policy(
    program: &PreparedProgram,
    bytecode_offset: usize,
    slots: &[JitSlot],
) -> Result<(), VmResumeError> {
    validate_resume_policy_with_mode(
        program,
        bytecode_offset,
        slots,
        ResumeAnalysisMode::ActualResume,
    )
}

fn validate_resume_policy_with_mode(
    program: &PreparedProgram,
    bytecode_offset: usize,
    slots: &[JitSlot],
    mode: ResumeAnalysisMode,
) -> Result<(), VmResumeError> {
    let entry_index = program
        .instructions()
        .binary_search_by_key(&bytecode_offset, |instruction| instruction.offset)
        .map_err(|_| VmResumeError::InvalidBytecodeOffset(bytecode_offset))?;
    let captured_count = program
        .num_locals()
        .checked_add(1)
        .and_then(|count| count.checked_add(program.num_arguments()))
        .ok_or(VmResumeError::AnalysisSizeOverflow)?;
    if slots.len() != captured_count {
        return Err(VmResumeError::SlotCountMismatch {
            actual: slots.len(),
            expected: captured_count,
        });
    }

    let instruction_count = program.instructions().len();
    let local_count = program.num_locals();
    let abstract_cells = instruction_count
        .checked_mul(captured_count)
        .ok_or(VmResumeError::AnalysisSizeOverflow)?;
    let analysis_bytes = abstract_cells
        .checked_mul(size_of::<u8>())
        .and_then(|bytes| {
            instruction_count
                .checked_mul(3)
                .and_then(|flags| bytes.checked_add(flags))
        })
        .and_then(|bytes| {
            instruction_count
                .checked_mul(size_of::<usize>())
                .and_then(|queue_bytes| bytes.checked_add(queue_bytes))
        })
        .and_then(|bytes| bytes.checked_add(captured_count))
        .ok_or(VmResumeError::AnalysisSizeOverflow)?;
    if analysis_bytes > MAX_VM_RESUME_ANALYSIS_BYTES {
        return Err(VmResumeError::AnalysisTooLarge {
            bytes: analysis_bytes,
            maximum: MAX_VM_RESUME_ANALYSIS_BYTES,
        });
    }

    let mut states = try_zeroed_bytes(abstract_cells)?;
    let mut queued = try_zeroed_bytes(instruction_count)?;
    let mut reached = try_zeroed_bytes(instruction_count)?;
    let mut vm_reachable = try_zeroed_bytes(instruction_count)?;
    let mut outgoing = try_zeroed_bytes(captured_count)?;
    // At most one entry per instruction is queued at a time, so this fixed-capacity stack cannot
    // grow beyond the byte count admitted above.
    let mut worklist = Vec::new();
    worklist
        .try_reserve_exact(instruction_count)
        .map_err(|_| VmResumeError::AnalysisAllocationFailed)?;

    let entry_start = entry_index
        .checked_mul(captured_count)
        .ok_or(VmResumeError::AnalysisSizeOverflow)?;
    for (state, slot) in states[entry_start..entry_start + captured_count]
        .iter_mut()
        .zip(slots)
    {
        *state = classify_abstract_value(slot.value());
    }
    queued[entry_index] = 1;
    reached[entry_index] = 1;
    worklist.push(entry_index);

    let mut work_steps = 0_usize;
    let mut found_terminal = false;
    while let Some(index) = worklist.pop() {
        queued[index] = 0;
        work_steps = work_steps
            .checked_add(1)
            .ok_or(VmResumeError::AnalysisWorkLimitExceeded {
                maximum: MAX_VM_RESUME_WORKLIST_STEPS,
            })?;
        if work_steps > MAX_VM_RESUME_WORKLIST_STEPS {
            return Err(VmResumeError::AnalysisWorkLimitExceeded {
                maximum: MAX_VM_RESUME_WORKLIST_STEPS,
            });
        }

        let row_start = index
            .checked_mul(captured_count)
            .ok_or(VmResumeError::AnalysisSizeOverflow)?;
        outgoing.copy_from_slice(&states[row_start..row_start + captured_count]);
        let instruction = &program.instructions()[index];
        let may_resume_here = vm_reachable[index] != 0;
        transfer_resume_instruction(
            instruction,
            &mut outgoing,
            local_count,
            program.num_arguments(),
            mode == ResumeAnalysisMode::NativeEntryPreflight && !may_resume_here,
        )?;

        // Generated code can side-exit at dynamic guards and every taken backedge can hit its
        // independent native-residency cap. Once either is possible, all successors are potential
        // actual-VM continuation states. A native `NewObject` is therefore admitted only while
        // this bit is false; actual resume analysis never admits it at all. This preserves the
        // older VM loop invariant that every admitted cyclic operation is nonallocating.
        let successor_vm_reachable = mode == ResumeAnalysisMode::NativeEntryPreflight
            && (may_resume_here || native_instruction_may_resume_vm(instruction));

        if matches!(instruction.opcode, OpCode::Ret | OpCode::Throw) {
            found_terminal = true;
        }
        for successor in resume_successors(program, index, instruction)?
            .into_iter()
            .flatten()
        {
            let successor_start = successor
                .checked_mul(captured_count)
                .ok_or(VmResumeError::AnalysisSizeOverflow)?;
            let successor_state = &mut states[successor_start..successor_start + captured_count];
            let mut changed = reached[successor] == 0;
            reached[successor] = 1;
            if successor_vm_reachable && vm_reachable[successor] == 0 {
                vm_reachable[successor] = 1;
                changed = true;
            }
            for (target, source) in successor_state.iter_mut().zip(&outgoing) {
                let joined = *target | *source;
                changed |= joined != *target;
                *target = joined;
            }
            if changed && queued[successor] == 0 {
                queued[successor] = 1;
                worklist.push(successor);
            }
        }
    }

    found_terminal
        .then_some(())
        .ok_or(VmResumeError::MissingReachableTerminal)
}

fn try_zeroed_bytes(length: usize) -> Result<Vec<u8>, VmResumeError> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length)
        .map_err(|_| VmResumeError::AnalysisAllocationFailed)?;
    bytes.resize(length, 0);
    Ok(bytes)
}

fn classify_abstract_value(value: Value) -> u8 {
    if value.is_empty() {
        ABSTRACT_EMPTY
    } else if value.is_number() {
        ABSTRACT_NUMBER
    } else if value.is_bool() {
        ABSTRACT_BOOLEAN
    } else if value.is_undefined() {
        ABSTRACT_UNDEFINED
    } else if value.is_null() {
        ABSTRACT_NULL
    } else if value.is_object() || value.is_string() || value.is_symbol() || value.is_bigint() {
        ABSTRACT_OTHER_JS
    } else {
        // JitSlot validation proves that pointer values name an exact allocation start, but
        // Brimstone's heap also contains engine metadata such as bytecode functions, realms, and
        // scopes. Those are not ECMAScript Values. Preserve them only through Mov so a dead slot
        // can be overwritten; never let ordinary bytecode observe one.
        ABSTRACT_INTERNAL
    }
}

fn transfer_resume_instruction(
    instruction: &VerifiedInstruction,
    state: &mut [u8],
    num_locals: usize,
    num_arguments: usize,
    allow_native_new_object: bool,
) -> Result<(), VmResumeError> {
    match instruction.opcode {
        OpCode::Mov => {
            let dest = require_captured_local(instruction, 0, num_locals)?;
            let source = require_captured_register(instruction, 1, num_locals, num_arguments)?;
            // Moving Empty is allowed only as a transfer. Any later consumer rejects a state
            // containing Empty, so it must ultimately be overwritten or dead.
            state[dest] = state[source];
        }
        OpCode::LoadImmediate => {
            let dest = require_captured_local(instruction, 0, num_locals)?;
            state[dest] = ABSTRACT_NUMBER;
        }
        OpCode::LoadUndefined => {
            let dest = require_captured_local(instruction, 0, num_locals)?;
            state[dest] = ABSTRACT_UNDEFINED;
        }
        OpCode::LoadNull => {
            let dest = require_captured_local(instruction, 0, num_locals)?;
            state[dest] = ABSTRACT_NULL;
        }
        OpCode::LoadEmpty => {
            let dest = require_captured_local(instruction, 0, num_locals)?;
            state[dest] = ABSTRACT_EMPTY;
        }
        OpCode::LoadTrue | OpCode::LoadFalse => {
            let dest = require_captured_local(instruction, 0, num_locals)?;
            state[dest] = ABSTRACT_BOOLEAN;
        }
        OpCode::NewObject
            if allow_native_new_object && instruction.operands[1].as_unsigned() == 0 =>
        {
            // The native prefix owns this sole allocating operation. Full hot-call preflight must
            // model its exact result so every later dynamic native side exit is already proven,
            // while nonzero-capacity forms remain unsupported and cannot enter native code.
            let dest = require_captured_local(instruction, 0, num_locals)?;
            state[dest] = ABSTRACT_OTHER_JS;
        }
        OpCode::LogNot => {
            let dest = require_captured_local(instruction, 0, num_locals)?;
            require_valid_js_operand(instruction, 1, state, num_locals, num_arguments)?;
            state[dest] = ABSTRACT_BOOLEAN;
        }
        OpCode::TypeOf => {
            let dest = require_captured_local(instruction, 0, num_locals)?;
            require_valid_js_operand(instruction, 1, state, num_locals, num_arguments)?;
            state[dest] = ABSTRACT_OTHER_JS;
        }
        OpCode::Add
        | OpCode::Sub
        | OpCode::Mul
        | OpCode::Div
        | OpCode::Rem
        | OpCode::BitAnd
        | OpCode::BitOr
        | OpCode::BitXor
        | OpCode::ShiftLeft
        | OpCode::ShiftRightArithmetic
        | OpCode::ShiftRightLogical => {
            let dest = require_captured_local(instruction, 0, num_locals)?;
            require_numeric_operand(instruction, 1, state, num_locals, num_arguments)?;
            require_numeric_operand(instruction, 2, state, num_locals, num_arguments)?;
            state[dest] = ABSTRACT_NUMBER;
        }
        OpCode::AddImm
        | OpCode::SubImm
        | OpCode::MulImm
        | OpCode::DivImm
        | OpCode::RemImm
        | OpCode::BitAndImm
        | OpCode::BitOrImm
        | OpCode::BitXorImm
        | OpCode::ShiftLeftImm
        | OpCode::ShiftRightArithmeticImm
        | OpCode::ShiftRightLogicalImm => {
            let dest = require_captured_local(instruction, 0, num_locals)?;
            require_numeric_operand(instruction, 1, state, num_locals, num_arguments)?;
            state[dest] = ABSTRACT_NUMBER;
        }
        OpCode::Neg | OpCode::BitNot => {
            let dest = require_captured_local(instruction, 0, num_locals)?;
            require_numeric_operand(instruction, 1, state, num_locals, num_arguments)?;
            state[dest] = ABSTRACT_NUMBER;
        }
        OpCode::Inc | OpCode::Dec => {
            let dest = require_captured_local(instruction, 0, num_locals)?;
            require_numeric_slot(instruction, 0, state, dest)?;
            state[dest] = ABSTRACT_NUMBER;
        }
        OpCode::LooseEqual
        | OpCode::LooseNotEqual
        | OpCode::StrictEqual
        | OpCode::StrictNotEqual
        | OpCode::LessThan
        | OpCode::LessThanOrEqual
        | OpCode::GreaterThan
        | OpCode::GreaterThanOrEqual => {
            let dest = require_captured_local(instruction, 0, num_locals)?;
            require_numeric_operand(instruction, 1, state, num_locals, num_arguments)?;
            require_numeric_operand(instruction, 2, state, num_locals, num_arguments)?;
            state[dest] = ABSTRACT_BOOLEAN;
        }
        OpCode::Jump | OpCode::JumpConstant => {}
        OpCode::JumpTrue
        | OpCode::JumpTrueConstant
        | OpCode::JumpFalse
        | OpCode::JumpFalseConstant => {
            let local = require_valid_js_operand(instruction, 0, state, num_locals, num_arguments)?;
            let abstract_value = state[local];
            if abstract_value == 0 || abstract_value & !ABSTRACT_BOOLEAN != 0 {
                return Err(VmResumeError::NonBooleanCondition {
                    offset: instruction.offset,
                    local,
                });
            }
        }
        OpCode::JumpToBooleanTrue
        | OpCode::JumpToBooleanTrueConstant
        | OpCode::JumpToBooleanFalse
        | OpCode::JumpToBooleanFalseConstant
        | OpCode::JumpNotUndefined
        | OpCode::JumpNotUndefinedConstant
        | OpCode::JumpNullish
        | OpCode::JumpNullishConstant
        | OpCode::JumpNotNullish
        | OpCode::JumpNotNullishConstant => {
            require_valid_js_operand(instruction, 0, state, num_locals, num_arguments)?;
        }
        OpCode::Ret | OpCode::Throw => {
            require_valid_js_operand(instruction, 0, state, num_locals, num_arguments)?;
        }
        _ => return Err(VmResumeError::UnsupportedAt(instruction.offset)),
    }
    Ok(())
}

/// Conservatively identify an instruction after which generated execution may already have
/// entered the ordinary VM. Loads and the sole native allocating helper have no continuation
/// status; helper failures are terminal. Every other admitted instruction is treated as a
/// possible dynamic guard or backedge exit even when a narrower value proof could avoid it.
fn native_instruction_may_resume_vm(instruction: &VerifiedInstruction) -> bool {
    !matches!(
        instruction.opcode,
        OpCode::LoadImmediate
            | OpCode::LoadUndefined
            | OpCode::LoadNull
            | OpCode::LoadEmpty
            | OpCode::LoadTrue
            | OpCode::LoadFalse
            | OpCode::NewObject
    )
}

fn require_valid_js_operand(
    instruction: &VerifiedInstruction,
    operand_index: usize,
    state: &[u8],
    num_locals: usize,
    num_arguments: usize,
) -> Result<usize, VmResumeError> {
    let local = require_captured_register(instruction, operand_index, num_locals, num_arguments)?;
    let abstract_value = state[local];
    if abstract_value & ABSTRACT_EMPTY != 0 {
        return Err(VmResumeError::EmptyValueConsumed {
            offset: instruction.offset,
            operand: operand_index,
            local,
        });
    }
    if abstract_value & ABSTRACT_INTERNAL != 0 {
        return Err(VmResumeError::InternalValueConsumed {
            offset: instruction.offset,
            operand: operand_index,
            local,
        });
    }
    if abstract_value & !ABSTRACT_VALID_JS != 0 {
        return Err(VmResumeError::UnsupportedAt(instruction.offset));
    }
    if abstract_value == 0 {
        return Err(VmResumeError::UnsupportedAt(instruction.offset));
    }
    Ok(local)
}

fn require_numeric_operand(
    instruction: &VerifiedInstruction,
    operand_index: usize,
    state: &[u8],
    num_locals: usize,
    num_arguments: usize,
) -> Result<usize, VmResumeError> {
    let local =
        require_valid_js_operand(instruction, operand_index, state, num_locals, num_arguments)?;
    require_numeric_slot(instruction, operand_index, state, local)?;
    Ok(local)
}

fn require_numeric_slot(
    instruction: &VerifiedInstruction,
    operand_index: usize,
    state: &[u8],
    local: usize,
) -> Result<(), VmResumeError> {
    let abstract_value = state[local];
    if abstract_value & !ABSTRACT_NUMBER != 0 {
        return Err(VmResumeError::NonNumericOperand {
            offset: instruction.offset,
            operand: operand_index,
            local,
        });
    }
    Ok(())
}

fn resume_successors(
    program: &PreparedProgram,
    index: usize,
    instruction: &VerifiedInstruction,
) -> Result<[Option<usize>; 2], VmResumeError> {
    let fallthrough = || {
        program
            .instructions()
            .get(index + 1)
            .map(|_| index + 1)
            .ok_or(VmResumeError::UnsupportedAt(instruction.offset))
    };
    let target = || {
        let offset = instruction
            .branch_target
            .ok_or(VmResumeError::UnsupportedAt(instruction.offset))?;
        program
            .instructions()
            .binary_search_by_key(&offset, |candidate| candidate.offset)
            .map_err(|_| VmResumeError::InvalidBytecodeOffset(offset))
    };

    match instruction.opcode {
        OpCode::Ret | OpCode::Throw => Ok([None, None]),
        OpCode::Jump | OpCode::JumpConstant => Ok([Some(target()?), None]),
        OpCode::JumpTrue
        | OpCode::JumpTrueConstant
        | OpCode::JumpToBooleanTrue
        | OpCode::JumpToBooleanTrueConstant
        | OpCode::JumpFalse
        | OpCode::JumpFalseConstant
        | OpCode::JumpToBooleanFalse
        | OpCode::JumpToBooleanFalseConstant
        | OpCode::JumpNotUndefined
        | OpCode::JumpNotUndefinedConstant
        | OpCode::JumpNullish
        | OpCode::JumpNullishConstant
        | OpCode::JumpNotNullish
        | OpCode::JumpNotNullishConstant => Ok([Some(target()?), Some(fallthrough()?)]),
        _ => Ok([Some(fallthrough()?), None]),
    }
}

fn require_captured_local(
    instruction: &VerifiedInstruction,
    operand_index: usize,
    num_locals: usize,
) -> Result<usize, VmResumeError> {
    let operand = instruction.operands[operand_index];
    captured_local_index(operand, instruction.width, num_locals).ok_or(
        VmResumeError::NonLocalTailOperand { offset: instruction.offset, operand: operand_index },
    )
}

fn require_captured_register(
    instruction: &VerifiedInstruction,
    operand_index: usize,
    num_locals: usize,
    num_arguments: usize,
) -> Result<usize, VmResumeError> {
    let operand = instruction.operands[operand_index];
    if let Some(local) = captured_local_index(operand, instruction.width, num_locals) {
        return Ok(local);
    }
    let raw = usize::try_from(operand.as_signed(instruction.width)).ok();
    let slot = if raw == Some(RECEIVER_SLOT_INDEX) {
        num_locals.checked_add(0)
    } else {
        let argument = raw.and_then(|raw| raw.checked_sub(FIRST_ARGUMENT_SLOT_INDEX));
        argument
            .filter(|argument| *argument < num_arguments)
            .and_then(|argument| {
                num_locals
                    .checked_add(1)
                    .and_then(|first_argument| first_argument.checked_add(argument))
            })
    };
    slot.ok_or(VmResumeError::NonLocalTailOperand {
        offset: instruction.offset,
        operand: operand_index,
    })
}

fn captured_local_index(
    operand: DecodedOperand,
    width: WidthEnum,
    num_locals: usize,
) -> Option<usize> {
    let raw = operand.as_signed(width);
    if raw >= 0 {
        return None;
    }
    let index = (-1_isize)
        .checked_sub(raw)
        .and_then(|value| usize::try_from(value).ok())?;
    (index < num_locals).then_some(index)
}

/// Native-only probe retained for executable-cache tests. It never interprets a detached copied
/// program; every side exit is terminal and fail-closed.
#[cfg(test)]
pub(in crate::runtime::jit) fn run_unbound_native_for_test<'scope>(
    context: &mut JitContextScope<'scope>,
    loaded: &LoadedPrototype,
    slots: &mut [JitSlot],
    budget: &mut DeterministicInterruptBudget,
) -> Result<ContainedOutcome<'scope>, ContainedRunError> {
    if slots.len() != loaded.required_frame_slots()
        && loaded.program().num_arguments() == 0
        && slots.len() == loaded.program().num_locals()
    {
        let mut captured = slots.to_vec();
        captured.push(JitSlot::undefined());
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            run_unbound_native_for_test(context, loaded, &mut captured, budget)
        }));
        slots.copy_from_slice(&captured[..slots.len()]);
        return match outcome {
            Ok(outcome) => outcome,
            Err(payload) => resume_unwind(payload),
        };
    }
    let mut result = None;
    context
        .with_initial_realm(|context| {
            result = Some(run_unbound_native_inner(context, loaded, slots, budget));
            Ok(())
        })
        .map_err(|_| ContainedRunError::InitialRealm)?;
    result.expect("initial-realm closure ran synchronously")
}

#[cfg(test)]
fn run_unbound_native_inner<'scope>(
    context: &mut JitContextScope<'scope>,
    loaded: &LoadedPrototype,
    slots: &mut [JitSlot],
    budget: &mut DeterministicInterruptBudget,
) -> Result<ContainedOutcome<'scope>, ContainedRunError> {
    let mut frame =
        ShadowFrameOwner::new(slots, loaded.safepoints()).map_err(ContainedRunError::Frame)?;
    let mut activation =
        ActivationOwner::new(context, &mut frame, budget).map_err(ContainedRunError::Activation)?;
    // SAFETY: Test-only native probe uses this loaded artifact's exact maps.
    let status = unsafe { loaded.call(&mut activation) }.map_err(ContainedRunError::Code)?;
    let outcome = activation
        .validate_result(status)
        .map_err(ContainedRunError::Result)?;
    match outcome {
        ActivationOutcome::Returned(bits) => {
            let root = activation
                .capture_validated_return_root(bits)
                .map_err(ContainedRunError::Result)?;
            drop(activation);
            rooted_completion(context, root).map(ContainedOutcome::NativeReturned)
        }
        ActivationOutcome::SideExit(offset) => Ok(ContainedOutcome::UnsupportedAt(offset)),
        ActivationOutcome::Interrupted(offset) => Ok(ContainedOutcome::InterruptedAt(offset)),
        ActivationOutcome::AllocationFailed(offset) => {
            Ok(ContainedOutcome::AllocationFailedAt(offset))
        }
        ActivationOutcome::Poisoned(offset) => Ok(ContainedOutcome::PoisonedAt(offset)),
        ActivationOutcome::InvalidActivation => Ok(ContainedOutcome::InvalidActivation),
    }
}

#[cfg(not(test))]
fn test_collect_before_vm_frame(
    _: &mut JitContextScope<'_>,
    _: &VmFunctionBinding<'_>,
    _: &[Handle<Value>],
) {
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct TestHandoffGcBehavior {
    pub(crate) before_vm_frame: bool,
    pub(crate) after_vm_frame: bool,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct IdentityAddresses {
    closure: usize,
    function: usize,
    bytecode: usize,
    scope: usize,
    realm: usize,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TestHandoffPointObservation {
    identities_before: IdentityAddresses,
    identities_after: IdentityAddresses,
    first_root_before: Option<u64>,
    first_root_after: Option<u64>,
}

#[cfg(test)]
#[derive(Clone, Copy)]
struct TestHandoffBeforeSnapshot {
    identities: IdentityAddresses,
    first_root: Option<u64>,
}

#[cfg(test)]
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct TestHandoffGcObservation {
    pub(crate) before_vm_frame: Option<TestHandoffPointObservation>,
    pub(crate) after_vm_frame: Option<TestHandoffPointObservation>,
}

#[cfg(test)]
#[derive(Default)]
struct TestHandoffGcState {
    active: bool,
    behavior: TestHandoffGcBehavior,
    observation: TestHandoffGcObservation,
}

#[cfg(test)]
std::thread_local! {
    static TEST_HANDOFF_GC_STATE: std::cell::RefCell<TestHandoffGcState> =
        std::cell::RefCell::new(TestHandoffGcState::default());
}

#[cfg(test)]
pub(crate) fn with_test_handoff_collections<R>(
    behavior: TestHandoffGcBehavior,
    f: impl FnOnce() -> R,
) -> (R, TestHandoffGcObservation) {
    TEST_HANDOFF_GC_STATE.with(|state| {
        let mut state = state.borrow_mut();
        assert!(!state.active, "nested handoff collection observation");
        *state = TestHandoffGcState {
            active: true,
            behavior,
            observation: TestHandoffGcObservation::default(),
        };
    });

    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            TEST_HANDOFF_GC_STATE.with(|state| *state.borrow_mut() = TestHandoffGcState::default());
        }
    }
    let reset = Reset;
    let result = f();
    let observation = TEST_HANDOFF_GC_STATE.with(|state| state.borrow().observation.clone());
    drop(reset);
    (result, observation)
}

#[cfg(test)]
fn test_collect_before_vm_frame(
    context: &mut JitContextScope<'_>,
    binding: &VmFunctionBinding<'_>,
    roots: &[Handle<Value>],
) {
    let enabled = TEST_HANDOFF_GC_STATE.with(|state| {
        let state = state.borrow();
        state.active && state.behavior.before_vm_frame
    });
    if enabled {
        let observation = force_test_collection(context.raw(), binding, roots);
        TEST_HANDOFF_GC_STATE.with(|state| {
            state.borrow_mut().observation.before_vm_frame = Some(observation);
        });
    }
}

#[cfg(test)]
fn test_prepare_after_vm_frame_collection(
    binding: &VmFunctionBinding<'_>,
    roots: &[Handle<Value>],
) -> Option<TestHandoffBeforeSnapshot> {
    let enabled = TEST_HANDOFF_GC_STATE.with(|state| {
        let state = state.borrow();
        state.active && state.behavior.after_vm_frame
    });
    enabled.then(|| TestHandoffBeforeSnapshot {
        identities: binding.identity_addresses(),
        first_root: roots.first().map(|root| (**root).as_raw_bits()),
    })
}

#[cfg(test)]
fn test_finish_after_vm_frame_collection(
    before: TestHandoffBeforeSnapshot,
    ran: bool,
    binding: &VmFunctionBinding<'_>,
    roots: &[Handle<Value>],
) {
    assert!(ran, "configured post-publication collection hook did not run");
    let observation = TestHandoffPointObservation {
        identities_before: before.identities,
        identities_after: binding.identity_addresses(),
        first_root_before: before.first_root,
        first_root_after: roots.first().map(|root| (**root).as_raw_bits()),
    };
    TEST_HANDOFF_GC_STATE.with(|state| {
        state.borrow_mut().observation.after_vm_frame = Some(observation);
    });
}

#[cfg(test)]
fn force_test_collection(
    raw: crate::runtime::Context,
    binding: &VmFunctionBinding<'_>,
    roots: &[Handle<Value>],
) -> TestHandoffPointObservation {
    use crate::runtime::gc::{GcType, Heap};

    let identities_before = binding.identity_addresses();
    let first_root_before = roots.first().map(|root| (**root).as_raw_bits());
    Heap::run_gc(raw, GcType::Normal);
    TestHandoffPointObservation {
        identities_before,
        identities_after: binding.identity_addresses(),
        first_root_before,
        first_root_after: roots.first().map(|root| (**root).as_raw_bits()),
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use super::*;
    #[cfg(feature = "alloc_error")]
    use crate::runtime::bytecode::vm::with_test_jit_resume_allocation_failure;
    use crate::runtime::bytecode::vm::{
        with_test_jit_resume_dispatch_panic, with_test_jit_resume_inner_dispatch_panic,
        with_test_jit_resume_interrupt_policy_failure,
    };
    use crate::runtime::{
        ContextBuilder, OwnedContext,
        alloc_error::AllocResult,
        bytecode::{
            function::{BytecodeFunction, ClosureObject},
            instruction::{
                extra_wide_prefix_index_to_opcode_index, wide_prefix_index_to_opcode_index,
            },
            stack_frame::{FIRST_ARGUMENT_SLOT_INDEX, NUM_STACK_SLOTS},
        },
        gc::HandleScopeGuard,
        jit::{
            abi::{
                TestBackedgePollBehavior, TestHelperBehavior, with_test_backedge_poll_behavior,
                with_test_helper_behavior,
            },
            code_cache::ExecutableCodeCache,
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

    fn expect_eval_ok<T>(result: EvalResult<T>) -> T {
        match result {
            Ok(value) => value,
            Err(_) => panic!("unexpected evaluation failure"),
        }
    }

    fn expect_alloc_ok<T>(result: AllocResult<T>) -> T {
        match result {
            Ok(value) => value,
            Err(_) => panic!("unexpected allocation failure"),
        }
    }

    fn expect_vm_returned(context: &JitContextScope<'_>, outcome: ContainedOutcome<'_>) -> u64 {
        match outcome {
            ContainedOutcome::VmReturned(value) => value.bits_for_test(context).unwrap(),
            other => panic!("expected VM return, got {other:?}"),
        }
    }

    fn expect_native_returned(context: &JitContextScope<'_>, outcome: ContainedOutcome<'_>) -> u64 {
        match outcome {
            ContainedOutcome::NativeReturned(value) => value.bits_for_test(context).unwrap(),
            other => panic!("expected native return, got {other:?}"),
        }
    }

    fn run_vm_only_returned(
        context: &mut JitContextScope<'_>,
        binding: &VmFunctionBinding<'_>,
        loaded: &LoadedPrototype,
        slots: &[JitSlot],
    ) -> u64 {
        let mut captured = slots.to_vec();
        if captured.len() == loaded.program().num_locals() && loaded.program().num_arguments() == 0
        {
            captured.push(JitSlot::undefined());
        }
        let admitted = admit_vm_resume(context, binding, loaded, 0, &captured).unwrap();
        let raw = context.raw();
        let roots: Vec<Handle<Value>> = captured
            .iter()
            .map(|slot| slot.value().to_handle(raw))
            .collect();
        let (mut budget, _) = DeterministicInterruptBudget::new(NonZeroU32::new(10_000).unwrap());
        let mut completion = None;
        expect_eval_ok(context.with_initial_realm(|context| {
            let mut raw = context.raw();
            completion = Some(
                raw.vm()
                    .resume_from_jit_side_exit(&admitted, &roots, &mut budget),
            );
            Ok(())
        }));
        match completion.unwrap().unwrap() {
            JitResumeOutcome::Completed(Ok(value)) => value.as_raw_bits(),
            _ => panic!("expected direct VM return"),
        }
    }

    fn expect_vm_threw(context: &JitContextScope<'_>, outcome: ContainedOutcome<'_>) -> u64 {
        match outcome {
            ContainedOutcome::VmThrew(value) => value.bits_for_test(context).unwrap(),
            other => panic!("expected VM throw, got {other:?}"),
        }
    }

    fn append_width_encoded(
        bytes: &mut Vec<u8>,
        opcode: OpCode,
        operands: &[i32],
        width: crate::runtime::bytecode::WidthEnum,
    ) {
        use crate::runtime::bytecode::WidthEnum;

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

    fn make_test_closure(
        context: &mut JitContextScope<'_>,
        bytes: Vec<u8>,
        num_locals: usize,
    ) -> Handle<ClosureObject> {
        make_test_closure_with_metadata(context, bytes, None, num_locals, 0)
    }

    fn make_test_closure_with_metadata(
        context: &mut JitContextScope<'_>,
        bytes: Vec<u8>,
        constant_table: Option<Handle<ConstantTable>>,
        num_locals: usize,
        num_parameters: usize,
    ) -> Handle<ClosureObject> {
        make_test_closure_with_all_metadata(
            context,
            bytes,
            constant_table,
            None,
            num_locals,
            num_parameters,
        )
    }

    fn make_test_closure_with_all_metadata(
        context: &mut JitContextScope<'_>,
        bytes: Vec<u8>,
        constant_table: Option<Handle<ConstantTable>>,
        caches: Option<Handle<CacheArray>>,
        num_locals: usize,
        num_parameters: usize,
    ) -> Handle<ClosureObject> {
        let mut closure = None;
        expect_eval_ok(context.with_initial_realm(|context| {
            let raw = context.raw();
            let realm = raw.initial_realm();
            let function = BytecodeFunction::new_for_jit_test(
                raw,
                bytes,
                constant_table,
                None,
                caches,
                realm,
                num_locals as u32,
                num_parameters as u32,
            )?;
            closure = Some(ClosureObject::new_without_properties(
                raw,
                function,
                realm.default_global_scope(),
                None,
            )?);
            Ok(())
        }));
        closure.unwrap()
    }

    fn raw_jump_table(
        context: &mut JitContextScope<'_>,
        offsets: &[isize],
    ) -> Handle<ConstantTable> {
        use crate::runtime::gc::{GcType, Heap};

        let raw = context.raw();
        let raw_values: Vec<Value> = offsets
            .iter()
            .map(|&offset| Value::from_raw_bits(offset as u64))
            .collect();
        let constants = raw_values
            .iter()
            .map(Handle::<Value>::from_fixed_non_heap_ptr)
            .collect();
        let mut metadata = vec![0_u8; ConstantTable::calculate_metadata_size(offsets.len())];
        for index in 0..offsets.len() {
            metadata[index / u8::BITS as usize] |= 1 << (index % u8::BITS as usize);
        }
        // Raw offsets are not Values and must never enter the handle-root chain. Keep them in
        // stable host storage and copy them into metadata-marked raw slots exactly as the canonical
        // builder does. The forced collection simultaneously proves that the fixed handles are not
        // traced as Values and that the finished table skips its raw offset entries.
        let table = expect_alloc_ok(ConstantTable::new(raw, constants, metadata));
        Heap::run_gc(raw, GcType::Normal);
        table
    }

    fn value_table(context: &mut JitContextScope<'_>, values: &[Value]) -> Handle<ConstantTable> {
        let raw = context.raw();
        let constants = values.iter().map(|value| value.to_handle(raw)).collect();
        let metadata = vec![0_u8; ConstantTable::calculate_metadata_size(values.len())];
        expect_alloc_ok(ConstantTable::new(raw, constants, metadata))
    }

    fn with_bound_artifact<R>(
        owned: &mut OwnedContext,
        bytes: Vec<u8>,
        num_locals: usize,
        f: impl for<'scope> FnOnce(
            &mut JitContextScope<'scope>,
            &VmFunctionBinding<'scope>,
            &mut ExecutableCodeCache,
        ) -> R,
    ) -> R {
        owned.with_jit_context(|context| {
            let closure = make_test_closure(context, bytes, num_locals);
            let (binding, prepared) = prepare_vm_prototype(context, closure).unwrap();
            let mapped_len = ExecutableCodeCache::mapped_len_for(&prepared).unwrap();
            let mut cache = ExecutableCodeCache::new(mapped_len, 1).unwrap();
            cache.insert(1, prepared).unwrap();
            f(context, &binding, &mut cache)
        })
    }

    fn assert_native_matches_actual_vm(bytes: Vec<u8>, initial: Vec<Value>) {
        let num_locals = initial.len();
        let mut owned = ContextBuilder::new().build().unwrap();
        with_bound_artifact(&mut owned, bytes, num_locals, |context, binding, cache| {
            let loaded = cache.get(1).unwrap().unwrap();
            let vm_slots: Vec<JitSlot> = initial
                .iter()
                .map(|&value| JitSlot::try_from_value(context, value).unwrap())
                .collect();
            let expected = run_vm_only_returned(context, binding, loaded, &vm_slots);

            let mut native_slots = vm_slots;
            let (mut budget, _) =
                DeterministicInterruptBudget::new(NonZeroU32::new(10_000).unwrap());
            let outcome =
                run_vm_contained(context, loaded, binding, &mut native_slots, &mut budget).unwrap();
            assert_eq!(expect_native_returned(context, outcome), expected);
            assert!(!context.has_registered_jit_frame());
            context.raw().vm().debug_assert_stack_empty();
        });
    }

    fn assert_slow_path_matches_actual_vm(bytes: Vec<u8>, initial: Vec<Value>) {
        let num_locals = initial.len();
        let mut owned = ContextBuilder::new().build().unwrap();
        with_bound_artifact(&mut owned, bytes, num_locals, |context, binding, cache| {
            let loaded = cache.get(1).unwrap().unwrap();
            let vm_slots: Vec<JitSlot> = initial
                .iter()
                .map(|&value| JitSlot::try_from_value(context, value).unwrap())
                .collect();
            let expected = run_vm_only_returned(context, binding, loaded, &vm_slots);

            let mut contained_slots = vm_slots.clone();
            let slots_before = contained_slots.clone();
            let (mut budget, _) =
                DeterministicInterruptBudget::new(NonZeroU32::new(10_000).unwrap());
            let outcome =
                run_vm_contained(context, loaded, binding, &mut contained_slots, &mut budget)
                    .unwrap();
            assert_eq!(expect_vm_returned(context, outcome), expected);
            assert_eq!(
                contained_slots, slots_before,
                "the native slow exit must precede destination mutation; VM mutations stay private"
            );
            assert!(!context.has_registered_jit_frame());
            context.raw().vm().debug_assert_stack_empty();
        });
    }

    fn unrooted_string_pair(
        context: &mut JitContextScope<'_>,
        first: &str,
        second: &str,
    ) -> (JitSlot, JitSlot) {
        let mut raw = context.raw();
        let guard = HandleScopeGuard::new(raw);
        let first = expect_eval_ok(raw.alloc_string(first));
        let second = expect_eval_ok(raw.alloc_string(second));
        let slots = (
            JitSlot::try_from_value(context, *first.as_value()).unwrap(),
            JitSlot::try_from_value(context, *second.as_value()).unwrap(),
        );
        drop(guard);
        slots
    }

    fn constant_jump_program() -> Vec<u8> {
        let mut bytes = encode(OpCode::JumpConstant, &[0]);
        bytes.extend(encode(OpCode::LoadImmediate, &[local(0), 1]));
        bytes.extend(encode(OpCode::Ret, &[local(0)]));
        bytes
    }

    #[test]
    fn constant_backed_branch_must_match_rooted_table_at_compile_and_entry() {
        let bytes = constant_jump_program();
        let mut owned = ContextBuilder::new().build().unwrap();

        owned.with_jit_context(|context| {
            let wrong_kind_table = value_table(context, &[Value::undefined()]);
            let wrong_kind = make_test_closure_with_metadata(
                context,
                bytes.clone(),
                Some(wrong_kind_table),
                1,
                0,
            );
            assert!(matches!(
                prepare_vm_prototype(context, wrong_kind),
                Err(VmCompileError::Binding(VmBindingError::ValueConstantUnsupported { index: 0 }))
            ));

            let table = raw_jump_table(context, &[2]);
            let closure = make_test_closure_with_metadata(context, bytes, Some(table), 1, 0);
            let (binding, prepared) = prepare_vm_prototype(context, closure).unwrap();
            assert_eq!(prepared.program().instructions()[0].branch_constant, Some((0, 2)));
            let mapped_len = ExecutableCodeCache::mapped_len_for(&prepared).unwrap();
            let mut cache = ExecutableCodeCache::new(mapped_len, 1).unwrap();
            cache.insert(1, prepared).unwrap();

            let mut rooted_table = binding.constant_table.unwrap();
            rooted_table.set_constant(0, Value::from_raw_bits(5));
            let loaded = cache.get(1).unwrap().unwrap();
            let mut slots = [JitSlot::undefined()];
            let (mut budget, _) = DeterministicInterruptBudget::new(NonZeroU32::new(100).unwrap());
            let (result, helper) = with_test_helper_behavior(TestHelperBehavior::Normal, || {
                run_vm_contained(context, loaded, &binding, &mut slots, &mut budget)
            });
            assert!(matches!(
                result,
                Err(ContainedRunError::Binding(VmBindingError::ConstantJumpChanged {
                    index: 0,
                    actual: 5,
                    expected: 2,
                }))
            ));
            assert_eq!(helper.calls, 0, "mutated branch constants reject before native entry");
            context.raw().vm().debug_assert_stack_empty();
        });
    }

    #[test]
    fn value_constants_and_caches_are_rejected_without_caller_descriptors() {
        let mut bytes = encode(OpCode::LoadConstant, &[local(0), 0]);
        bytes.extend(encode(OpCode::Ret, &[local(0)]));
        let mut owned = ContextBuilder::new().build().unwrap();

        owned.with_jit_context(|context| {
            let table = value_table(context, &[Value::raw_smi(7)]);
            let closure =
                make_test_closure_with_metadata(context, bytes.clone(), Some(table), 1, 0);
            assert!(matches!(
                prepare_vm_prototype(context, closure),
                Err(VmCompileError::Binding(VmBindingError::ValueConstantUnsupported { index: 0 }))
            ));

            let caches = expect_alloc_ok(CacheArray::new(context.raw(), 1)).unwrap();
            let closure = make_test_closure_with_all_metadata(
                context,
                encode(OpCode::Ret, &[local(0)]),
                None,
                Some(caches),
                1,
                0,
            );
            assert!(matches!(
                prepare_vm_prototype(context, closure),
                Err(VmCompileError::Binding(VmBindingError::CacheArrayUnsupported { actual: 1 }))
            ));
            context.raw().vm().debug_assert_stack_empty();
        });
    }

    #[test]
    fn coercing_neg_is_rejected_before_native_entry() {
        let mut numeric_tail = encode(OpCode::Neg, &[local(1), local(0)]);
        numeric_tail.extend(encode(OpCode::Ret, &[local(1)]));
        let mut owned = ContextBuilder::new().build().unwrap();
        let mut slots = [JitSlot::undefined(), JitSlot::undefined()];
        let (mut budget, _) = DeterministicInterruptBudget::new(NonZeroU32::new(100).unwrap());
        with_bound_artifact(&mut owned, numeric_tail, 2, |context, binding, cache| {
            let mut raw = context.raw();
            let value = expect_eval_ok(raw.alloc_string("would coerce"));
            slots[0] = JitSlot::try_from_value(context, *value.as_value()).unwrap();
            let loaded = cache.get(1).unwrap().unwrap();
            let ((result, helper), handoff) = with_test_handoff_collections(
                TestHandoffGcBehavior { before_vm_frame: true, after_vm_frame: true },
                || {
                    with_test_helper_behavior(TestHelperBehavior::Normal, || {
                        run_vm_contained(context, loaded, binding, &mut slots, &mut budget)
                    })
                },
            );
            assert!(matches!(
                result,
                Err(ContainedRunError::Resume(VmResumeError::NonNumericOperand {
                    offset: 0,
                    operand: 1,
                    local: 0,
                }))
            ));
            assert_eq!(helper.calls, 0);
            assert_eq!(handoff, TestHandoffGcObservation::default());
            context.raw().vm().debug_assert_stack_empty();
        });
    }

    #[test]
    fn generated_numeric_local_families_match_the_actual_brimstone_vm() {
        for opcode in [
            OpCode::Add,
            OpCode::Sub,
            OpCode::Mul,
            OpCode::BitAnd,
            OpCode::BitOr,
            OpCode::BitXor,
            OpCode::ShiftLeft,
            OpCode::ShiftRightArithmetic,
            OpCode::ShiftRightLogical,
        ] {
            let mut bytes = encode(opcode, &[local(0), local(1), local(2)]);
            bytes.extend(encode(OpCode::Ret, &[local(0)]));
            assert_native_matches_actual_vm(
                bytes,
                vec![Value::undefined(), Value::raw_smi(13), Value::raw_smi(3)],
            );
        }

        for opcode in [
            OpCode::AddImm,
            OpCode::SubImm,
            OpCode::MulImm,
            OpCode::BitAndImm,
            OpCode::BitOrImm,
            OpCode::BitXorImm,
            OpCode::ShiftLeftImm,
            OpCode::ShiftRightArithmeticImm,
            OpCode::ShiftRightLogicalImm,
        ] {
            let mut bytes = encode(opcode, &[local(0), local(1), 3]);
            bytes.extend(encode(OpCode::Ret, &[local(0)]));
            assert_native_matches_actual_vm(bytes, vec![Value::undefined(), Value::raw_smi(13)]);
        }

        for opcode in [OpCode::Neg, OpCode::BitNot] {
            let mut bytes = encode(opcode, &[local(0), local(1)]);
            bytes.extend(encode(OpCode::Ret, &[local(0)]));
            assert_native_matches_actual_vm(bytes, vec![Value::undefined(), Value::raw_smi(7)]);
        }
        for opcode in [OpCode::Inc, OpCode::Dec] {
            let mut bytes = encode(opcode, &[local(0)]);
            bytes.extend(encode(OpCode::Ret, &[local(0)]));
            assert_native_matches_actual_vm(bytes, vec![Value::raw_smi(7)]);
        }

        for (opcode, right) in [
            (OpCode::StrictEqual, 3),
            (OpCode::StrictNotEqual, 7),
            (OpCode::LessThan, 7),
            (OpCode::LessThanOrEqual, 3),
            (OpCode::GreaterThan, 1),
            (OpCode::GreaterThanOrEqual, 3),
        ] {
            let mut bytes = encode(opcode, &[local(0), local(1), local(2)]);
            bytes.extend(encode(OpCode::Ret, &[local(0)]));
            assert_native_matches_actual_vm(
                bytes,
                vec![Value::undefined(), Value::raw_smi(3), Value::raw_smi(right)],
            );
        }

        let mut logical_not = encode(OpCode::LogNot, &[local(0), local(1)]);
        logical_not.extend(encode(OpCode::Ret, &[local(0)]));
        for value in [
            Value::undefined(),
            Value::null(),
            Value::bool(false),
            Value::bool(true),
            Value::raw_smi(0),
            Value::raw_smi(-1),
        ] {
            assert_native_matches_actual_vm(logical_not.clone(), vec![Value::undefined(), value]);
        }
    }

    #[test]
    fn generated_constants_moves_and_branch_families_match_the_actual_brimstone_vm() {
        for (opcode, operands) in [
            (OpCode::LoadImmediate, vec![local(0), 7]),
            (OpCode::LoadUndefined, vec![local(0)]),
            (OpCode::LoadNull, vec![local(0)]),
            (OpCode::LoadTrue, vec![local(0)]),
            (OpCode::LoadFalse, vec![local(0)]),
        ] {
            let mut bytes = encode(opcode, &operands);
            bytes.extend(encode(OpCode::Mov, &[local(1), local(0)]));
            bytes.extend(encode(OpCode::Ret, &[local(1)]));
            assert_native_matches_actual_vm(bytes, vec![Value::undefined(); 2]);
        }

        let branch_program = |opcode: OpCode| {
            let mut bytes = encode(opcode, &[local(0), 8]); // 0 -> 8
            bytes.extend(encode(OpCode::LoadImmediate, &[local(1), 2])); // 3
            bytes.extend(encode(OpCode::Jump, &[5])); // 6 -> 11
            bytes.extend(encode(OpCode::LoadImmediate, &[local(1), 7])); // 8
            bytes.extend(encode(OpCode::Ret, &[local(1)])); // 11
            bytes
        };

        for (opcode, values) in [
            (OpCode::JumpTrue, vec![Value::bool(false), Value::bool(true)]),
            (OpCode::JumpFalse, vec![Value::bool(false), Value::bool(true)]),
            (
                OpCode::JumpToBooleanTrue,
                vec![
                    Value::undefined(),
                    Value::null(),
                    Value::bool(false),
                    Value::bool(true),
                    Value::raw_smi(0),
                    Value::raw_smi(-3),
                ],
            ),
            (
                OpCode::JumpToBooleanFalse,
                vec![
                    Value::undefined(),
                    Value::null(),
                    Value::bool(false),
                    Value::bool(true),
                    Value::raw_smi(0),
                    Value::raw_smi(-3),
                ],
            ),
            (
                OpCode::JumpNotUndefined,
                vec![Value::undefined(), Value::null(), Value::raw_smi(1)],
            ),
            (OpCode::JumpNullish, vec![Value::undefined(), Value::null(), Value::raw_smi(0)]),
            (
                OpCode::JumpNotNullish,
                vec![Value::undefined(), Value::null(), Value::raw_smi(0)],
            ),
        ] {
            let bytes = branch_program(opcode);
            for value in values {
                assert_native_matches_actual_vm(bytes.clone(), vec![value, Value::undefined()]);
            }
        }

        let mut unconditional = encode(OpCode::Jump, &[5]); // 0 -> 5
        unconditional.extend(encode(OpCode::LoadImmediate, &[local(0), 2])); // 2
        unconditional.extend(encode(OpCode::LoadImmediate, &[local(0), 7])); // 5
        unconditional.extend(encode(OpCode::Ret, &[local(0)])); // 8
        assert_native_matches_actual_vm(unconditional, vec![Value::undefined()]);

        let mut loop_program = encode(OpCode::LoadImmediate, &[local(0), 0]);
        loop_program.extend(encode(OpCode::LoadImmediate, &[local(1), 9]));
        let loop_offset = loop_program.len();
        loop_program.extend(encode(OpCode::AddImm, &[local(0), local(0), 1]));
        loop_program.extend(encode(OpCode::LessThan, &[local(2), local(0), local(1)]));
        let branch_offset = loop_program.len();
        loop_program.extend(encode(
            OpCode::JumpTrue,
            &[
                local(2),
                (loop_offset as isize - branch_offset as isize) as i8 as u8,
            ],
        ));
        loop_program.extend(encode(OpCode::Ret, &[local(0)]));
        assert_native_matches_actual_vm(loop_program, vec![Value::undefined(); 3]);
    }

    #[test]
    fn generated_rooted_constant_branch_matches_the_actual_brimstone_vm() {
        let mut bytes = encode(OpCode::JumpTrueConstant, &[local(0), 0]); // 0 -> 8
        bytes.extend(encode(OpCode::LoadImmediate, &[local(1), 2])); // 3
        bytes.extend(encode(OpCode::Jump, &[5])); // 6 -> 11
        bytes.extend(encode(OpCode::LoadImmediate, &[local(1), 7])); // 8
        bytes.extend(encode(OpCode::Ret, &[local(1)])); // 11

        let mut owned = ContextBuilder::new().build().unwrap();
        owned.with_jit_context(|context| {
            let table = raw_jump_table(context, &[8]);
            let closure = make_test_closure_with_metadata(context, bytes, Some(table), 2, 0);
            let (binding, prepared) = prepare_vm_prototype(context, closure).unwrap();
            assert_eq!(prepared.program().instructions()[0].branch_target, Some(8));
            let mapped_len = ExecutableCodeCache::mapped_len_for(&prepared).unwrap();
            let mut cache = ExecutableCodeCache::new(mapped_len, 1).unwrap();
            cache.insert(1, prepared).unwrap();
            let loaded = cache.get(1).unwrap().unwrap();

            for condition in [Value::bool(false), Value::bool(true)] {
                let vm_slots = vec![
                    JitSlot::try_from_value(context, condition).unwrap(),
                    JitSlot::undefined(),
                ];
                let expected = run_vm_only_returned(context, &binding, loaded, &vm_slots);
                let mut native_slots = vm_slots;
                let (mut budget, _) =
                    DeterministicInterruptBudget::new(NonZeroU32::new(100).unwrap());
                let outcome =
                    run_vm_contained(context, loaded, &binding, &mut native_slots, &mut budget)
                        .unwrap();
                assert_eq!(expect_native_returned(context, outcome), expected);
                assert!(!context.has_registered_jit_frame());
                context.raw().vm().debug_assert_stack_empty();
            }
        });
    }

    #[test]
    fn generated_numeric_slow_paths_side_exit_before_effects_and_match_the_actual_vm() {
        let mut add_overflow = encode(OpCode::Add, &[local(0), local(1), local(2)]);
        add_overflow.extend(encode(OpCode::Ret, &[local(0)]));
        assert_slow_path_matches_actual_vm(
            add_overflow,
            vec![
                Value::undefined(),
                Value::raw_smi(i32::MAX),
                Value::raw_smi(1),
            ],
        );

        let mut negative_zero_mul = encode(OpCode::Mul, &[local(0), local(1), local(2)]);
        negative_zero_mul.extend(encode(OpCode::Ret, &[local(0)]));
        assert_slow_path_matches_actual_vm(
            negative_zero_mul,
            vec![Value::undefined(), Value::raw_smi(-1), Value::raw_smi(0)],
        );

        let mut negative_zero_neg = encode(OpCode::Neg, &[local(0), local(1)]);
        negative_zero_neg.extend(encode(OpCode::Ret, &[local(0)]));
        assert_slow_path_matches_actual_vm(
            negative_zero_neg,
            vec![Value::undefined(), Value::raw_smi(0)],
        );

        let mut unsigned_shift = encode(OpCode::ShiftRightLogical, &[local(0), local(1), local(2)]);
        unsigned_shift.extend(encode(OpCode::Ret, &[local(0)]));
        assert_slow_path_matches_actual_vm(
            unsigned_shift,
            vec![Value::undefined(), Value::raw_smi(-1), Value::raw_smi(0)],
        );

        let mut immediate_truthiness = encode(OpCode::LogNot, &[local(0), local(1)]);
        immediate_truthiness.extend(encode(OpCode::Ret, &[local(0)]));
        assert_slow_path_matches_actual_vm(
            immediate_truthiness,
            vec![Value::undefined(), Value::number(0.5_f64)],
        );

        let mut non_smi_strict = encode(OpCode::StrictEqual, &[local(0), local(1), local(2)]);
        non_smi_strict.extend(encode(OpCode::Ret, &[local(0)]));
        assert_slow_path_matches_actual_vm(
            non_smi_strict,
            vec![
                Value::undefined(),
                Value::number(0.5_f64),
                Value::number(0.5_f64),
            ],
        );
    }

    #[test]
    fn vm_resume_setup_rejects_bad_count_and_offset_before_frame_mutation() {
        let mut bytes = encode(OpCode::Neg, &[local(0), local(0)]);
        bytes.extend(encode(OpCode::Ret, &[local(0)]));
        let bytecode_len = bytes.len();
        let mut owned = ContextBuilder::new().build().unwrap();

        owned.with_jit_context(|context| {
            let closure = make_test_closure(context, bytes, 1);
            let (binding, prepared) = prepare_vm_prototype(context, closure).unwrap();
            let mapped_len = ExecutableCodeCache::mapped_len_for(&prepared).unwrap();
            let mut cache = ExecutableCodeCache::new(mapped_len, 1).unwrap();
            cache.insert(1, prepared).unwrap();
            let loaded = cache.get(1).unwrap().unwrap();
            let slots = [
                JitSlot::try_from_value(context, Value::raw_smi(1)).unwrap(),
                JitSlot::undefined(),
            ];

            for invalid_offset in [1, bytecode_len] {
                assert!(matches!(
                    admit_vm_resume(context, &binding, loaded, invalid_offset, &slots),
                    Err(ContainedRunError::Resume(VmResumeError::InvalidBytecodeOffset(
                        actual
                    ))) if actual == invalid_offset
                ));
            }

            let admitted = admit_vm_resume(context, &binding, loaded, 0, &slots).unwrap();
            let (mut budget, _) = DeterministicInterruptBudget::new(NonZeroU32::new(100).unwrap());
            expect_eval_ok(context.with_initial_realm(|context| {
                let mut raw = context.raw();
                let (wrong_count, hook_ran) = with_test_jit_resume_collection(|| {
                    raw.vm()
                        .resume_from_jit_side_exit(&admitted, &[], &mut budget)
                });
                assert!(matches!(
                    wrong_count,
                    Err(JitResumeSetupError::RegisterCountMismatch { actual: 0, expected: 2 })
                ));
                assert!(!hook_ran, "setup rejection must precede frame publication");
                Ok(())
            }));
            context.raw().vm().debug_assert_stack_empty();
        });
    }

    #[test]
    fn near_capacity_vm_resume_rejects_before_publication_and_context_recovers() {
        let mut bytes = encode(OpCode::DivImm, &[local(0), local(0), (-1_i8) as u8]);
        bytes.extend(encode(OpCode::Ret, &[local(0)]));
        let mut owned = ContextBuilder::new().build().unwrap();

        owned.with_jit_context(|context| {
            // This resumed frame would exactly fill an otherwise empty VM stack. The mandatory
            // initial-realm frame makes it too large, which used to form an out-of-allocation
            // pointer before the old bounds comparison could reject it.
            let num_locals = NUM_STACK_SLOTS - FIRST_ARGUMENT_SLOT_INDEX - 1;
            let closure = make_test_closure(context, bytes.clone(), num_locals);
            let (binding, prepared) = prepare_vm_prototype(context, closure).unwrap();
            let mapped_len = ExecutableCodeCache::mapped_len_for(&prepared).unwrap();
            let mut cache = ExecutableCodeCache::new(mapped_len, 1).unwrap();
            cache.insert(1, prepared).unwrap();
            let loaded = cache.get(1).unwrap().unwrap();

            let mut slots = vec![JitSlot::undefined(); num_locals];
            slots[0] = JitSlot::try_from_value(context, Value::raw_smi(1)).unwrap();
            slots.push(JitSlot::undefined());
            let admitted = admit_vm_resume(context, &binding, loaded, 0, &slots).unwrap();

            let raw = context.raw();
            let undefined = Value::undefined().to_handle(raw);
            let mut roots = vec![undefined; num_locals + 1];
            roots[0] = Value::raw_smi(1).to_handle(raw);
            let (mut budget, _) = DeterministicInterruptBudget::new(NonZeroU32::new(100).unwrap());
            expect_eval_ok(context.with_initial_realm(|context| {
                let mut raw = context.raw();
                let stack_before = raw.vm().jit_stack_state_for_test();
                let (result, hook_ran) = with_test_jit_resume_collection(|| {
                    raw.vm()
                        .resume_from_jit_side_exit(&admitted, &roots, &mut budget)
                });
                assert!(matches!(
                    result,
                    Ok(JitResumeOutcome::Completed(Err(EvalError::Value(_))))
                ));
                assert!(!hook_ran, "overflow rejection must precede frame publication");
                assert_eq!(raw.vm().jit_stack_state_for_test(), stack_before);
                Ok(())
            }));
            context.raw().vm().debug_assert_stack_empty();

            // A normal bound continuation in the same context proves the rejected setup did not
            // change FP/SP/depth or poison subsequent entry.
            let small_closure = make_test_closure(context, bytes, 1);
            let (small_binding, small_prepared) =
                prepare_vm_prototype(context, small_closure).unwrap();
            let small_mapped_len = ExecutableCodeCache::mapped_len_for(&small_prepared).unwrap();
            let mut small_cache = ExecutableCodeCache::new(small_mapped_len, 1).unwrap();
            small_cache.insert(1, small_prepared).unwrap();
            let small_loaded = small_cache.get(1).unwrap().unwrap();
            let mut small_slots = [JitSlot::try_from_value(context, Value::raw_smi(1)).unwrap()];
            let (mut budget, _) = DeterministicInterruptBudget::new(NonZeroU32::new(100).unwrap());
            let outcome = run_vm_contained(
                context,
                small_loaded,
                &small_binding,
                &mut small_slots,
                &mut budget,
            )
            .unwrap();
            assert_eq!(expect_vm_returned(context, outcome), Value::raw_smi(-1).as_raw_bits());
            context.raw().vm().debug_assert_stack_empty();
        });
    }

    #[cfg(feature = "alloc_error")]
    #[test]
    fn vm_allocation_failure_pops_resumed_frame_and_context_recovers() {
        let mut bytes = encode(OpCode::NewObject, &[local(0), 0]);
        let resume_offset = bytes.len();
        bytes.extend(encode(OpCode::DivImm, &[local(1), local(1), (-1_i8) as u8]));
        bytes.extend(encode(OpCode::Ret, &[local(1)]));

        let mut owned = ContextBuilder::new().build().unwrap();
        let mut slots = [JitSlot::undefined(), JitSlot::undefined()];
        let (mut budget, _) = DeterministicInterruptBudget::new(NonZeroU32::new(100).unwrap());
        with_bound_artifact(&mut owned, bytes, 2, |context, binding, cache| {
            slots[1] = JitSlot::try_from_value(context, Value::raw_smi(3)).unwrap();
            let loaded = cache.get(1).unwrap().unwrap();
            let ((outcome, helper), handoff) = with_test_handoff_collections(
                TestHandoffGcBehavior { before_vm_frame: false, after_vm_frame: true },
                || {
                    with_test_jit_resume_allocation_failure(|| {
                        with_test_helper_behavior(TestHelperBehavior::Normal, || {
                            run_vm_contained(context, loaded, binding, &mut slots, &mut budget)
                                .unwrap()
                        })
                    })
                },
            );
            assert_eq!(outcome, ContainedOutcome::VmAllocationFailedAt(resume_offset));
            assert_eq!(helper.calls, 1);
            assert!(handoff.after_vm_frame.is_some());
            assert!(!context.has_registered_jit_frame());
            context.raw().vm().debug_assert_stack_empty();

            let (outcome, helper) = with_test_helper_behavior(TestHelperBehavior::Normal, || {
                run_vm_contained(context, loaded, binding, &mut slots, &mut budget).unwrap()
            });
            assert_eq!(expect_vm_returned(context, outcome), Value::raw_smi(-3).as_raw_bits());
            assert_eq!(helper.calls, 1);
            context.raw().vm().debug_assert_stack_empty();
        });
    }

    #[test]
    fn two_allocating_calls_have_distinct_maps_and_resume_without_replay() {
        let mut bytes = encode(OpCode::NewObject, &[local(0), 0]);
        let second_call_offset = bytes.len();
        bytes.extend(encode(OpCode::NewObject, &[local(1), 0]));
        bytes.extend(encode(OpCode::Mov, &[local(3), local(0)]));
        bytes.extend(encode(OpCode::LoadImmediate, &[local(2), 5]));
        let resume_offset = bytes.len();
        bytes.extend(encode(OpCode::DivImm, &[local(4), local(2), (-1_i8) as u8]));
        bytes.extend(encode(OpCode::Ret, &[local(4)]));

        let mut owned = ContextBuilder::new().build().unwrap();
        let mut slots = vec![JitSlot::undefined(); 5];
        let (mut budget, _) = DeterministicInterruptBudget::new(NonZeroU32::new(100).unwrap());
        with_bound_artifact(&mut owned, bytes, 5, |context, binding, cache| {
            let loaded = cache.get(1).unwrap().unwrap();
            let records = loaded.safepoints().records();
            assert_eq!(records.len(), 2);
            assert_eq!(records[0].bytecode_offset, 0);
            assert_eq!(records[1].bytecode_offset as usize, second_call_offset);
            assert_ne!(records[0].native_return_offset, records[1].native_return_offset);
            assert_eq!(records[0].live_slot_count, 1);
            let first_start = records[0].live_slot_start as usize;
            let first_end = first_start + records[0].live_slot_count as usize;
            assert_eq!(&loaded.safepoints().live_slots()[first_start..first_end], &[5]);
            let second_start = records[1].live_slot_start as usize;
            let second_end = second_start + records[1].live_slot_count as usize;
            assert_eq!(&loaded.safepoints().live_slots()[second_start..second_end], &[0, 5]);

            let (outcome, observation) =
                with_test_helper_behavior(TestHelperBehavior::Normal, || {
                    run_vm_contained(context, loaded, binding, &mut slots, &mut budget).unwrap()
                });
            assert_eq!(expect_vm_returned(context, outcome), Value::raw_smi(-5).as_raw_bits());
            assert_eq!(observation.calls, 2, "committed allocating helpers must not replay");
            assert!(slots[0].value().is_object());
            assert!(slots[1].value().is_object());
            assert_eq!(resume_offset, loaded.program().instructions()[4].offset);
            assert!(!context.has_registered_jit_frame());
        });
    }

    #[test]
    fn forced_collection_overwrites_moving_destination_and_updates_live_slots() {
        let mut bytes = encode(OpCode::NewObject, &[local(1), 0]);
        bytes.extend(encode(OpCode::Mov, &[local(3), local(0)]));
        bytes.extend(encode(OpCode::DivImm, &[local(2), local(2), (-1_i8) as u8]));
        bytes.extend(encode(OpCode::Ret, &[local(2)]));

        let mut owned = ContextBuilder::new().build().unwrap();
        let mut slots = vec![JitSlot::undefined(); 4];
        let (mut budget, _) = DeterministicInterruptBudget::new(NonZeroU32::new(100).unwrap());
        with_bound_artifact(&mut owned, bytes, 4, |context, binding, cache| {
            (slots[0], slots[1]) =
                unrooted_string_pair(context, "live source", "overwritten destination");
            slots[2] = JitSlot::try_from_value(context, Value::raw_smi(5)).unwrap();
            let live_before = slots[0].value().as_raw_bits();
            let overwritten_before = slots[1].value().as_raw_bits();
            let loaded = cache.get(1).unwrap().unwrap();
            assert_eq!(loaded.safepoints().live_slots(), &[0, 2, 4]);
            assert_eq!(loaded.safepoints().records()[0].result_slot, 1);

            let (outcome, observation) = with_test_helper_behavior(
                TestHelperBehavior::ForceCollectionAfterAllocation,
                || run_vm_contained(context, loaded, binding, &mut slots, &mut budget).unwrap(),
            );
            assert_eq!(expect_vm_returned(context, outcome), Value::raw_smi(-5).as_raw_bits());
            assert_eq!(observation.calls, 1);
            assert_ne!(slots[0].value().as_raw_bits(), live_before);
            assert_ne!(slots[1].value().as_raw_bits(), overwritten_before);
            assert!(slots[1].value().is_object());
            assert_eq!(slots[3].value().as_raw_bits(), slots[0].value().as_raw_bits());
            assert!(slots[0].value().is::<StringValue>());
        });
    }

    #[test]
    fn native_return_clears_dead_pointer_slots_before_forced_collection() {
        let mut bytes = encode(OpCode::NewObject, &[local(0), 0]);
        bytes.extend(encode(OpCode::Ret, &[local(0)]));
        let mut owned = ContextBuilder::new().build().unwrap();
        let mut slots = [JitSlot::undefined(), JitSlot::undefined()];
        let (mut budget, _) = DeterministicInterruptBudget::new(NonZeroU32::new(100).unwrap());

        with_bound_artifact(&mut owned, bytes, 2, |context, binding, cache| {
            let (dead, _) = unrooted_string_pair(context, "dead native slot", "unused peer");
            slots[1] = dead;
            let loaded = cache.get(1).unwrap().unwrap();
            assert_eq!(loaded.safepoints().live_slots(), &[2]);

            let (outcome, helper) = with_test_helper_behavior(
                TestHelperBehavior::ForceCollectionAfterAllocation,
                || run_vm_contained(context, loaded, binding, &mut slots, &mut budget).unwrap(),
            );
            assert!(matches!(outcome, ContainedOutcome::NativeReturned(_)));
            assert_eq!(helper.calls, 1);
            assert!(slots[0].value().is_object());
            assert_eq!(slots[1], JitSlot::undefined());

            // Re-linking the same caller array proves no stale pointer escaped the native return.
            let outcome =
                run_vm_contained(context, loaded, binding, &mut slots, &mut budget).unwrap();
            assert!(matches!(outcome, ContainedOutcome::NativeReturned(_)));
        });
    }

    #[test]
    fn terminal_helper_outcomes_clear_dead_slots_after_forced_collection() {
        let mut bytes = encode(OpCode::NewObject, &[local(0), 0]);
        bytes.extend(encode(OpCode::DivImm, &[local(1), local(1), (-1_i8) as u8]));
        bytes.extend(encode(OpCode::Ret, &[local(1)]));

        for (behavior, expected_poison) in [
            (TestHelperBehavior::ForceCollectionThenAllocationFailure, false),
            (TestHelperBehavior::ForceCollectionThenPanic, true),
        ] {
            let mut owned = ContextBuilder::new().build().unwrap();
            let mut slots = [JitSlot::undefined(); 3];
            let (mut budget, _) = DeterministicInterruptBudget::new(NonZeroU32::new(100).unwrap());
            with_bound_artifact(&mut owned, bytes.clone(), 3, |context, binding, cache| {
                let (dead, _) = unrooted_string_pair(context, "dead terminal slot", "unused peer");
                slots[1] = JitSlot::try_from_value(context, Value::raw_smi(3)).unwrap();
                slots[2] = dead;
                let loaded = cache.get(1).unwrap().unwrap();
                let (outcome, helper) = with_test_helper_behavior(behavior, || {
                    run_vm_contained(context, loaded, binding, &mut slots, &mut budget).unwrap()
                });
                if expected_poison {
                    assert_eq!(outcome, ContainedOutcome::PoisonedAt(0));
                } else {
                    assert_eq!(outcome, ContainedOutcome::AllocationFailedAt(0));
                }
                assert_eq!(helper.calls, 1);
                assert_eq!(slots[0], JitSlot::undefined());
                assert_eq!(slots[2], JitSlot::undefined());

                let outcome =
                    run_vm_contained(context, loaded, binding, &mut slots, &mut budget).unwrap();
                assert_eq!(expect_vm_returned(context, outcome), Value::raw_smi(-3).as_raw_bits());
            });
        }
    }

    #[test]
    fn post_vm_collection_then_panic_resyncs_slots_and_recovers_context() {
        let mut bytes = encode(OpCode::NewObject, &[local(0), 0]);
        bytes.extend(encode(OpCode::DivImm, &[local(1), local(1), (-1_i8) as u8]));
        bytes.extend(encode(OpCode::Ret, &[local(1)]));
        let mut owned = ContextBuilder::new().build().unwrap();
        let mut slots = [JitSlot::undefined(), JitSlot::undefined()];
        let (mut budget, _) = DeterministicInterruptBudget::new(NonZeroU32::new(100).unwrap());

        with_bound_artifact(&mut owned, bytes, 2, |context, binding, cache| {
            slots[1] = JitSlot::try_from_value(context, Value::raw_smi(3)).unwrap();
            let loaded = cache.get(1).unwrap().unwrap();
            let result = catch_unwind(AssertUnwindSafe(|| {
                with_test_handoff_collections(
                    TestHandoffGcBehavior { before_vm_frame: false, after_vm_frame: true },
                    || {
                        with_test_jit_resume_dispatch_panic(|| {
                            run_vm_contained(context, loaded, binding, &mut slots, &mut budget)
                                .unwrap()
                        })
                    },
                )
            }));
            assert!(result.is_err());
            assert!(!context.has_registered_jit_frame());
            context.raw().vm().debug_assert_stack_empty();
            for slot in &slots {
                JitSlot::try_from_value(context, slot.value()).unwrap();
            }
            assert!(slots[0].value().is_object());

            let outcome =
                run_vm_contained(context, loaded, binding, &mut slots, &mut budget).unwrap();
            assert_eq!(expect_vm_returned(context, outcome), Value::raw_smi(-3).as_raw_bits());
        });
    }

    #[test]
    fn inner_dispatch_panic_exits_handle_scope_restores_stack_and_recovers() {
        let mut bytes = encode(OpCode::Div, &[local(0), local(0), local(1)]);
        bytes.extend(encode(OpCode::Ret, &[local(0)]));
        let mut owned = ContextBuilder::new().build().unwrap();
        let mut slots = vec![JitSlot::undefined(); 2];
        let (mut budget, _) = DeterministicInterruptBudget::new(NonZeroU32::new(100).unwrap());

        with_bound_artifact(&mut owned, bytes, 2, |context, binding, cache| {
            slots[0] = JitSlot::try_from_value(context, Value::raw_smi(6)).unwrap();
            slots[1] = JitSlot::try_from_value(context, Value::raw_smi(2)).unwrap();
            let loaded = cache.get(1).unwrap().unwrap();
            let stack_before = context.raw().vm().jit_stack_state_for_test();
            #[cfg(feature = "handle_stats")]
            let handles_before = context.raw().vm().jit_handle_count_for_test();

            let result = catch_unwind(AssertUnwindSafe(|| {
                with_test_jit_resume_inner_dispatch_panic(|| {
                    run_vm_contained(context, loaded, binding, &mut slots, &mut budget).unwrap()
                })
            }));
            assert!(result.is_err());
            assert_eq!(context.raw().vm().jit_stack_state_for_test(), stack_before);
            #[cfg(feature = "handle_stats")]
            assert_eq!(
                context.raw().vm().jit_handle_count_for_test(),
                handles_before + slots.len() + 1,
                "the local and implicit receiver bridge roots remain in the outer JIT scope, but the inner sentinel is gone"
            );
            assert!(!context.has_registered_jit_frame());

            let outcome =
                run_vm_contained(context, loaded, binding, &mut slots, &mut budget).unwrap();
            assert_eq!(expect_vm_returned(context, outcome), Value::raw_smi(3).as_raw_bits());
            context.raw().vm().debug_assert_stack_empty();
        });
    }

    #[test]
    fn identity_and_bridge_roots_move_on_both_sides_of_vm_frame_publication() {
        let mut bytes = encode(OpCode::NewObject, &[local(1), 0]);
        bytes.extend(encode(OpCode::Throw, &[local(0)]));

        let mut owned = ContextBuilder::new().build().unwrap();
        let mut slots = vec![JitSlot::undefined(); 2];
        let (mut budget, _) = DeterministicInterruptBudget::new(NonZeroU32::new(100).unwrap());
        with_bound_artifact(&mut owned, bytes, 2, |context, binding, cache| {
            let mut raw = context.raw();
            let guard = HandleScopeGuard::new(raw);
            let thrown = expect_eval_ok(raw.alloc_string("rooted thrown value"));
            slots[0] = JitSlot::try_from_value(context, *thrown.as_value()).unwrap();
            drop(guard);

            let loaded = cache.get(1).unwrap().unwrap();
            let ((outcome, helper), handoff) = with_test_handoff_collections(
                TestHandoffGcBehavior { before_vm_frame: true, after_vm_frame: true },
                || {
                    with_test_helper_behavior(TestHelperBehavior::Normal, || {
                        run_vm_contained(context, loaded, binding, &mut slots, &mut budget).unwrap()
                    })
                },
            );
            let bits = expect_vm_threw(context, outcome);
            assert_eq!(helper.calls, 1, "throw continuation must not replay the helper");
            let thrown = Value::from_raw_bits(bits);
            assert!(thrown.is::<StringValue>());
            assert_eq!(thrown.as_string().len(), "rooted thrown value".len() as u32);

            for point in [handoff.before_vm_frame, handoff.after_vm_frame] {
                let point = point.expect("configured handoff collection must run");
                assert_ne!(point.identities_before.closure, point.identities_after.closure);
                assert_ne!(point.identities_before.function, point.identities_after.function);
                assert_ne!(point.identities_before.bytecode, point.identities_after.bytecode);
                assert_eq!(point.identities_before.realm, point.identities_after.realm);
                assert_eq!(point.identities_before.scope, point.identities_after.scope);
                assert_ne!(point.first_root_before, point.first_root_after);
            }
            assert!(!context.has_registered_jit_frame());
        });
    }

    #[test]
    fn identical_bytecode_from_another_function_cannot_rebind_loaded_code() {
        let mut bytes = encode(OpCode::NewObject, &[local(0), 0]);
        bytes.extend(encode(OpCode::DivImm, &[local(1), local(1), (-1_i8) as u8]));
        bytes.extend(encode(OpCode::Ret, &[local(1)]));
        let mut owned = ContextBuilder::new().build().unwrap();

        owned.with_jit_context(|context| {
            let closure_a = make_test_closure(context, bytes.clone(), 2);
            let closure_b = make_test_closure(context, bytes, 2);
            let (binding_a, prepared_a) = prepare_vm_prototype(context, closure_a).unwrap();
            let (binding_b, _prepared_b) = prepare_vm_prototype(context, closure_b).unwrap();
            let mapped_len = ExecutableCodeCache::mapped_len_for(&prepared_a).unwrap();
            let mut cache = ExecutableCodeCache::new(mapped_len, 1).unwrap();
            cache.insert(1, prepared_a).unwrap();

            let mut slots = vec![JitSlot::undefined(); 2];
            slots[1] = JitSlot::try_from_value(context, Value::raw_smi(3)).unwrap();
            let (mut budget, _) = DeterministicInterruptBudget::new(NonZeroU32::new(100).unwrap());
            let loaded = cache.get(1).unwrap().unwrap();
            let (result, observation) =
                with_test_helper_behavior(TestHelperBehavior::Normal, || {
                    run_vm_contained(context, loaded, &binding_b, &mut slots, &mut budget)
                });
            assert!(matches!(
                result,
                Err(ContainedRunError::Binding(VmBindingError::ArtifactIdentityMismatch))
            ));
            assert_eq!(observation.calls, 0, "mismatched identity must reject before native entry");

            let outcome =
                run_vm_contained(context, loaded, &binding_a, &mut slots, &mut budget).unwrap();
            assert_eq!(expect_vm_returned(context, outcome), Value::raw_smi(-3).as_raw_bits());
        });
    }

    #[test]
    fn helper_failures_interrupts_and_unsupported_paths_are_terminal() {
        let mut helper_bytes = encode(OpCode::NewObject, &[local(0), 0]);
        helper_bytes.extend(encode(OpCode::Neg, &[local(1), local(1)]));
        helper_bytes.extend(encode(OpCode::Ret, &[local(1)]));

        for behavior in [
            TestHelperBehavior::AllocationFailure,
            TestHelperBehavior::PanicBeforeAllocation,
        ] {
            let mut owned = ContextBuilder::new().build().unwrap();
            let mut slots = vec![JitSlot::undefined(); 2];
            let seed = slots.clone();
            let (mut budget, _) = DeterministicInterruptBudget::new(NonZeroU32::new(100).unwrap());
            with_bound_artifact(&mut owned, helper_bytes.clone(), 2, |context, binding, cache| {
                let loaded = cache.get(1).unwrap().unwrap();
                let (outcome, observation) = with_test_helper_behavior(behavior, || {
                    run_vm_contained(context, loaded, binding, &mut slots, &mut budget).unwrap()
                });
                match behavior {
                    TestHelperBehavior::AllocationFailure => {
                        assert_eq!(outcome, ContainedOutcome::AllocationFailedAt(0));
                    }
                    TestHelperBehavior::PanicBeforeAllocation => {
                        assert_eq!(outcome, ContainedOutcome::PoisonedAt(0));
                    }
                    other => panic!("unexpected terminal helper behavior {other:?}"),
                }
                assert_eq!(observation.calls, 1);
                assert_eq!(slots, seed, "terminal helper failure must not continue");
            });
        }

        let mut owned = ContextBuilder::new().build().unwrap();
        let mut slots = vec![JitSlot::undefined(); 2];
        let seed = slots.clone();
        let (mut budget, _) = DeterministicInterruptBudget::new(NonZeroU32::new(1).unwrap());
        with_bound_artifact(&mut owned, helper_bytes, 2, |context, binding, cache| {
            let loaded = cache.get(1).unwrap().unwrap();
            let (outcome, observation) =
                with_test_helper_behavior(TestHelperBehavior::Normal, || {
                    run_vm_contained(context, loaded, binding, &mut slots, &mut budget).unwrap()
                });
            assert_eq!(outcome, ContainedOutcome::InterruptedAt(0));
            assert_eq!(observation.calls, 1);
            assert_eq!(observation.object_before, 0);
            assert_eq!(slots, seed);
        });

        let mut unsupported = encode(OpCode::NewObject, &[local(0), 1]);
        unsupported.extend(encode(OpCode::Ret, &[local(0)]));
        let mut owned = ContextBuilder::new().build().unwrap();
        let mut slots = vec![JitSlot::undefined()];
        let (mut budget, _) = DeterministicInterruptBudget::new(NonZeroU32::new(100).unwrap());
        with_bound_artifact(&mut owned, unsupported, 1, |context, binding, cache| {
            let loaded = cache.get(1).unwrap().unwrap();
            let (outcome, observation) =
                with_test_helper_behavior(TestHelperBehavior::Normal, || {
                    run_vm_contained(context, loaded, binding, &mut slots, &mut budget).unwrap()
                });
            assert_eq!(outcome, ContainedOutcome::UnsupportedAt(0));
            assert_eq!(observation.calls, 0);
        });

        let backedge = encode(OpCode::Jump, &[0]);
        let mut owned = ContextBuilder::new().build().unwrap();
        let mut slots = Vec::new();
        let (mut budget, _) = DeterministicInterruptBudget::new(NonZeroU32::new(100).unwrap());
        with_bound_artifact(&mut owned, backedge, 0, |context, binding, cache| {
            let loaded = cache.get(1).unwrap().unwrap();
            assert_eq!(
                run_vm_contained(context, loaded, binding, &mut slots, &mut budget).unwrap(),
                ContainedOutcome::InterruptedAt(0),
                "a pure native cycle must poll rather than reaching VM admission"
            );
        });
    }

    #[test]
    fn actual_vm_resume_executes_both_forward_cfg_paths_and_joins() {
        // NewObject is the generated-code prefix. Div is deliberately unsupported by this JIT
        // gate, so the remainder must execute in Brimstone's real VM.
        let mut bytes = encode(OpCode::NewObject, &[local(5), 0]); // 0
        let resume_offset = bytes.len();
        bytes.extend(encode(OpCode::Div, &[local(2), local(0), local(1)])); // 3
        bytes.extend(encode(OpCode::LessThan, &[local(3), local(2), local(4)])); // 7
        bytes.extend(encode(OpCode::JumpTrue, &[local(3), 8])); // 11 -> 19
        bytes.extend(encode(OpCode::LoadImmediate, &[local(2), 7])); // 14
        bytes.extend(encode(OpCode::Jump, &[5])); // 17 -> 22
        bytes.extend(encode(OpCode::LoadImmediate, &[local(2), 9])); // 19
        bytes.extend(encode(OpCode::Ret, &[local(2)])); // 22

        let mut owned = ContextBuilder::new().build().unwrap();
        let mut slots = vec![JitSlot::undefined(); 6];
        with_bound_artifact(&mut owned, bytes, 6, |context, binding, cache| {
            slots[1] = JitSlot::try_from_value(context, Value::raw_smi(2)).unwrap();
            slots[4] = JitSlot::try_from_value(context, Value::raw_smi(10)).unwrap();
            let loaded = cache.get(1).unwrap().unwrap();
            assert_eq!(loaded.program().instructions()[1].offset, resume_offset);

            for (left, expected) in [(16, 9), (24, 7)] {
                slots[0] = JitSlot::try_from_value(context, Value::raw_smi(left)).unwrap();
                let (mut budget, _) =
                    DeterministicInterruptBudget::new(NonZeroU32::new(100).unwrap());
                let (outcome, helper) =
                    with_test_helper_behavior(TestHelperBehavior::Normal, || {
                        run_vm_contained(context, loaded, binding, &mut slots, &mut budget).unwrap()
                    });
                assert_eq!(
                    expect_vm_returned(context, outcome),
                    Value::raw_smi(expected).as_raw_bits()
                );
                assert_eq!(helper.calls, 1);
                assert!(!context.has_registered_jit_frame());
                context.raw().vm().debug_assert_stack_empty();
            }
        });
    }

    #[test]
    fn native_loop_runs_allocating_safepoints_across_forced_moving_gc() {
        let mut bytes = encode(OpCode::LoadImmediate, &[local(0), 0]); // 0
        bytes.extend(encode(OpCode::LoadImmediate, &[local(1), 4])); // 3
        let loop_offset = bytes.len();
        bytes.extend(encode(OpCode::NewObject, &[local(3), 0])); // 6
        bytes.extend(encode(OpCode::AddImm, &[local(0), local(0), 1])); // 9
        bytes.extend(encode(OpCode::LessThan, &[local(2), local(0), local(1)])); // 13
        let branch_offset = bytes.len();
        bytes.extend(encode(
            OpCode::JumpTrue,
            &[
                local(2),
                (loop_offset as isize - branch_offset as isize) as i8 as u8,
            ],
        )); // 17 -> 6
        bytes.extend(encode(OpCode::Mov, &[local(5), local(4)])); // 20
        bytes.extend(encode(OpCode::Ret, &[local(0)])); // 23

        let mut owned = ContextBuilder::new().build().unwrap();
        let mut slots = vec![JitSlot::undefined(); 6];
        let (mut budget, _) = DeterministicInterruptBudget::new(NonZeroU32::new(100).unwrap());
        with_bound_artifact(&mut owned, bytes, 6, |context, binding, cache| {
            let (persistent, _) =
                unrooted_string_pair(context, "live across every native safepoint", "dead peer");
            slots[4] = persistent;
            let persistent_before = persistent.value().as_raw_bits();
            let loaded = cache.get(1).unwrap().unwrap();
            assert_eq!(loaded.program().instructions()[2].offset, loop_offset);
            assert_eq!(loaded.safepoints().records().len(), 1);
            assert_eq!(loaded.safepoints().live_slots(), &[0, 1, 4, 6]);
            let ((outcome, helper), polls) =
                with_test_backedge_poll_behavior(TestBackedgePollBehavior::Normal, || {
                    with_test_helper_behavior(
                        TestHelperBehavior::ForceCollectionAfterAllocation,
                        || {
                            run_vm_contained(context, loaded, binding, &mut slots, &mut budget)
                                .unwrap()
                        },
                    )
                });
            assert_eq!(expect_native_returned(context, outcome), Value::raw_smi(4).as_raw_bits());
            assert_eq!(helper.calls, 4, "one allocating helper per loop iteration");
            assert_eq!(polls.calls, 0, "ordinary taken backedges stay in generated code");
            assert_ne!(slots[4].value().as_raw_bits(), persistent_before);
            assert_eq!(slots[5].value().as_raw_bits(), slots[4].value().as_raw_bits());
            assert!(slots[4].value().is::<StringValue>());
            assert!(slots[3].value().is_object());
            assert!(!context.has_registered_jit_frame());
            context.raw().vm().debug_assert_stack_empty();
        });
    }

    #[test]
    fn native_backedges_distinguish_side_exit_interrupt_failure_panic_and_recovery() {
        let mut bytes = encode(OpCode::LoadImmediate, &[local(0), 0]);
        bytes.extend(encode(OpCode::LoadImmediate, &[local(1), 4]));
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

        let mut owned = ContextBuilder::new().build().unwrap();
        let mut slots = [JitSlot::undefined(); 3];
        with_bound_artifact(&mut owned, bytes, 3, |context, binding, cache| {
            let loaded = cache.get(1).unwrap().unwrap();

            let (mut side_exit_budget, _) =
                DeterministicInterruptBudget::new(NonZeroU32::new(100).unwrap());
            let (outcome, polls) =
                with_test_backedge_poll_behavior(TestBackedgePollBehavior::PolicySideExit, || {
                    run_vm_contained(context, loaded, binding, &mut slots, &mut side_exit_budget)
                        .unwrap()
                });
            assert_eq!(expect_vm_returned(context, outcome), Value::raw_smi(4).as_raw_bits());
            assert_eq!(polls.calls, 1);
            assert_eq!(slots[0].value().as_raw_bits(), Value::raw_smi(1).as_raw_bits());
            assert_eq!(slots[2].value().as_raw_bits(), Value::bool(true).as_raw_bits());
            context.raw().vm().debug_assert_stack_empty();

            for behavior in [
                TestBackedgePollBehavior::PolicyFailure,
                TestBackedgePollBehavior::Panic,
            ] {
                let (mut failed_budget, _) =
                    DeterministicInterruptBudget::new(NonZeroU32::new(100).unwrap());
                let (outcome, polls) = with_test_backedge_poll_behavior(behavior, || {
                    run_vm_contained(context, loaded, binding, &mut slots, &mut failed_budget)
                        .unwrap()
                });
                assert_eq!(outcome, ContainedOutcome::PoisonedAt(loop_offset));
                assert_eq!(polls.calls, 1);
                assert_eq!(slots[0].value().as_raw_bits(), Value::raw_smi(1).as_raw_bits());
                assert!(!context.has_registered_jit_frame());
                context.raw().vm().debug_assert_stack_empty();

                let (mut recovery_budget, _) =
                    DeterministicInterruptBudget::new(NonZeroU32::new(100).unwrap());
                let (recovered, recovery_polls) =
                    with_test_backedge_poll_behavior(TestBackedgePollBehavior::Normal, || {
                        run_vm_contained(context, loaded, binding, &mut slots, &mut recovery_budget)
                            .unwrap()
                    });
                assert_eq!(
                    expect_native_returned(context, recovered),
                    Value::raw_smi(4).as_raw_bits()
                );
                assert_eq!(recovery_polls.calls, 0);
                context.raw().vm().debug_assert_stack_empty();
            }

            let (mut quantum_budget, _) =
                DeterministicInterruptBudget::new(NonZeroU32::new(1).unwrap());
            let (quantum_outcome, quantum_polls) =
                with_test_backedge_poll_behavior(TestBackedgePollBehavior::Normal, || {
                    run_vm_contained(context, loaded, binding, &mut slots, &mut quantum_budget)
                        .unwrap()
                });
            assert_eq!(quantum_outcome, ContainedOutcome::InterruptedAt(loop_offset));
            assert_eq!(quantum_polls.calls, 1);
            assert_eq!(slots[0].value().as_raw_bits(), Value::raw_smi(1).as_raw_bits());

            let (mut requested_budget, request) =
                DeterministicInterruptBudget::new(NonZeroU32::new(100).unwrap());
            request.request();
            let (requested_outcome, requested_polls) =
                with_test_backedge_poll_behavior(TestBackedgePollBehavior::Normal, || {
                    run_vm_contained(context, loaded, binding, &mut slots, &mut requested_budget)
                        .unwrap()
                });
            assert_eq!(requested_outcome, ContainedOutcome::InterruptedAt(loop_offset));
            assert_eq!(requested_polls.calls, 1);
            assert_eq!(slots[0].value().as_raw_bits(), Value::raw_smi(1).as_raw_bits());

            let (mut final_budget, _) =
                DeterministicInterruptBudget::new(NonZeroU32::new(100).unwrap());
            let (final_outcome, final_polls) =
                with_test_backedge_poll_behavior(TestBackedgePollBehavior::Normal, || {
                    run_vm_contained(context, loaded, binding, &mut slots, &mut final_budget)
                        .unwrap()
                });
            assert_eq!(
                expect_native_returned(context, final_outcome),
                Value::raw_smi(4).as_raw_bits()
            );
            assert_eq!(final_polls.calls, 0);
            assert!(!context.has_registered_jit_frame());
            context.raw().vm().debug_assert_stack_empty();
        });
    }

    #[test]
    fn resumed_backedges_distinguish_interrupts_policy_failure_and_recovery() {
        // Div guarantees an immediate native side exit. The actual VM then performs three loop
        // iterations, polling only after each taken backward branch at offset 12.
        let mut bytes = encode(OpCode::Div, &[local(0), local(0), local(2)]); // 0
        let loop_offset = bytes.len();
        bytes.extend(encode(OpCode::AddImm, &[local(0), local(0), 1])); // 4
        bytes.extend(encode(OpCode::LessThan, &[local(3), local(0), local(1)])); // 8
        let branch_offset = bytes.len();
        bytes.extend(encode(
            OpCode::JumpTrue,
            &[
                local(3),
                (loop_offset as isize - branch_offset as isize) as i8 as u8,
            ],
        )); // 12 -> 4
        bytes.extend(encode(OpCode::Ret, &[local(0)])); // 15

        let mut owned = ContextBuilder::new().build().unwrap();
        let mut slots = vec![JitSlot::undefined(); 4];
        with_bound_artifact(&mut owned, bytes, 4, |context, binding, cache| {
            slots[0] = JitSlot::try_from_value(context, Value::raw_smi(0)).unwrap();
            slots[1] = JitSlot::try_from_value(context, Value::raw_smi(3)).unwrap();
            slots[2] = JitSlot::try_from_value(context, Value::raw_smi(1)).unwrap();
            let loaded = cache.get(1).unwrap().unwrap();

            let (mut quantum_budget, _) =
                DeterministicInterruptBudget::new(NonZeroU32::new(1).unwrap());
            assert_eq!(
                run_vm_contained(context, loaded, binding, &mut slots, &mut quantum_budget)
                    .unwrap(),
                ContainedOutcome::VmInterruptedAt(branch_offset)
            );
            context.raw().vm().debug_assert_stack_empty();

            let (mut requested_budget, request) =
                DeterministicInterruptBudget::new(NonZeroU32::new(100).unwrap());
            request.request();
            assert_eq!(
                run_vm_contained(context, loaded, binding, &mut slots, &mut requested_budget)
                    .unwrap(),
                ContainedOutcome::VmInterruptedAt(branch_offset)
            );
            context.raw().vm().debug_assert_stack_empty();

            let (mut failed_budget, _) =
                DeterministicInterruptBudget::new(NonZeroU32::new(100).unwrap());
            let failed = with_test_jit_resume_interrupt_policy_failure(|| {
                run_vm_contained(context, loaded, binding, &mut slots, &mut failed_budget)
            });
            assert!(matches!(
                failed,
                Err(ContainedRunError::VmSetup(
                    JitResumeSetupError::InterruptPolicyFailedAt(offset)
                )) if offset == branch_offset
            ));
            context.raw().vm().debug_assert_stack_empty();

            let (mut recovery_budget, _) =
                DeterministicInterruptBudget::new(NonZeroU32::new(100).unwrap());
            let outcome =
                run_vm_contained(context, loaded, binding, &mut slots, &mut recovery_budget)
                    .unwrap();
            assert_eq!(expect_vm_returned(context, outcome), Value::raw_smi(3).as_raw_bits());
            assert!(!context.has_registered_jit_frame());
            context.raw().vm().debug_assert_stack_empty();
        });
    }

    #[test]
    fn rooted_constant_backedge_resumes_in_actual_vm_and_polls() {
        let mut bytes = encode(OpCode::Div, &[local(0), local(0), local(2)]); // 0
        let loop_offset = bytes.len();
        bytes.extend(encode(OpCode::AddImm, &[local(0), local(0), 1])); // 4
        bytes.extend(encode(OpCode::LessThan, &[local(3), local(0), local(1)])); // 8
        let branch_offset = bytes.len();
        bytes.extend(encode(OpCode::JumpTrueConstant, &[local(3), 0])); // 12 -> 4
        bytes.extend(encode(OpCode::Ret, &[local(0)])); // 15

        let mut owned = ContextBuilder::new().build().unwrap();
        owned.with_jit_context(|context| {
            let table = raw_jump_table(context, &[loop_offset as isize - branch_offset as isize]);
            let closure = make_test_closure_with_metadata(context, bytes, Some(table), 4, 0);
            let (binding, prepared) = prepare_vm_prototype(context, closure).unwrap();
            assert_eq!(prepared.program().instructions()[3].branch_target, Some(loop_offset));
            let mapped_len = ExecutableCodeCache::mapped_len_for(&prepared).unwrap();
            let mut cache = ExecutableCodeCache::new(mapped_len, 1).unwrap();
            cache.insert(1, prepared).unwrap();
            let loaded = cache.get(1).unwrap().unwrap();

            let mut slots = vec![JitSlot::undefined(); 4];
            slots[0] = JitSlot::try_from_value(context, Value::raw_smi(0)).unwrap();
            slots[1] = JitSlot::try_from_value(context, Value::raw_smi(3)).unwrap();
            slots[2] = JitSlot::try_from_value(context, Value::raw_smi(1)).unwrap();
            let (mut budget, _) = DeterministicInterruptBudget::new(NonZeroU32::new(100).unwrap());
            let outcome =
                run_vm_contained(context, loaded, &binding, &mut slots, &mut budget).unwrap();
            assert_eq!(expect_vm_returned(context, outcome), Value::raw_smi(3).as_raw_bits());
            assert!(!context.has_registered_jit_frame());
            context.raw().vm().debug_assert_stack_empty();
        });
    }

    #[test]
    fn internal_heap_allocations_are_rejected_before_vm_frame_or_handoff_gc() {
        let mut bytes = encode(OpCode::NewObject, &[local(2), 0]);
        let resume_offset = bytes.len();
        bytes.extend(encode(OpCode::DivImm, &[local(1), local(0), (-1_i8) as u8]));
        bytes.extend(encode(OpCode::Ret, &[local(1)]));

        let mut owned = ContextBuilder::new().build().unwrap();
        owned.with_jit_context(|context| {
            let closure = make_test_closure(context, bytes, 3);
            let (binding, prepared) = prepare_vm_prototype(context, closure).unwrap();
            let mapped_len = ExecutableCodeCache::mapped_len_for(&prepared).unwrap();
            let mut cache = ExecutableCodeCache::new(mapped_len, 1).unwrap();
            cache.insert(1, prepared).unwrap();
            let loaded = cache.get(1).unwrap().unwrap();

            let function_value = Value::from_raw_bits((*binding.function).as_ptr() as usize as u64);
            let mut slots = vec![JitSlot::undefined(); 3];
            slots[0] = JitSlot::try_from_value(context, function_value).unwrap();
            let (mut budget, _) = DeterministicInterruptBudget::new(NonZeroU32::new(100).unwrap());
            let ((result, helper), handoff) = with_test_handoff_collections(
                TestHandoffGcBehavior { before_vm_frame: true, after_vm_frame: true },
                || {
                    with_test_helper_behavior(TestHelperBehavior::Normal, || {
                        run_vm_contained(context, loaded, &binding, &mut slots, &mut budget)
                    })
                },
            );
            assert!(matches!(
                result,
                Err(ContainedRunError::Resume(VmResumeError::InternalValueConsumed {
                    offset,
                    operand: 1,
                    local: 0,
                })) if offset == resume_offset
            ));
            assert_eq!(helper.calls, 1);
            assert_eq!(handoff, TestHandoffGcObservation::default());
            assert!(!context.has_registered_jit_frame());
            context.raw().vm().debug_assert_stack_empty();

            // Rejection did not poison the context or cache entry.
            slots[0] = JitSlot::try_from_value(context, Value::raw_smi(2)).unwrap();
            let outcome =
                run_vm_contained(context, loaded, &binding, &mut slots, &mut budget).unwrap();
            assert_eq!(expect_vm_returned(context, outcome), Value::raw_smi(-2).as_raw_bits());
        });
    }

    #[test]
    fn native_return_defers_empty_and_unproven_pointers_to_vm_admission() {
        let mut loaded_empty = encode(OpCode::LoadEmpty, &[local(0)]);
        let loaded_ret_offset = loaded_empty.len();
        loaded_empty.extend(encode(OpCode::Ret, &[local(0)]));
        let mut owned = ContextBuilder::new().build().unwrap();
        let mut loaded_slots = [JitSlot::undefined()];
        with_bound_artifact(&mut owned, loaded_empty, 1, |context, binding, cache| {
            let loaded = cache.get(1).unwrap().unwrap();
            let (mut budget, _) = DeterministicInterruptBudget::new(NonZeroU32::new(100).unwrap());
            assert!(matches!(
                run_vm_contained(
                    context,
                    loaded,
                    binding,
                    &mut loaded_slots,
                    &mut budget,
                ),
                Err(ContainedRunError::Resume(VmResumeError::EmptyValueConsumed {
                    offset,
                    operand: 0,
                    local: 0,
                })) if offset == loaded_ret_offset
            ));
            context.raw().vm().debug_assert_stack_empty();
        });

        let direct_ret = encode(OpCode::Ret, &[local(0)]);
        let mut owned = ContextBuilder::new().build().unwrap();
        let mut slots = [JitSlot::undefined()];
        with_bound_artifact(&mut owned, direct_ret, 1, |context, binding, cache| {
            let loaded = cache.get(1).unwrap().unwrap();
            let (mut budget, _) = DeterministicInterruptBudget::new(NonZeroU32::new(100).unwrap());

            slots[0] = JitSlot::try_from_value(context, Value::empty()).unwrap();
            assert!(matches!(
                run_vm_contained(context, loaded, binding, &mut slots, &mut budget),
                Err(ContainedRunError::Resume(VmResumeError::EmptyValueConsumed {
                    offset: 0,
                    operand: 0,
                    local: 0,
                }))
            ));

            let function_value = Value::from_raw_bits((*binding.function).as_ptr() as usize as u64);
            slots[0] = JitSlot::try_from_value(context, function_value).unwrap();
            assert!(matches!(
                run_vm_contained(context, loaded, binding, &mut slots, &mut budget),
                Err(ContainedRunError::Resume(VmResumeError::InternalValueConsumed {
                    offset: 0,
                    operand: 0,
                    local: 0,
                }))
            ));

            let (string, _) = unrooted_string_pair(context, "valid JS pointer return", "peer");
            slots[0] = string;
            let expected = string.value().as_raw_bits();
            let outcome =
                run_vm_contained(context, loaded, binding, &mut slots, &mut budget).unwrap();
            assert_eq!(expect_vm_returned(context, outcome), expected);
            assert!(!context.has_registered_jit_frame());
            context.raw().vm().debug_assert_stack_empty();
        });
    }

    #[test]
    fn native_undefined_branch_defers_unproven_pointer_and_empty_values_to_vm_admission() {
        let mut direct_branch = encode(OpCode::JumpNotUndefined, &[local(0), 8]); // 0 -> 8
        direct_branch.extend(encode(OpCode::LoadImmediate, &[local(1), 2])); // 3
        direct_branch.extend(encode(OpCode::Jump, &[5])); // 6 -> 11
        direct_branch.extend(encode(OpCode::LoadImmediate, &[local(1), 7])); // 8
        direct_branch.extend(encode(OpCode::Ret, &[local(1)])); // 11

        let mut owned = ContextBuilder::new().build().unwrap();
        let mut slots = [JitSlot::undefined(), JitSlot::undefined()];
        with_bound_artifact(&mut owned, direct_branch, 2, |context, binding, cache| {
            let loaded = cache.get(1).unwrap().unwrap();
            let (mut budget, _) = DeterministicInterruptBudget::new(NonZeroU32::new(100).unwrap());

            slots[0] = JitSlot::try_from_value(context, Value::empty()).unwrap();
            assert!(matches!(
                run_vm_contained(context, loaded, binding, &mut slots, &mut budget),
                Err(ContainedRunError::Resume(VmResumeError::EmptyValueConsumed {
                    offset: 0,
                    operand: 0,
                    local: 0,
                }))
            ));

            let function_value = Value::from_raw_bits((*binding.function).as_ptr() as usize as u64);
            slots[0] = JitSlot::try_from_value(context, function_value).unwrap();
            assert!(matches!(
                run_vm_contained(context, loaded, binding, &mut slots, &mut budget),
                Err(ContainedRunError::Resume(VmResumeError::InternalValueConsumed {
                    offset: 0,
                    operand: 0,
                    local: 0,
                }))
            ));

            let (string, _) = unrooted_string_pair(context, "valid branch value", "peer");
            slots[0] = string;
            let outcome =
                run_vm_contained(context, loaded, binding, &mut slots, &mut budget).unwrap();
            assert_eq!(expect_vm_returned(context, outcome), Value::raw_smi(7).as_raw_bits());
            assert!(!context.has_registered_jit_frame());
            context.raw().vm().debug_assert_stack_empty();
        });

        // A compiler-proven JavaScript pointer may stay native. The allocating helper moves the
        // object before the branch, proving that static provenance never substitutes for the
        // shadow-frame rewrite required at the safepoint.
        let mut proven_branch = encode(OpCode::NewObject, &[local(0), 0]); // 0
        proven_branch.extend(encode(OpCode::JumpNotUndefined, &[local(0), 8])); // 3 -> 11
        proven_branch.extend(encode(OpCode::LoadImmediate, &[local(1), 2])); // 6
        proven_branch.extend(encode(OpCode::Jump, &[5])); // 9 -> 14
        proven_branch.extend(encode(OpCode::LoadImmediate, &[local(1), 7])); // 11
        proven_branch.extend(encode(OpCode::Ret, &[local(1)])); // 14

        let mut owned = ContextBuilder::new().build().unwrap();
        let mut slots = [JitSlot::undefined(), JitSlot::undefined()];
        with_bound_artifact(&mut owned, proven_branch, 2, |context, binding, cache| {
            let loaded = cache.get(1).unwrap().unwrap();
            let (mut budget, _) = DeterministicInterruptBudget::new(NonZeroU32::new(100).unwrap());
            let (outcome, helper) = with_test_helper_behavior(
                TestHelperBehavior::ForceCollectionAfterAllocation,
                || run_vm_contained(context, loaded, binding, &mut slots, &mut budget),
            );
            assert_eq!(helper.calls, 1);
            assert_eq!(
                expect_native_returned(context, outcome.unwrap()),
                Value::raw_smi(7).as_raw_bits()
            );
            assert!(!context.has_registered_jit_frame());
            context.raw().vm().debug_assert_stack_empty();
        });
    }

    #[test]
    fn abstract_cfg_allows_dead_empty_moves_but_rejects_every_consuming_path() {
        let mut dead_empty = encode(OpCode::LoadEmpty, &[local(0)]);
        dead_empty.extend(encode(OpCode::Mov, &[local(1), local(0)]));
        dead_empty.extend(encode(OpCode::LoadImmediate, &[local(1), 5]));
        dead_empty.extend(encode(OpCode::Ret, &[local(1)]));
        let verified =
            VerifiedBytecode::verify(&dead_empty, VerificationLimits::empty(2, 0)).unwrap();
        let prepared = compile_prototype(&verified).unwrap();
        assert_eq!(
            validate_resume_policy(
                prepared.program(),
                0,
                &[
                    JitSlot::undefined(),
                    JitSlot::undefined(),
                    JitSlot::undefined(),
                ],
            ),
            Ok(())
        );

        let mut consumed = encode(OpCode::LoadEmpty, &[local(0)]);
        consumed.extend(encode(OpCode::Mov, &[local(1), local(0)]));
        let ret_offset = consumed.len();
        consumed.extend(encode(OpCode::Ret, &[local(1)]));
        let verified =
            VerifiedBytecode::verify(&consumed, VerificationLimits::empty(2, 0)).unwrap();
        let prepared = compile_prototype(&verified).unwrap();
        assert_eq!(
            validate_resume_policy(
                prepared.program(),
                0,
                &[
                    JitSlot::undefined(),
                    JitSlot::undefined(),
                    JitSlot::undefined(),
                ],
            ),
            Err(VmResumeError::EmptyValueConsumed { offset: ret_offset, operand: 0, local: 1 })
        );

        // Analysis visits both successors even though this particular bytecode loads true.
        let mut joined = encode(OpCode::LoadTrue, &[local(1)]); // 0
        joined.extend(encode(OpCode::JumpTrue, &[local(1), 7])); // 2 -> 9
        joined.extend(encode(OpCode::LoadEmpty, &[local(0)])); // 5
        joined.extend(encode(OpCode::Jump, &[5])); // 7 -> 12
        joined.extend(encode(OpCode::LoadImmediate, &[local(0), 1])); // 9
        let add_offset = joined.len();
        joined.extend(encode(OpCode::AddImm, &[local(0), local(0), 1])); // 12
        joined.extend(encode(OpCode::Ret, &[local(0)]));
        let verified = VerifiedBytecode::verify(&joined, VerificationLimits::empty(2, 0)).unwrap();
        let prepared = compile_prototype(&verified).unwrap();
        assert_eq!(
            validate_resume_policy(
                prepared.program(),
                0,
                &[
                    JitSlot::undefined(),
                    JitSlot::undefined(),
                    JitSlot::undefined(),
                ],
            ),
            Err(VmResumeError::EmptyValueConsumed { offset: add_offset, operand: 1, local: 0 })
        );
    }

    #[test]
    fn exact_boolean_branch_rejects_number_proof() {
        let mut bytes = encode(OpCode::LoadImmediate, &[local(0), 1]); // 0
        let branch_offset = bytes.len();
        bytes.extend(encode(OpCode::JumpTrue, &[local(0), 5])); // 3 -> 8
        bytes.extend(encode(OpCode::Ret, &[local(0)])); // 6
        bytes.extend(encode(OpCode::Ret, &[local(0)])); // 8
        let verified = VerifiedBytecode::verify(&bytes, VerificationLimits::empty(1, 0)).unwrap();
        let prepared = compile_prototype(&verified).unwrap();
        assert_eq!(
            validate_resume_policy(
                prepared.program(),
                0,
                &[JitSlot::undefined(), JitSlot::undefined()],
            ),
            Err(VmResumeError::NonBooleanCondition { offset: branch_offset, local: 0 })
        );
    }

    #[test]
    fn wide_and_extra_wide_loops_resume_from_prefix_start() {
        use crate::runtime::bytecode::WidthEnum;

        for (width, counter, limit, condition, num_locals) in [
            (WidthEnum::Wide, -129, -130, -131, 131),
            (WidthEnum::ExtraWide, -65_537, -65_538, -65_539, 65_539),
        ] {
            let mut bytes = Vec::new();
            append_width_encoded(&mut bytes, OpCode::DivImm, &[counter, counter, 1], width);
            append_width_encoded(&mut bytes, OpCode::LoadImmediate, &[limit, 3], width);
            let loop_offset = bytes.len();
            append_width_encoded(&mut bytes, OpCode::AddImm, &[counter, counter, 1], width);
            append_width_encoded(&mut bytes, OpCode::LessThan, &[condition, counter, limit], width);
            let branch_offset = bytes.len();
            append_width_encoded(
                &mut bytes,
                OpCode::JumpTrue,
                &[condition, loop_offset as i32 - branch_offset as i32],
                width,
            );
            append_width_encoded(&mut bytes, OpCode::Ret, &[counter], width);

            let mut owned = ContextBuilder::new().build().unwrap();
            let mut slots = vec![JitSlot::undefined(); num_locals];
            let (mut budget, _) = DeterministicInterruptBudget::new(NonZeroU32::new(100).unwrap());
            with_bound_artifact(&mut owned, bytes, num_locals, |context, binding, cache| {
                let counter_index = usize::try_from(-1_i32 - counter).unwrap();
                slots[counter_index] = JitSlot::try_from_value(context, Value::raw_smi(0)).unwrap();
                let loaded = cache.get(1).unwrap().unwrap();
                assert_eq!(loaded.program().instructions()[0].offset, 0);
                assert_eq!(loaded.program().instructions()[2].offset, loop_offset);
                assert_eq!(loaded.program().instructions()[4].offset, branch_offset);
                let (outcome, handoff) = with_test_handoff_collections(
                    TestHandoffGcBehavior { before_vm_frame: true, after_vm_frame: true },
                    || run_vm_contained(context, loaded, binding, &mut slots, &mut budget).unwrap(),
                );
                assert_eq!(expect_vm_returned(context, outcome), Value::raw_smi(3).as_raw_bits());
                assert!(handoff.before_vm_frame.is_some());
                assert!(handoff.after_vm_frame.is_some());
                assert!(!context.has_registered_jit_frame());
                context.raw().vm().debug_assert_stack_empty();
            });
        }
    }

    #[test]
    fn activation_unlinks_before_executable_cache_eviction() {
        let mut bytes = encode(OpCode::NewObject, &[local(0), 0]);
        bytes.extend(encode(OpCode::Ret, &[local(0)]));
        let replacement = bytes.clone();
        let mut owned = ContextBuilder::new().build().unwrap();
        let mut slots = vec![JitSlot::undefined()];
        let (mut budget, _) = DeterministicInterruptBudget::new(NonZeroU32::new(100).unwrap());
        with_bound_artifact(&mut owned, bytes, 1, |context, binding, cache| {
            let loaded = cache.get(1).unwrap().unwrap();
            assert!(matches!(
                run_vm_contained(context, loaded, binding, &mut slots, &mut budget).unwrap(),
                ContainedOutcome::NativeReturned(_)
            ));
            assert!(!context.has_registered_jit_frame());

            let verified =
                VerifiedBytecode::verify(&replacement, VerificationLimits::empty(1, 0)).unwrap();
            cache
                .insert(2, compile_prototype(&verified).unwrap())
                .unwrap();
            assert!(cache.get(1).unwrap().is_none());
            assert!(cache.get(2).unwrap().is_some());
        });
    }

    #[test]
    fn hot_preflight_models_native_new_object_without_admitting_it_to_vm_resume() {
        let mut native_prefix = encode(OpCode::NewObject, &[local(0), 0]);
        native_prefix.extend(encode(OpCode::Ret, &[local(0)]));
        let verified =
            VerifiedBytecode::verify(&native_prefix, VerificationLimits::empty(1, 0)).unwrap();
        let prepared = compile_prototype(&verified).unwrap();
        let slots = [JitSlot::undefined(), JitSlot::undefined()];
        assert_eq!(validate_hot_entry_policy(prepared.program(), &slots), Ok(()));
        assert_eq!(
            validate_resume_policy(prepared.program(), 0, &slots),
            Err(VmResumeError::UnsupportedAt(0))
        );

        let mut after_possible_exit = encode(OpCode::Mov, &[local(0), local(1)]);
        let new_object_offset = after_possible_exit.len();
        after_possible_exit.extend(encode(OpCode::NewObject, &[local(1), 0]));
        after_possible_exit.extend(encode(OpCode::Ret, &[local(1)]));
        let verified =
            VerifiedBytecode::verify(&after_possible_exit, VerificationLimits::empty(2, 0))
                .unwrap();
        let prepared = compile_prototype(&verified).unwrap();
        let slots = [
            JitSlot::undefined(),
            JitSlot::undefined(),
            JitSlot::undefined(),
        ];
        assert_eq!(
            validate_hot_entry_policy(prepared.program(), &slots),
            Err(VmResumeError::UnsupportedAt(new_object_offset))
        );

        // A taken branch can exhaust the native-residency cap and resume at its target. A loop
        // which reaches the allocating prefix again must therefore be rejected before entry.
        let mut allocating_loop = encode(OpCode::NewObject, &[local(0), 0]); // 0
        allocating_loop.extend(encode(OpCode::LoadTrue, &[local(1)])); // 3
        let branch_offset = allocating_loop.len();
        allocating_loop
            .extend(encode(OpCode::JumpTrue, &[local(1), (-(branch_offset as isize)) as i8 as u8]));
        allocating_loop.extend(encode(OpCode::Ret, &[local(0)]));
        let verified =
            VerifiedBytecode::verify(&allocating_loop, VerificationLimits::empty(2, 0)).unwrap();
        let prepared = compile_prototype(&verified).unwrap();
        let slots = [
            JitSlot::undefined(),
            JitSlot::undefined(),
            JitSlot::undefined(),
        ];
        assert_eq!(
            validate_hot_entry_policy(prepared.program(), &slots),
            Err(VmResumeError::UnsupportedAt(0))
        );
    }

    #[test]
    fn resume_policy_requires_exact_boundary_and_terminal_shape() {
        let mut bytes = encode(OpCode::Neg, &[local(1), local(0)]);
        bytes.extend(encode(OpCode::Ret, &[local(1)]));
        let verified = VerifiedBytecode::verify(&bytes, VerificationLimits::empty(2, 0)).unwrap();
        let prepared = compile_prototype(&verified).unwrap();
        let mut owned = ContextBuilder::new().build().unwrap();
        owned.with_jit_context(|context| {
            let slots = [
                JitSlot::try_from_value(context, Value::raw_smi(1)).unwrap(),
                JitSlot::undefined(),
                JitSlot::undefined(),
            ];
            assert_eq!(validate_resume_policy(prepared.program(), 0, &slots), Ok(()));
            assert_eq!(
                validate_resume_policy(prepared.program(), 1, &slots),
                Err(VmResumeError::InvalidBytecodeOffset(1))
            );
        });

        let mut unsupported = encode(OpCode::ToNumber, &[local(1), local(0)]);
        unsupported.extend(encode(OpCode::Ret, &[local(1)]));
        let verified =
            VerifiedBytecode::verify(&unsupported, VerificationLimits::empty(2, 0)).unwrap();
        let prepared = compile_prototype(&verified).unwrap();
        assert_eq!(
            validate_resume_policy(
                prepared.program(),
                0,
                &[
                    JitSlot::undefined(),
                    JitSlot::undefined(),
                    JitSlot::undefined(),
                ],
            ),
            Err(VmResumeError::UnsupportedAt(0))
        );
    }
}
