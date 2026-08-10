//! Context-owned, bounded hot-call dispatch for the disabled baseline tier.
//!
//! Product admission remains compile-time false. Tests may change only policy admission and
//! thresholds; identity assignment, compilation, cache ownership, preflight, native entry, and
//! terminal handling are the same algorithm used by a future product caller.

use std::num::NonZeroU32;

use crate::runtime::{
    Handle, HeapPtr, JitContextScope, Value,
    bytecode::{
        function::{BytecodeFunction, ClosureObject},
        vm::VM,
    },
    gc::HeapVisitor,
    jit::{
        PRODUCT_DISPATCH_ENABLED,
        abi::JitSlot,
        code_cache::{CodeMemoryError, ExecutableCodeCache},
        compiler::{VmBindingId, allocate_vm_binding_id},
        continuation::{
            ContainedOutcome, HotCallRunError, bind_vm_function_with_id,
            prepare_vm_prototype_with_id, run_vm_hot_call,
        },
        hotness::{
            DeterministicInterruptBudget, FunctionHotness, HotnessDecision, HotnessThresholds,
        },
    },
    object_value::ObjectValue,
};

const DISPATCH_CODE_CAPACITY_BYTES: usize = 8 * 1024 * 1024;
const DISPATCH_MAX_ENTRIES: usize = 32;

/// Generation-checked identity for one sibling GC-root slot.
///
/// Dispatch owns these opaque IDs, never a moving heap pointer. This is the structural boundary
/// which permits GC to visit `BaselineDispatchRoots` while `BaselineDispatchState` is mutably
/// borrowed by an active call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DispatchRootSlotId {
    index: u8,
    generation: u64,
}

struct DispatchRootSlot {
    generation: u64,
    function: Option<HeapPtr<BytecodeFunction>>,
    /// Exact value-constant identities for one loaded artifact. Raw jump entries are `None`.
    /// This storage is deliberately separate from pointer-free `PreparedProgram` descriptors.
    constants: Option<Box<[Option<Value>]>>,
}

/// Bounded moving-GC roots stored beside, rather than inside, the mutably active dispatcher.
pub(crate) struct BaselineDispatchRoots {
    slots: [DispatchRootSlot; DISPATCH_MAX_ENTRIES],
    next_generation: u64,
}

impl BaselineDispatchRoots {
    pub(crate) fn new() -> Self {
        Self {
            slots: std::array::from_fn(|_| DispatchRootSlot {
                generation: 0,
                function: None,
                constants: None,
            }),
            next_generation: 1,
        }
    }

    pub(crate) fn visit_roots(&mut self, visitor: &mut impl HeapVisitor) {
        for slot in &mut self.slots {
            if let Some(function) = &mut slot.function {
                visitor.visit_pointer(function);
            }
            if let Some(constants) = &mut slot.constants {
                for value in constants.iter_mut().flatten() {
                    visitor.visit_value(value);
                }
            }
        }
    }

    fn allocate(&mut self, function: HeapPtr<BytecodeFunction>) -> Option<DispatchRootSlotId> {
        let index = self.slots.iter().position(|slot| slot.function.is_none())?;
        let generation = self.next_generation;
        self.next_generation = self.next_generation.checked_add(1)?;
        self.slots[index] =
            DispatchRootSlot { generation, function: Some(function), constants: None };
        Some(DispatchRootSlotId { index: u8::try_from(index).ok()?, generation })
    }

    fn get(&self, id: DispatchRootSlotId) -> Option<HeapPtr<BytecodeFunction>> {
        let slot = self.slots.get(id.index as usize)?;
        (slot.generation == id.generation)
            .then_some(slot.function)
            .flatten()
    }

    fn clear(&mut self, id: DispatchRootSlotId) -> bool {
        let Some(slot) = self.slots.get_mut(id.index as usize) else {
            return false;
        };
        if slot.generation != id.generation || slot.function.is_none() {
            return false;
        }
        slot.function = None;
        slot.constants = None;
        true
    }

    fn install_constants(
        &mut self,
        id: DispatchRootSlotId,
        constants: Box<[Option<Value>]>,
    ) -> bool {
        let Some(slot) = self.slots.get_mut(id.index as usize) else {
            return false;
        };
        if slot.generation != id.generation || slot.function.is_none() || slot.constants.is_some() {
            return false;
        }
        slot.constants = Some(constants);
        true
    }

    fn constants(&self, id: DispatchRootSlotId) -> Option<&[Option<Value>]> {
        let slot = self.slots.get(id.index as usize)?;
        if slot.generation != id.generation || slot.function.is_none() {
            return None;
        }
        slot.constants.as_deref()
    }

    fn clear_constants(&mut self, id: DispatchRootSlotId) -> bool {
        let Some(slot) = self.slots.get_mut(id.index as usize) else {
            return false;
        };
        if slot.generation != id.generation || slot.function.is_none() {
            return false;
        }
        slot.constants.take().is_some()
    }

    fn is_empty(&self) -> bool {
        self.slots
            .iter()
            .all(|slot| slot.function.is_none() && slot.constants.is_none())
    }

    fn has_capacity(&self) -> bool {
        self.slots.iter().any(|slot| slot.function.is_none())
    }

    #[cfg(test)]
    fn live_count(&self) -> usize {
        self.slots
            .iter()
            .filter(|slot| slot.function.is_some())
            .count()
    }
}

#[derive(Clone, Copy)]
struct DispatchPolicy {
    enabled: bool,
    thresholds: HotnessThresholds,
    interrupt_quantum: NonZeroU32,
}

impl Default for DispatchPolicy {
    fn default() -> Self {
        Self {
            enabled: PRODUCT_DISPATCH_ENABLED,
            thresholds: HotnessThresholds {
                calls: NonZeroU32::new(100).expect("nonzero constant"),
                backedges: NonZeroU32::new(1_000).expect("nonzero constant"),
            },
            interrupt_quantum: NonZeroU32::new(100_000).expect("nonzero constant"),
        }
    }
}

struct DispatchEntry {
    id: VmBindingId,
    root: DispatchRootSlotId,
    hotness: FunctionHotness,
    last_used: u64,
    artifact_loaded: bool,
    rejected: bool,
    retire_when_unpinned: bool,
}

/// Raw completion copied while rooted and immediately re-rooted by the VM caller after the private
/// JIT handle scope closes. No allocation or collection may intervene.
pub(crate) enum HotDispatchAttempt {
    NotEntered,
    Completed(Result<Value, Value>),
    Terminal(HotDispatchTerminal),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HotDispatchTerminal {
    NativeInterrupted(usize),
    VmInterrupted(usize),
    NativeAllocationFailed(usize),
    VmAllocationFailed(usize),
    Poisoned(usize),
    InvalidActivation,
    PostEntryFailure,
    ImpossibleUnsupported(usize),
    CacheIncoherent,
}

pub(crate) struct BaselineDispatchState {
    policy: DispatchPolicy,
    cache: ExecutableCodeCache,
    entries: Vec<DispatchEntry>,
    entry_limit: usize,
    clock: u64,
    #[cfg(test)]
    compile_attempts: u32,
    #[cfg(test)]
    native_entries: u32,
    #[cfg(test)]
    clean_fallbacks: u32,
    #[cfg(test)]
    request_next_entry: bool,
    #[cfg(test)]
    request_next_nested_entry: bool,
    #[cfg(test)]
    collect_before_next_preentry_decision: bool,
}

impl BaselineDispatchState {
    pub(crate) fn new() -> Self {
        Self {
            policy: DispatchPolicy::default(),
            cache: ExecutableCodeCache::new(DISPATCH_CODE_CAPACITY_BYTES, DISPATCH_MAX_ENTRIES)
                .expect("fixed nonzero baseline cache limits"),
            entries: Vec::new(),
            entry_limit: DISPATCH_MAX_ENTRIES,
            clock: 0,
            #[cfg(test)]
            compile_attempts: 0,
            #[cfg(test)]
            native_entries: 0,
            #[cfg(test)]
            clean_fallbacks: 0,
            #[cfg(test)]
            request_next_entry: false,
            #[cfg(test)]
            request_next_nested_entry: false,
            #[cfg(test)]
            collect_before_next_preentry_decision: false,
        }
    }

    pub(crate) fn try_call<'scope>(
        &mut self,
        context: &mut JitContextScope<'scope>,
        vm: &mut VM,
        closure: Handle<ClosureObject>,
        receiver: Handle<Value>,
        arguments: &[Handle<Value>],
    ) -> HotDispatchAttempt {
        self.try_invoke(context, vm, closure, receiver, arguments, None)
    }

    pub(crate) fn try_construct<'scope>(
        &mut self,
        context: &mut JitContextScope<'scope>,
        vm: &mut VM,
        closure: Handle<ClosureObject>,
        receiver: Handle<Value>,
        arguments: &[Handle<Value>],
        new_target: Handle<ObjectValue>,
    ) -> HotDispatchAttempt {
        self.try_invoke(context, vm, closure, receiver, arguments, Some(new_target))
    }

    fn try_invoke<'scope>(
        &mut self,
        context: &mut JitContextScope<'scope>,
        vm: &mut VM,
        closure: Handle<ClosureObject>,
        receiver: Handle<Value>,
        arguments: &[Handle<Value>],
        new_target: Option<Handle<ObjectValue>>,
    ) -> HotDispatchAttempt {
        if !self.policy.enabled {
            return HotDispatchAttempt::NotEntered;
        }
        self.retire_deferred_rejections(context.raw());

        // Copy only immutable primitive metadata before any operation that may allocate or collect.
        // The raw moving pointer itself is used only for the immediate root-registry lookup below.
        let (num_parameters, num_registers, entry_index) = {
            let function = closure.function_ptr();
            let num_parameters = function.num_parameters();
            let num_registers = function.num_registers();
            if new_target.is_some() && !function.is_constructor()
                || function
                    .new_target_index()
                    .is_some_and(|index| index >= num_registers)
            {
                return self.clean_fallback();
            }
            let (current_realm, initial_realm) = vm.jit_dispatch_realms();
            if !function.realm_ptr().ptr_eq(&current_realm) || !current_realm.ptr_eq(&initial_realm)
            {
                // Native allocation helpers consult `Context::current_realm` before any callee VM
                // frame exists. This gate admits only the exact initial-realm caller/callee pair;
                // the ordinary interpreter fallback installs the callee frame before NewObject.
                return self.clean_fallback();
            }
            let entry_index = match self.find_or_insert_entry(context.raw(), function) {
                Some(index) => index,
                None => return self.clean_fallback(),
            };
            (num_parameters, num_registers, entry_index)
        };
        self.touch(entry_index);
        #[cfg(test)]
        if std::mem::take(&mut self.collect_before_next_preentry_decision) {
            use crate::runtime::gc::{GcType, Heap};

            // The enclosing call's closure/receiver/argument handles and the sibling function
            // root are all live here. This common point precedes both first-time compilation and
            // a rooted negative-cache fallback; no raw JIT slots have been copied.
            Heap::run_gc(context.raw(), GcType::Normal);
        }
        if self.entries[entry_index].rejected {
            return self.clean_fallback();
        }
        let hot = match self.entries[entry_index].hotness.record_call() {
            Ok(HotnessDecision::Cold) => return self.clean_fallback(),
            Ok(HotnessDecision::BecameHot | HotnessDecision::AlreadyHot) => true,
            Err(_) => false,
        };
        if !hot {
            return self.clean_fallback();
        }

        let id = self.entries[entry_index].id;
        let key = id.get();
        let expects_code = self.entries[entry_index].artifact_loaded;
        let has_code = match self.cache.contains_key(key) {
            Ok(true) if expects_code => true,
            Ok(false) if !expects_code => false,
            Ok(true) | Ok(false) => {
                return HotDispatchAttempt::Terminal(HotDispatchTerminal::CacheIncoherent);
            }
            Err(_) => return self.reject_cleanly(context.raw(), id),
        };

        let binding = if has_code {
            let binding = match bind_vm_function_with_id(context, closure, id) {
                Ok(binding) => binding,
                Err(_) => return self.reject_cleanly(context.raw(), id),
            };
            let constants_match = {
                let mut raw = context.raw();
                raw.jit_dispatch_roots()
                    .constants(self.entries[entry_index].root)
                    .is_some_and(|roots| binding.matches_dispatch_constant_roots(roots))
            };
            if !constants_match {
                return self.reject_cleanly(context.raw(), id);
            }
            binding
        } else {
            #[cfg(test)]
            {
                self.compile_attempts = self.compile_attempts.saturating_add(1);
            }
            let (binding, prepared) = match prepare_vm_prototype_with_id(context, closure, id) {
                Ok(compiled) => compiled,
                Err(_) => return self.reject_cleanly(context.raw(), id),
            };
            let constant_roots = match binding.capture_dispatch_constant_roots() {
                Ok(roots) => roots,
                Err(_) => return self.reject_cleanly(context.raw(), id),
            };
            if !context
                .raw()
                .jit_dispatch_roots()
                .install_constants(self.entries[entry_index].root, constant_roots)
            {
                std::process::abort();
            }
            let raw = context.raw();
            let entries = &mut self.entries;
            let insert = self.cache.insert_retiring(key, prepared, |retired| {
                let Some(index) = entries.iter().position(|entry| entry.id.get() == retired) else {
                    std::process::abort();
                };
                if !entries[index].artifact_loaded {
                    std::process::abort();
                }
                let retired = entries.remove(index);
                Self::clear_root_or_abort(raw, retired.root);
            });
            if let Err(error) = insert {
                if matches!(error, CodeMemoryError::DuplicateKey(_)) {
                    std::process::abort();
                }
                return self.reject_cleanly(context.raw(), id);
            }
            let Some(index) = self.entries.iter().position(|entry| entry.id == id) else {
                // A newly compiled identity cannot be the victim of making room for itself.
                std::process::abort();
            };
            self.entries[index].artifact_loaded = true;
            binding
        };

        let mut slots = Vec::new();
        let required = match num_registers
            .checked_add(1)
            .and_then(|count| count.checked_add(num_parameters))
            .and_then(|count| usize::try_from(count).ok())
        {
            Some(required) => required,
            None => return self.reject_cleanly(context.raw(), id),
        };
        if slots.try_reserve_exact(required).is_err() {
            return self.reject_cleanly(context.raw(), id);
        }
        slots.resize(num_registers as usize, JitSlot::undefined());
        let receiver = match JitSlot::try_from_value(context, *receiver) {
            Ok(receiver) => receiver,
            Err(_) => return self.reject_cleanly(context.raw(), id),
        };
        slots.push(receiver);
        for index in 0..num_parameters as usize {
            let Some(argument) = arguments.get(index) else {
                slots.push(JitSlot::undefined());
                continue;
            };
            let argument = match JitSlot::try_from_value(context, **argument) {
                Ok(argument) => argument,
                Err(_) => return self.reject_cleanly(context.raw(), id),
            };
            slots.push(argument);
        }
        if let Some(new_target) = new_target
            && let Some(index) = closure.function_ptr().new_target_index()
        {
            let new_target = match JitSlot::try_from_value(context, *new_target.as_value()) {
                Ok(new_target) => new_target,
                Err(_) => return self.reject_cleanly(context.raw(), id),
            };
            slots[index as usize] = new_target;
        }
        if slots.len() != required {
            std::process::abort();
        }

        #[cfg(test)]
        let is_nested_entry = match self.cache.has_pinned_entry_for_test() {
            Ok(is_nested) => is_nested,
            Err(_) => return HotDispatchAttempt::Terminal(HotDispatchTerminal::CacheIncoherent),
        };
        let loaded = match self.cache.pin(key) {
            Ok(Some(loaded)) => loaded,
            Ok(None) | Err(_) => {
                return HotDispatchAttempt::Terminal(HotDispatchTerminal::CacheIncoherent);
            }
        };
        let (mut budget, _request) =
            DeterministicInterruptBudget::new(self.policy.interrupt_quantum);
        #[cfg(test)]
        if std::mem::take(&mut self.request_next_entry) {
            _request.request();
        }
        #[cfg(test)]
        if is_nested_entry && std::mem::take(&mut self.request_next_nested_entry) {
            _request.request();
        }
        let outcome = run_vm_hot_call(
            context,
            vm,
            &loaded,
            &binding,
            &mut slots,
            arguments,
            &mut budget,
            self,
        );
        #[cfg(test)]
        if !matches!(&outcome, Err(HotCallRunError::PreEntry(_))) {
            self.native_entries = self.native_entries.saturating_add(1);
        }
        // Release the activation pin before any pre-entry rejection retires this exact mapping.
        // Every committed outcome has already left generated code at this point.
        drop(loaded);
        self.retire_deferred_rejections(context.raw());
        match outcome {
            Ok(ContainedOutcome::NativeReturned(value) | ContainedOutcome::VmReturned(value)) => {
                HotDispatchAttempt::Completed(Ok(value.value_for_dispatch()))
            }
            Ok(ContainedOutcome::VmThrew(value)) => {
                HotDispatchAttempt::Completed(Err(value.value_for_dispatch()))
            }
            Ok(ContainedOutcome::InterruptedAt(offset)) => {
                HotDispatchAttempt::Terminal(HotDispatchTerminal::NativeInterrupted(offset))
            }
            Ok(ContainedOutcome::VmInterruptedAt(offset)) => {
                HotDispatchAttempt::Terminal(HotDispatchTerminal::VmInterrupted(offset))
            }
            Ok(ContainedOutcome::AllocationFailedAt(offset)) => {
                HotDispatchAttempt::Terminal(HotDispatchTerminal::NativeAllocationFailed(offset))
            }
            Ok(ContainedOutcome::VmAllocationFailedAt(offset)) => {
                HotDispatchAttempt::Terminal(HotDispatchTerminal::VmAllocationFailed(offset))
            }
            Ok(ContainedOutcome::PoisonedAt(offset)) => {
                HotDispatchAttempt::Terminal(HotDispatchTerminal::Poisoned(offset))
            }
            Ok(ContainedOutcome::InvalidActivation) => {
                HotDispatchAttempt::Terminal(HotDispatchTerminal::InvalidActivation)
            }
            Ok(ContainedOutcome::UnsupportedAt(offset)) => {
                HotDispatchAttempt::Terminal(HotDispatchTerminal::ImpossibleUnsupported(offset))
            }
            Err(HotCallRunError::PreEntry(_)) => self.reject_cleanly(context.raw(), id),
            Err(HotCallRunError::PostEntry(_)) => {
                HotDispatchAttempt::Terminal(HotDispatchTerminal::PostEntryFailure)
            }
        }
    }

    fn find_or_insert_entry(
        &mut self,
        mut raw: crate::runtime::Context,
        function: HeapPtr<BytecodeFunction>,
    ) -> Option<usize> {
        for (index, entry) in self.entries.iter().enumerate() {
            let rooted = Self::require_root(raw, entry.root);
            if rooted.ptr_eq(&function) {
                return Some(index);
            }
        }
        if self.entries.len() == self.entry_limit {
            let index = self
                .entries
                .iter()
                .enumerate()
                .filter(|(_, entry)| match self.cache.is_pinned(entry.id.get()) {
                    Ok(pinned) => !pinned,
                    Err(_) => std::process::abort(),
                })
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(index, _)| index)?;
            let retired = self.entries[index].id;
            let expected_mapping = self.entries[index].artifact_loaded;
            if !matches!(
                self.cache.remove(retired.get()),
                Ok(removed) if removed == expected_mapping
            ) {
                std::process::abort();
            }
            let retired = self.entries.remove(index);
            Self::clear_root_or_abort(raw, retired.root);
        }
        if self.entries.try_reserve(1).is_err() {
            return None;
        }
        let id = allocate_vm_binding_id().ok()?;
        let roots = raw.jit_dispatch_roots();
        if !roots.has_capacity() {
            // At most one exact root exists for every bounded entry. Reaching root capacity while
            // metadata still has room means the registries have diverged, not ordinary pressure.
            std::process::abort();
        }
        let root = roots.allocate(function)?;
        self.entries.push(DispatchEntry {
            id,
            root,
            hotness: FunctionHotness::new(self.policy.thresholds),
            last_used: self.clock,
            artifact_loaded: false,
            rejected: false,
            retire_when_unpinned: false,
        });
        Some(self.entries.len() - 1)
    }

    fn touch(&mut self, index: usize) {
        self.clock = self.clock.saturating_add(1);
        self.entries[index].last_used = self.clock;
    }

    fn reject_cleanly(
        &mut self,
        raw: crate::runtime::Context,
        id: VmBindingId,
    ) -> HotDispatchAttempt {
        let Some(index) = self.entries.iter().position(|entry| entry.id == id) else {
            std::process::abort();
        };
        self.entries[index].rejected = true;
        if self.entries[index].artifact_loaded {
            match self.cache.is_pinned(id.get()) {
                Ok(true) => {
                    // A recursive clean rejection may name the same mapping whose outer
                    // activation is still executing. Keep the hard-counted mapping and exact root
                    // tombstoned until the final owning activation pin leaves generated code.
                    self.entries[index].retire_when_unpinned = true;
                    return self.clean_fallback();
                }
                Ok(false) => {}
                Err(_) => std::process::abort(),
            }
        }
        let expected_mapping = self.entries[index].artifact_loaded;
        if !matches!(
            self.cache.remove(id.get()),
            Ok(removed) if removed == expected_mapping
        ) {
            std::process::abort();
        }
        let entry = &mut self.entries[index];
        entry.artifact_loaded = false;
        entry.retire_when_unpinned = false;
        Self::clear_constant_roots_if_present(raw, entry.root);
        self.clean_fallback()
    }

    fn retire_deferred_rejections(&mut self, raw: crate::runtime::Context) {
        for entry in &mut self.entries {
            if !entry.retire_when_unpinned {
                continue;
            }
            if !entry.rejected || !entry.artifact_loaded {
                std::process::abort();
            }
            match self.cache.is_pinned(entry.id.get()) {
                Ok(true) => continue,
                Ok(false) => {}
                Err(_) => std::process::abort(),
            }
            if self.cache.remove(entry.id.get()) != Ok(true) {
                std::process::abort();
            }
            entry.artifact_loaded = false;
            entry.retire_when_unpinned = false;
            Self::clear_constant_roots_if_present(raw, entry.root);
        }
    }

    fn require_root(
        mut raw: crate::runtime::Context,
        id: DispatchRootSlotId,
    ) -> HeapPtr<BytecodeFunction> {
        let Some(function) = raw.jit_dispatch_roots().get(id) else {
            std::process::abort();
        };
        function
    }

    fn clear_root_or_abort(mut raw: crate::runtime::Context, id: DispatchRootSlotId) {
        if !raw.jit_dispatch_roots().clear(id) {
            std::process::abort();
        }
    }

    fn clear_constant_roots_if_present(mut raw: crate::runtime::Context, id: DispatchRootSlotId) {
        let roots = raw.jit_dispatch_roots();
        if roots.constants(id).is_some() && !roots.clear_constants(id) {
            std::process::abort();
        }
    }

    /// Retire every RX artifact and exact sibling root before `ContextCell` field destruction.
    pub(crate) fn shutdown(&mut self, mut raw: crate::runtime::Context) {
        self.retire_deferred_rejections(raw);
        for entry in self.entries.drain(..) {
            if !matches!(
                self.cache.remove(entry.id.get()),
                Ok(removed) if removed == entry.artifact_loaded
            ) {
                std::process::abort();
            }
            Self::clear_root_or_abort(raw, entry.root);
        }
        if self.cache.len() != 0 || !raw.jit_dispatch_roots().is_empty() {
            std::process::abort();
        }
    }

    fn clean_fallback(&mut self) -> HotDispatchAttempt {
        #[cfg(test)]
        {
            self.clean_fallbacks = self.clean_fallbacks.saturating_add(1);
        }
        HotDispatchAttempt::NotEntered
    }

    #[cfg(test)]
    pub(crate) fn configure_for_test(
        &mut self,
        call_threshold: NonZeroU32,
        interrupt_quantum: NonZeroU32,
    ) {
        assert!(self.entries.is_empty() && self.cache.len() == 0);
        self.policy.enabled = true;
        self.policy.thresholds.calls = call_threshold;
        self.policy.interrupt_quantum = interrupt_quantum;
    }

    #[cfg(test)]
    pub(crate) fn set_interrupt_quantum_for_test(&mut self, interrupt_quantum: NonZeroU32) {
        assert!(self.policy.enabled);
        self.policy.interrupt_quantum = interrupt_quantum;
    }

    #[cfg(test)]
    pub(crate) fn request_next_entry_interrupt_for_test(&mut self) {
        assert!(self.policy.enabled);
        assert!(!self.request_next_entry);
        self.request_next_entry = true;
    }

    #[cfg(test)]
    pub(crate) fn request_next_nested_entry_interrupt_for_test(&mut self) {
        assert!(self.policy.enabled);
        assert!(!self.request_next_nested_entry);
        self.request_next_nested_entry = true;
    }

    #[cfg(test)]
    pub(crate) fn collect_before_next_preentry_decision_for_test(&mut self) {
        assert!(self.policy.enabled);
        assert!(!self.collect_before_next_preentry_decision);
        self.collect_before_next_preentry_decision = true;
    }

    #[cfg(test)]
    pub(crate) fn set_entry_limit_for_test(&mut self, entry_limit: usize) {
        assert!((1..=DISPATCH_MAX_ENTRIES).contains(&entry_limit));
        assert!(self.entries.is_empty() && self.cache.len() == 0);
        self.entry_limit = entry_limit;
    }

    #[cfg(test)]
    pub(crate) fn observations_for_test(&self) -> (u32, u32, u32) {
        (self.compile_attempts, self.native_entries, self.clean_fallbacks)
    }

    #[cfg(test)]
    pub(crate) fn artifact_coherent_for_test(&self, mut raw: crate::runtime::Context) -> bool {
        self.entries.len() == raw.jit_dispatch_roots().live_count()
            && self.entries.iter().all(|entry| {
                let _root = Self::require_root(raw, entry.root);
                let has_constant_roots = raw.jit_dispatch_roots().constants(entry.root).is_some();
                let cached = self.cache.contains_key(entry.id.get()).unwrap_or(false);
                cached == entry.artifact_loaded
                    && (entry.artifact_loaded == has_constant_roots)
                    && (!entry.retire_when_unpinned
                        || (entry.rejected && cached && entry.artifact_loaded))
                    && (!entry.rejected
                        || (!cached && !entry.artifact_loaded)
                        || entry.retire_when_unpinned)
            })
    }

    #[cfg(test)]
    pub(crate) fn root_count_for_test(&self, mut raw: crate::runtime::Context) -> usize {
        raw.jit_dispatch_roots().live_count()
    }

    #[cfg(test)]
    pub(crate) fn first_root_for_test(&self) -> Option<DispatchRootSlotId> {
        self.entries.first().map(|entry| entry.root)
    }

    #[cfg(test)]
    pub(crate) fn rooted_function_address_for_test(
        &self,
        raw: crate::runtime::Context,
    ) -> Option<usize> {
        self.entries
            .first()
            .map(|entry| Self::require_root(raw, entry.root).as_ptr() as usize)
    }

    #[cfg(test)]
    pub(crate) fn retire_first_code_only_for_test(&mut self) {
        let entry = self.entries.first().expect("test dispatch entry");
        assert!(entry.artifact_loaded && !entry.rejected);
        assert_eq!(self.cache.remove(entry.id.get()), Ok(true));
    }
}

#[cfg(test)]
mod tests {
    use std::{
        num::NonZeroU32,
        panic::{AssertUnwindSafe, catch_unwind},
    };

    use super::*;
    #[cfg(feature = "alloc_error")]
    use crate::runtime::bytecode::vm::with_test_active_jit_fallback_allocation_failure;
    use crate::runtime::{
        ContextBuilder, EvalResult,
        bytecode::{
            constant_table::ConstantTable,
            exception_handlers::{ExceptionHandlerBuilder, ExceptionHandlersBuilder},
            instruction::OpCode,
            stack_frame::{FIRST_ARGUMENT_SLOT_INDEX, RECEIVER_SLOT_INDEX},
            vm::with_test_active_jit_fallback_dispatch_panic,
        },
        gc::{GcType, Heap},
        intrinsics::intrinsics::Intrinsic,
        jit::abi::{
            TestBackedgePollBehavior, TestHelperBehavior, with_test_backedge_poll_behavior,
            with_test_helper_behavior,
        },
        realm::Realm,
        string_value::StringValue,
    };
    #[cfg(feature = "handle_stats")]
    use crate::runtime::{
        bytecode::generator::BytecodeScript,
        bytecode::vm::with_test_jit_resume_collection,
        eval_result::EvalError,
        gc::HandleScope,
        global_names::GlobalNames,
        intrinsics::{error_object::ErrorObject, rust_runtime::RuntimeFunction},
        scope_names::{ScopeFlags, ScopeNames},
    };

    fn nonzero(value: u32) -> NonZeroU32 {
        NonZeroU32::new(value).unwrap()
    }

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

    fn make_test_closure(
        context: &mut JitContextScope<'_>,
        bytes: Vec<u8>,
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
                None,
                None,
                None,
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

    /// Build an outer -> middle -> leaf chain whose first two functions each execute ordinary
    /// synchronous `NewClosure` from their own rooted constant table.
    fn make_nested_closure_factory(context: &mut JitContextScope<'_>) -> Handle<ClosureObject> {
        let mut closure = None;
        expect_eval_ok(context.with_initial_realm(|context| {
            let raw = context.raw();
            let realm = raw.initial_realm();
            let leaf = BytecodeFunction::new_for_jit_test(
                raw,
                encode(OpCode::Ret, &[RECEIVER_SLOT_INDEX as u8]),
                None,
                None,
                None,
                realm,
                0,
                0,
            )?;
            let leaf_table = ConstantTable::new(
                raw,
                vec![leaf.cast::<Value>()],
                vec![0; ConstantTable::calculate_metadata_size(1)],
            )?;
            let mut factory_bytes = encode(OpCode::NewClosure, &[local(0), 0]);
            factory_bytes.extend(encode(OpCode::Ret, &[local(0)]));
            let middle = BytecodeFunction::new_for_jit_test(
                raw,
                factory_bytes.clone(),
                Some(leaf_table),
                None,
                None,
                realm,
                1,
                0,
            )?;
            let middle_table = ConstantTable::new(
                raw,
                vec![middle.cast::<Value>()],
                vec![0; ConstantTable::calculate_metadata_size(1)],
            )?;
            let outer = BytecodeFunction::new_for_jit_test(
                raw,
                factory_bytes,
                Some(middle_table),
                None,
                None,
                realm,
                1,
                0,
            )?;
            closure = Some(ClosureObject::new_without_properties(
                raw,
                outer,
                realm.default_global_scope(),
                None,
            )?);
            Ok(())
        }));
        closure.unwrap()
    }

    fn make_semantic_test_closure(
        context: &mut JitContextScope<'_>,
        bytes: Vec<u8>,
        num_locals: usize,
        num_parameters: usize,
        is_strict: bool,
        constructor_is_base: Option<bool>,
        new_target_index: Option<u32>,
    ) -> Handle<ClosureObject> {
        let mut closure = None;
        expect_eval_ok(context.with_initial_realm(|context| {
            let raw = context.raw();
            let realm = raw.initial_realm();
            let mut function = BytecodeFunction::new_for_jit_test(
                raw,
                bytes,
                None,
                None,
                None,
                realm,
                num_locals as u32,
                num_parameters as u32,
            )?;
            function.configure_call_semantics_for_jit_test(
                is_strict,
                constructor_is_base,
                new_target_index,
            );
            closure = Some(if constructor_is_base.is_some() {
                ClosureObject::new(raw, function, realm.default_global_scope(), realm)?
            } else {
                ClosureObject::new_without_properties(
                    raw,
                    function,
                    realm.default_global_scope(),
                    None,
                )?
            });
            Ok(())
        }));
        closure.unwrap()
    }

    fn make_test_closure_with_handler(
        context: &mut JitContextScope<'_>,
        bytes: Vec<u8>,
        num_locals: usize,
        num_parameters: usize,
        handler: ExceptionHandlerBuilder,
    ) -> Handle<ClosureObject> {
        let mut closure = None;
        expect_eval_ok(context.with_initial_realm(|context| {
            let raw = context.raw();
            let realm = raw.initial_realm();
            let mut handlers = ExceptionHandlersBuilder::new();
            handlers.add(handler);
            let handlers = handlers.finish(raw)?;
            let function = BytecodeFunction::new_for_jit_test(
                raw,
                bytes,
                None,
                handlers,
                None,
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

    #[cfg(feature = "handle_stats")]
    fn make_runtime_call_closure(context: &mut JitContextScope<'_>) -> Handle<ClosureObject> {
        let mut closure = None;
        expect_eval_ok(context.with_initial_realm(|context| {
            let raw = context.raw();
            let realm = raw.initial_realm();
            let function = BytecodeFunction::new_rust_runtime_function(
                raw,
                RuntimeFunction::FunctionPrototype_call_intrinsic.to_id(),
                realm,
                false,
                None,
                1,
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

    #[cfg(feature = "handle_stats")]
    fn make_test_constructor(
        context: &mut JitContextScope<'_>,
        bytes: Vec<u8>,
        num_locals: usize,
        num_parameters: usize,
    ) -> Handle<ClosureObject> {
        let mut closure = None;
        expect_eval_ok(context.with_initial_realm(|context| {
            let raw = context.raw();
            let realm = raw.initial_realm();
            let mut function = BytecodeFunction::new_for_jit_test(
                raw,
                bytes,
                None,
                None,
                None,
                realm,
                num_locals as u32,
                num_parameters as u32,
            )?;
            function.mark_base_constructor_for_jit_test();
            closure = Some(ClosureObject::new(raw, function, realm.default_global_scope(), realm)?);
            Ok(())
        }));
        closure.unwrap()
    }

    #[cfg(feature = "handle_stats")]
    fn make_test_script(
        context: &mut JitContextScope<'_>,
        bytes: Vec<u8>,
        num_locals: usize,
    ) -> (Handle<BytecodeFunction>, Handle<GlobalNames>) {
        let mut script = None;
        expect_eval_ok(context.with_initial_realm(|context| {
            let raw = context.raw();
            let realm = raw.initial_realm();
            let function = BytecodeFunction::new_for_jit_test(
                raw,
                bytes,
                None,
                None,
                None,
                realm,
                num_locals as u32,
                0,
            )?;
            let scope_names = ScopeNames::new(raw, ScopeFlags::IS_VAR_SCOPE, &[], &[])?;
            let global_names = GlobalNames::new(raw, &[], scope_names)?;
            script = Some((function, global_names));
            Ok(())
        }));
        script.unwrap()
    }

    #[cfg(feature = "handle_stats")]
    fn execute_scoped_without_accumulation(
        context: &mut JitContextScope<'_>,
        closure: Handle<ClosureObject>,
        receiver: Handle<Value>,
        arguments: &[Handle<Value>],
    ) -> Result<Value, Value> {
        let mut raw = context.raw();
        let handles_before = raw.vm().jit_handle_count_for_test();
        let call_scope = HandleScope::enter(raw);
        let result = raw.vm().execute_for_jit_test(closure, receiver, arguments);
        assert_eq!(raw.vm().jit_handle_count_for_test(), handles_before + 1);
        let copied = match result {
            Ok(value) => Ok(*value),
            Err(EvalError::Value(error)) => Err(*error),
            #[cfg(feature = "alloc_error")]
            Err(EvalError::Alloc(_)) => panic!("unexpected allocation failure"),
        };
        drop(call_scope);
        assert_eq!(raw.vm().jit_handle_count_for_test(), handles_before);
        copied
    }

    #[cfg(feature = "handle_stats")]
    fn execute_scoped_with_stack_constraints(
        context: &mut JitContextScope<'_>,
        closure: Handle<ClosureObject>,
        receiver: Handle<Value>,
        available_slots: Option<usize>,
        force_max_depth: bool,
    ) -> Result<Value, Value> {
        let mut raw = context.raw();
        let handles_before = raw.vm().jit_handle_count_for_test();
        let call_scope = HandleScope::enter(raw);
        let result =
            raw.vm()
                .with_jit_stack_constraints_for_test(available_slots, force_max_depth, |vm| {
                    vm.execute_for_jit_test(closure, receiver, &[])
                });
        assert_eq!(raw.vm().jit_handle_count_for_test(), handles_before + 1);
        let copied = match result {
            Ok(value) => Ok(*value),
            Err(EvalError::Value(error)) => Err(*error),
            #[cfg(feature = "alloc_error")]
            Err(EvalError::Alloc(_)) => panic!("unexpected allocation failure"),
        };
        drop(call_scope);
        assert_eq!(raw.vm().jit_handle_count_for_test(), handles_before);
        copied
    }

    #[cfg(feature = "handle_stats")]
    fn expect_copied_ok(label: &str, result: Result<Value, Value>) -> Value {
        match result {
            Ok(value) => value,
            Err(_) => panic!("unexpected copied throw during {label}"),
        }
    }

    #[cfg(feature = "handle_stats")]
    fn expect_copied_throw(result: Result<Value, Value>) -> Value {
        match result {
            Ok(_) => panic!("expected copied throw"),
            Err(value) => value,
        }
    }

    fn call_function<'scope>(
        context: &mut JitContextScope<'scope>,
        closure: Handle<ClosureObject>,
        receiver: Handle<Value>,
        arguments: &[Handle<Value>],
    ) -> EvalResult<Handle<Value>> {
        context.with_initial_realm(|context| {
            let mut raw = context.raw();
            raw.vm().call_from_rust(closure.cast(), receiver, arguments)
        })
    }

    fn configure(context: &mut JitContextScope<'_>, calls: u32, quantum: u32) {
        let mut raw = context.raw();
        raw.jit_dispatch()
            .configure_for_test(nonzero(calls), nonzero(quantum));
    }

    fn assert_coherent(context: &JitContextScope<'_>) {
        let mut state_raw = context.raw();
        let roots_raw = state_raw;
        let state = state_raw.jit_dispatch();
        assert!(state.artifact_coherent_for_test(roots_raw));
    }

    fn observations(context: &JitContextScope<'_>) -> (u32, u32, u32) {
        let mut raw = context.raw();
        raw.jit_dispatch().observations_for_test()
    }

    fn finite_loop(limit: i8) -> Vec<u8> {
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
        bytes
    }

    fn recursive_call_program(function_argument: usize) -> Vec<u8> {
        let mut bytes = encode(OpCode::LoadImmediate, &[local(4), 0]);
        bytes.extend(encode(
            OpCode::LessThanOrEqual,
            &[local(3), (FIRST_ARGUMENT_SLOT_INDEX + 1) as u8, local(4)],
        ));
        let branch_offset = bytes.len();
        bytes.extend(encode(OpCode::JumpTrue, &[local(3), 0]));
        bytes.extend(encode(OpCode::Mov, &[local(0), function_argument as u8]));
        bytes.extend(encode(OpCode::SubImm, &[local(1), (FIRST_ARGUMENT_SLOT_INDEX + 1) as u8, 1]));
        bytes.extend(encode(OpCode::Call, &[local(2), function_argument as u8, local(0), 2]));
        bytes.extend(encode(OpCode::Ret, &[local(2)]));
        let base_offset = bytes.len();
        bytes.extend(encode(OpCode::Ret, &[(FIRST_ARGUMENT_SLOT_INDEX + 1) as u8]));
        bytes[branch_offset + 2] = (base_offset as isize - branch_offset as isize) as i8 as u8;
        bytes
    }

    fn mutual_recursive_call_program() -> Vec<u8> {
        let mut bytes = encode(OpCode::LoadImmediate, &[local(5), 0]);
        bytes.extend(encode(
            OpCode::LessThanOrEqual,
            &[local(4), (FIRST_ARGUMENT_SLOT_INDEX + 2) as u8, local(5)],
        ));
        let branch_offset = bytes.len();
        bytes.extend(encode(OpCode::JumpTrue, &[local(4), 0]));
        bytes.extend(encode(OpCode::Mov, &[local(0), (FIRST_ARGUMENT_SLOT_INDEX + 1) as u8]));
        bytes.extend(encode(OpCode::Mov, &[local(1), FIRST_ARGUMENT_SLOT_INDEX as u8]));
        bytes.extend(encode(OpCode::SubImm, &[local(2), (FIRST_ARGUMENT_SLOT_INDEX + 2) as u8, 1]));
        bytes.extend(encode(
            OpCode::Call,
            &[local(3), (FIRST_ARGUMENT_SLOT_INDEX + 1) as u8, local(0), 3],
        ));
        bytes.extend(encode(OpCode::Ret, &[local(3)]));
        let base_offset = bytes.len();
        bytes.extend(encode(OpCode::Ret, &[(FIRST_ARGUMENT_SLOT_INDEX + 2) as u8]));
        bytes[branch_offset + 2] = (base_offset as isize - branch_offset as isize) as i8 as u8;
        bytes
    }

    #[cfg(feature = "handle_stats")]
    #[test]
    fn actual_vm_execute_disabled_policy_escapes_exactly_one_handle() {
        let bytes = encode(OpCode::Ret, &[RECEIVER_SLOT_INDEX as u8]);
        let mut owned = ContextBuilder::new().build().unwrap();
        owned.with_jit_context(|context| {
            let closure = make_test_closure(context, bytes, 0, 0);
            let receiver = context.raw().smi(23);
            expect_eval_ok(context.with_initial_realm(|context| {
                let result = execute_scoped_without_accumulation(context, closure, receiver, &[]);
                assert_eq!(
                    expect_copied_ok("disabled-policy return", result).as_raw_bits(),
                    Value::raw_smi(23).as_raw_bits()
                );
                assert_eq!(observations(context), (0, 0, 0));
                Ok(())
            }));
            context.raw().vm().debug_assert_stack_empty();
        });
    }

    #[cfg(feature = "handle_stats")]
    #[test]
    fn actual_vm_execute_and_constructor_callbacks_restore_handles_and_frames() {
        let mut fallback_bytes = encode(OpCode::Mov, &[local(0), RECEIVER_SLOT_INDEX as u8]);
        fallback_bytes.extend(encode(OpCode::ToNumber, &[local(1), local(0)]));
        fallback_bytes.extend(encode(OpCode::Ret, &[local(0)]));

        let throw_bytes = encode(OpCode::Throw, &[RECEIVER_SLOT_INDEX as u8]);

        let mut middle_bytes = encode(
            OpCode::CallWithReceiver,
            &[
                local(0),
                FIRST_ARGUMENT_SLOT_INDEX as u8,
                (FIRST_ARGUMENT_SLOT_INDEX + 1) as u8,
                (FIRST_ARGUMENT_SLOT_INDEX + 2) as u8,
                1,
            ],
        );
        middle_bytes.extend(encode(OpCode::Ret, &[local(0)]));

        let mut outer_bytes =
            encode(OpCode::Mov, &[local(1), (FIRST_ARGUMENT_SLOT_INDEX + 1) as u8]);
        outer_bytes.extend(encode(OpCode::Mov, &[local(2), (FIRST_ARGUMENT_SLOT_INDEX + 2) as u8]));
        outer_bytes.extend(encode(OpCode::Mov, &[local(3), (FIRST_ARGUMENT_SLOT_INDEX + 3) as u8]));
        outer_bytes.extend(encode(
            OpCode::Call,
            &[local(0), FIRST_ARGUMENT_SLOT_INDEX as u8, local(1), 3],
        ));
        outer_bytes.extend(encode(OpCode::Ret, &[local(0)]));

        let mut owned = ContextBuilder::new().build().unwrap();
        owned.with_jit_context(|context| {
            configure(context, 1, 100);
            let hot = make_test_closure(context, finite_loop(4), 3, 0);
            let fallback = make_test_closure(context, fallback_bytes, 2, 0);
            let throwing = make_test_closure(context, throw_bytes, 0, 0);
            let runtime_call = make_runtime_call_closure(context);
            let middle = make_test_closure(context, middle_bytes, 1, 3);
            let outer = make_test_closure(context, outer_bytes.clone(), 4, 4);
            let constructor = make_test_constructor(context, outer_bytes, 4, 4);
            let mut raw = context.raw();
            let undefined = raw.undefined();
            let fallback_receiver = expect_eval_ok(raw.alloc_string("actual fallback")).as_value();
            let thrown = raw.smi(91);
            let inner_receiver = raw.smi(37);
            let nested_arguments = [
                middle.as_value(),
                runtime_call.as_value(),
                hot.as_value(),
                inner_receiver,
            ];

            expect_eval_ok(context.with_initial_realm(|context| {
                let native = execute_scoped_without_accumulation(context, hot, undefined, &[]);
                assert_eq!(
                    expect_copied_ok("native return", native).as_raw_bits(),
                    Value::raw_smi(4).as_raw_bits()
                );

                let fallback_result =
                    execute_scoped_without_accumulation(context, fallback, fallback_receiver, &[]);
                assert_eq!(
                    expect_copied_ok("negative-cache fallback", fallback_result).as_raw_bits(),
                    (*fallback_receiver).as_raw_bits()
                );

                let thrown_result =
                    execute_scoped_without_accumulation(context, throwing, thrown, &[]);
                assert_eq!(
                    expect_copied_throw(thrown_result).as_raw_bits(),
                    Value::raw_smi(91).as_raw_bits()
                );

                let before_callback = observations(context);
                let (nested, collected) = with_test_jit_resume_collection(|| {
                    execute_scoped_without_accumulation(
                        context,
                        outer,
                        undefined,
                        &nested_arguments,
                    )
                });
                assert!(collected, "outer resume must collect before the callback boundary");
                assert_eq!(
                    expect_copied_ok("nested callback return", nested).as_raw_bits(),
                    Value::raw_smi(4).as_raw_bits()
                );
                let after_callback = observations(context);
                assert_eq!(after_callback.0 - before_callback.0, 2);
                assert_eq!(after_callback.1 - before_callback.1, 1);
                assert_eq!(after_callback.2 - before_callback.2, 1);

                // `Function.prototype.call` reenters bytecode while the dispatcher is moved out
                // of Context. The already-compiled callback must therefore interpret, propagate
                // its exact throw once, and leave only the outer/middle tier entries observable.
                let throwing_callback_arguments = [
                    middle.as_value(),
                    runtime_call.as_value(),
                    throwing.as_value(),
                    thrown,
                ];
                let before_throwing_callback = observations(context);
                let callback_throw = execute_scoped_without_accumulation(
                    context,
                    outer,
                    undefined,
                    &throwing_callback_arguments,
                );
                assert_eq!(
                    expect_copied_throw(callback_throw).as_raw_bits(),
                    Value::raw_smi(91).as_raw_bits()
                );
                let after_throwing_callback = observations(context);
                assert_eq!(after_throwing_callback.0 - before_throwing_callback.0, 0);
                assert_eq!(after_throwing_callback.1 - before_throwing_callback.1, 1);
                assert_eq!(after_throwing_callback.2 - before_throwing_callback.2, 1);

                let mut raw = context.raw();
                let handles_before = raw.vm().jit_handle_count_for_test();
                let stack_before = raw.vm().jit_stack_state_for_test();
                raw.jit_dispatch().request_next_entry_interrupt_for_test();
                let (interrupted, polls) =
                    with_test_backedge_poll_behavior(TestBackedgePollBehavior::Normal, || {
                        catch_unwind(AssertUnwindSafe(|| {
                            let mut raw = context.raw();
                            let _call_scope = HandleScope::enter(raw);
                            let _ = raw.vm().execute_for_jit_test(hot, undefined, &[]);
                        }))
                    });
                assert!(interrupted.is_err());
                assert_eq!(polls.calls, 1);
                assert_eq!(raw.vm().jit_handle_count_for_test(), handles_before);
                assert_eq!(raw.vm().jit_stack_state_for_test(), stack_before);
                assert!(!context.has_registered_jit_frame());

                raw.jit_dispatch().request_next_entry_interrupt_for_test();
                let (poisoned, polls) =
                    with_test_backedge_poll_behavior(TestBackedgePollBehavior::Panic, || {
                        catch_unwind(AssertUnwindSafe(|| {
                            let mut raw = context.raw();
                            let _call_scope = HandleScope::enter(raw);
                            let _ = raw.vm().execute_for_jit_test(hot, undefined, &[]);
                        }))
                    });
                assert!(poisoned.is_err());
                assert_eq!(polls.calls, 1);
                assert_eq!(raw.vm().jit_handle_count_for_test(), handles_before);
                assert_eq!(raw.vm().jit_stack_state_for_test(), stack_before);
                assert!(!context.has_registered_jit_frame());

                let constructor_scope = HandleScope::enter(raw);
                let constructed = expect_eval_ok(raw.vm().construct_from_rust(
                    constructor.as_value(),
                    &nested_arguments,
                    constructor.as_object(),
                ));
                assert!(!constructed.as_ptr().is_null());
                assert_eq!(raw.vm().jit_handle_count_for_test(), handles_before + 1);
                drop(constructor_scope);
                assert_eq!(raw.vm().jit_handle_count_for_test(), handles_before);
                assert_eq!(raw.vm().jit_stack_state_for_test(), stack_before);

                let recovered = execute_scoped_without_accumulation(
                    context,
                    outer,
                    undefined,
                    &nested_arguments,
                );
                assert_eq!(
                    expect_copied_ok("post-terminal recovery", recovered).as_raw_bits(),
                    Value::raw_smi(4).as_raw_bits()
                );
                assert!(observations(context).0 >= 4);
                assert!(observations(context).1 >= 5);
                assert_coherent(context);
                Ok(())
            }));
            context.raw().vm().debug_assert_stack_empty();
        });
    }

    #[cfg(feature = "handle_stats")]
    #[test]
    fn stack_admission_rejects_before_native_effects_and_uses_ordinary_errors() {
        let mut bytes = encode(OpCode::NewObject, &[local(0), 0]);
        bytes.extend(encode(OpCode::Ret, &[local(0)]));
        let mut owned = ContextBuilder::new().build().unwrap();
        owned.with_jit_context(|context| {
            configure(context, 1, 100);
            let closure = make_test_closure(context, bytes, 1, 0);
            let receiver = context.raw().undefined();
            expect_eval_ok(context.with_initial_realm(|context| {
                let stack_before = context.raw().vm().jit_stack_state_for_test();
                let near_capacity = expect_copied_throw(execute_scoped_with_stack_constraints(
                    context,
                    closure,
                    receiver,
                    Some(FIRST_ARGUMENT_SLOT_INDEX),
                    false,
                ));
                let near_capacity = near_capacity
                    .as_opt::<ErrorObject>()
                    .expect("ordinary stack overflow ErrorObject");
                assert!(near_capacity.is_stack_overflow());

                let max_depth = expect_copied_throw(execute_scoped_with_stack_constraints(
                    context, closure, receiver, None, true,
                ));
                let max_depth = max_depth
                    .as_opt::<ErrorObject>()
                    .expect("ordinary stack overflow ErrorObject");
                assert!(max_depth.is_stack_overflow());
                assert_eq!(observations(context), (0, 0, 0));
                assert_eq!(context.raw().vm().jit_stack_state_for_test(), stack_before);

                let (normal, helper) =
                    with_test_helper_behavior(TestHelperBehavior::Normal, || {
                        execute_scoped_without_accumulation(context, closure, receiver, &[])
                    });
                assert!(expect_copied_ok("post-stack-overflow recovery", normal).is_object());
                assert_eq!(helper.calls, 1);
                assert_eq!(observations(context), (1, 1, 0));
                assert_eq!(context.raw().vm().jit_stack_state_for_test(), stack_before);
                Ok(())
            }));
            context.raw().vm().debug_assert_stack_empty();
        });
    }

    #[cfg(feature = "handle_stats")]
    #[test]
    fn actual_context_run_script_terminal_restores_state_and_reuses_context() {
        let mut owned = ContextBuilder::new().build().unwrap();
        owned.with_jit_context(|context| {
            configure(context, 1, 100);
            let (script_function, global_names) = make_test_script(context, finite_loop(4), 3);
            let mut raw = context.raw();
            let handles_before = raw.vm().jit_handle_count_for_test();
            let stack_before = raw.vm().jit_stack_state_for_test();
            raw.jit_dispatch().request_next_entry_interrupt_for_test();

            let (terminal, polls) =
                with_test_backedge_poll_behavior(TestBackedgePollBehavior::Normal, || {
                    catch_unwind(AssertUnwindSafe(|| {
                        let mut raw = context.raw();
                        let _run_scope = HandleScope::enter(raw);
                        let _ = raw.run_script(BytecodeScript { script_function, global_names });
                    }))
                });
            assert!(terminal.is_err());
            assert_eq!(polls.calls, 1);
            assert_eq!(raw.vm().jit_handle_count_for_test(), handles_before);
            assert_eq!(raw.vm().jit_stack_state_for_test(), stack_before);
            assert!(!context.has_registered_jit_frame());
            assert_coherent(context);

            let run_scope = HandleScope::enter(raw);
            expect_eval_ok(raw.run_script(BytecodeScript { script_function, global_names }));
            drop(run_scope);
            assert_eq!(raw.vm().jit_handle_count_for_test(), handles_before);
            assert_eq!(raw.vm().jit_stack_state_for_test(), stack_before);
            assert!(!context.has_registered_jit_frame());
            assert_coherent(context);
        });
    }

    #[test]
    fn dispatch_take_guard_restores_on_panic_and_nested_entry_interprets() {
        let mut owned = ContextBuilder::new().build().unwrap();
        owned.with_jit_context(|context| {
            configure(context, 1, 100);
            let closure =
                make_test_closure(context, encode(OpCode::Ret, &[RECEIVER_SLOT_INDEX as u8]), 0, 0);
            let receiver = context.raw().smi(17);

            let panicked = catch_unwind(AssertUnwindSafe(|| {
                let mut raw = context.raw();
                let _ = raw.with_internal_jit_dispatch(|_, _| -> () {
                    panic!("injected dispatch-hook panic")
                });
            }));
            assert!(panicked.is_err());
            assert_coherent(context);

            let mut raw = context.raw();
            let outer = raw.with_internal_jit_dispatch(|dispatch, scope| {
                let mut nested = scope.raw();
                assert!(nested.with_internal_jit_dispatch(|_, _| ()).is_none());
                let interpreted = expect_eval_ok(call_function(scope, closure, receiver, &[]));
                assert_eq!((*interpreted).as_raw_bits(), Value::raw_smi(17).as_raw_bits());
                assert_eq!(dispatch.observations_for_test(), (0, 0, 0));
            });
            assert_eq!(outer, Some(()));
            assert_coherent(context);
            context.raw().vm().debug_assert_stack_empty();
        });
    }

    #[test]
    fn hot_threshold_enters_once_and_reuses_exact_argument_artifact() {
        let mut bytes = encode(OpCode::AddImm, &[local(0), FIRST_ARGUMENT_SLOT_INDEX as u8, 1]);
        bytes.extend(encode(OpCode::Ret, &[local(0)]));

        let mut owned = ContextBuilder::new().build().unwrap();
        owned.with_jit_context(|context| {
            configure(context, 2, 100);
            let closure = make_test_closure(context, bytes, 1, 1);
            let raw = context.raw();
            let receiver = raw.undefined();
            let argument = raw.smi(7);

            for _ in 0..3 {
                let result = expect_eval_ok(call_function(context, closure, receiver, &[argument]));
                assert_eq!((*result).as_raw_bits(), Value::raw_smi(8).as_raw_bits());
            }

            assert_eq!(observations(context), (1, 2, 1));
            assert_coherent(context);
            context.raw().vm().debug_assert_stack_empty();
        });
    }

    #[test]
    fn ordinary_hot_dispatch_builds_nested_closures_through_exact_vm_side_exits() {
        let mut owned = ContextBuilder::new().build().unwrap();
        owned.with_jit_context(|context| {
            configure(context, 1, 100);
            let outer = make_nested_closure_factory(context);
            let undefined = context.raw().undefined();

            let first_middle = expect_eval_ok(call_function(context, outer, undefined, &[]));
            assert!(first_middle.is::<ClosureObject>());
            context
                .raw()
                .jit_dispatch()
                .collect_before_next_preentry_decision_for_test();
            let second_middle = expect_eval_ok(call_function(context, outer, undefined, &[]));
            assert!(second_middle.is::<ClosureObject>());
            assert_ne!(
                first_middle.as_raw_bits(),
                second_middle.as_raw_bits(),
                "each ordinary call executes exactly one fresh closure allocation"
            );

            let middle = first_middle.cast::<ClosureObject>();
            let leaf = expect_eval_ok(call_function(context, middle, undefined, &[]));
            assert!(leaf.is::<ClosureObject>());
            let receiver = context.raw().smi(73);
            let result =
                expect_eval_ok(call_function(context, leaf.cast::<ClosureObject>(), receiver, &[]));
            assert_eq!(result.as_raw_bits(), Value::raw_smi(73).as_raw_bits());

            assert_eq!(observations(context), (3, 4, 0));
            assert_coherent(context);
            assert!(!context.has_registered_jit_frame());
            context.raw().vm().debug_assert_stack_empty();
        });
    }

    #[test]
    fn cached_artifact_rebind_rejects_equal_content_constant_substitution() {
        let mut bytes = encode(OpCode::LoadConstant, &[local(0), 0]);
        bytes.extend(encode(OpCode::Ret, &[local(0)]));
        let mut owned = ContextBuilder::new().build().unwrap();
        owned.with_jit_context(|context| {
            configure(context, 1, 100);
            let mut raw = context.raw();
            let first = expect_eval_ok(raw.alloc_string("stable constant identity"));
            let second = expect_eval_ok(raw.alloc_string("stable constant identity"));
            assert_ne!(first.as_value().as_raw_bits(), second.as_value().as_raw_bits());
            let table = ConstantTable::new(
                raw,
                vec![first.cast::<Value>()],
                vec![0; ConstantTable::calculate_metadata_size(1)],
            )
            .unwrap();
            let realm = raw.initial_realm();
            let function = BytecodeFunction::new_for_jit_test(
                raw,
                bytes,
                Some(table),
                None,
                None,
                realm,
                1,
                0,
            )
            .unwrap();
            let closure = ClosureObject::new_without_properties(
                raw,
                function,
                realm.default_global_scope(),
                None,
            )
            .unwrap();
            let undefined = raw.undefined();

            let initial = expect_eval_ok(call_function(context, closure, undefined, &[]));
            assert_eq!(initial.as_raw_bits(), first.as_value().as_raw_bits());
            let mut table = closure.function_ptr().constant_table_ptr().unwrap();
            table.set_constant(0, *second.as_value());

            let substituted = expect_eval_ok(call_function(context, closure, undefined, &[]));
            assert_eq!(substituted.as_raw_bits(), second.as_value().as_raw_bits());
            assert_eq!(observations(context), (1, 1, 1));
            assert_coherent(context);
            assert!(!context.has_registered_jit_frame());
            context.raw().vm().debug_assert_stack_empty();
        });
    }

    #[test]
    fn missing_and_extra_arguments_share_native_artifact_with_canonical_formal_padding() {
        let bytes = encode(OpCode::Ret, &[FIRST_ARGUMENT_SLOT_INDEX as u8]);
        let mut owned = ContextBuilder::new().build().unwrap();
        owned.with_jit_context(|context| {
            configure(context, 1, 100);
            let closure = make_test_closure(context, bytes, 0, 1);
            let raw = context.raw();
            let receiver = raw.undefined();
            let first = raw.smi(4);
            let second = raw.smi(9);

            let missing = expect_eval_ok(call_function(context, closure, receiver, &[]));
            assert!(missing.is_undefined());
            let extra = expect_eval_ok(call_function(context, closure, receiver, &[first, second]));
            assert_eq!((*extra).as_raw_bits(), Value::raw_smi(4).as_raw_bits());
            assert_eq!(observations(context), (1, 2, 0));
            {
                let mut raw = context.raw();
                let roots_raw = raw;
                assert_eq!(raw.jit_dispatch().root_count_for_test(roots_raw), 1);
            }

            let exact = expect_eval_ok(call_function(context, closure, receiver, &[first]));
            assert_eq!((*exact).as_raw_bits(), Value::raw_smi(4).as_raw_bits());
            assert_eq!(observations(context), (1, 3, 0));
            assert_coherent(context);
        });
    }

    #[test]
    fn ordinary_vm_call_side_exit_enters_nested_tier_without_replay() {
        let mut inner_bytes =
            encode(OpCode::AddImm, &[local(0), FIRST_ARGUMENT_SLOT_INDEX as u8, 1]);
        inner_bytes.extend(encode(OpCode::Ret, &[local(0)]));

        let mut outer_bytes =
            encode(OpCode::Mov, &[local(0), (FIRST_ARGUMENT_SLOT_INDEX + 1) as u8]);
        outer_bytes.extend(encode(
            OpCode::Call,
            &[local(1), FIRST_ARGUMENT_SLOT_INDEX as u8, local(0), 1],
        ));
        outer_bytes.extend(encode(OpCode::Ret, &[local(1)]));

        let mut owned = ContextBuilder::new().build().unwrap();
        owned.with_jit_context(|context| {
            configure(context, 1, 100);
            let inner = make_test_closure(context, inner_bytes, 1, 1);
            let outer = make_test_closure(context, outer_bytes, 2, 2);
            let raw = context.raw();
            let undefined = raw.undefined();
            let argument = raw.smi(41);

            let result = expect_eval_ok(call_function(
                context,
                outer,
                undefined,
                &[inner.as_value(), argument],
            ));
            assert_eq!((*result).as_raw_bits(), Value::raw_smi(42).as_raw_bits());
            assert_eq!(observations(context), (2, 2, 0));
            assert_coherent(context);
            context.raw().vm().debug_assert_stack_empty();
        });
    }

    #[test]
    fn recursive_same_artifact_stays_pinned_and_reenters_at_each_call() {
        let bytes = recursive_call_program(FIRST_ARGUMENT_SLOT_INDEX);
        let mut owned = ContextBuilder::new().build().unwrap();
        owned.with_jit_context(|context| {
            configure(context, 1, 100);
            let recursive = make_test_closure(context, bytes, 5, 2);
            let raw = context.raw();
            let undefined = raw.undefined();
            let depth = raw.smi(6);

            let result = expect_eval_ok(call_function(
                context,
                recursive,
                undefined,
                &[recursive.as_value(), depth],
            ));
            assert_eq!((*result).as_raw_bits(), Value::raw_smi(0).as_raw_bits());
            assert_eq!(observations(context), (1, 7, 0));
            assert_coherent(context);
            context.raw().vm().debug_assert_stack_empty();
        });
    }

    #[test]
    fn mutually_recursive_artifacts_remain_pinned_across_nested_entry() {
        let bytes = mutual_recursive_call_program();
        let mut owned = ContextBuilder::new().build().unwrap();
        owned.with_jit_context(|context| {
            configure(context, 1, 100);
            let first = make_test_closure(context, bytes.clone(), 6, 3);
            let second = make_test_closure(context, bytes, 6, 3);
            let raw = context.raw();
            let undefined = raw.undefined();
            let depth = raw.smi(5);

            let result = expect_eval_ok(call_function(
                context,
                first,
                undefined,
                &[first.as_value(), second.as_value(), depth],
            ));
            assert_eq!((*result).as_raw_bits(), Value::raw_smi(0).as_raw_bits());
            assert_eq!(observations(context), (2, 6, 0));
            assert_coherent(context);
            context.raw().vm().debug_assert_stack_empty();
        });
    }

    #[test]
    fn cache_full_nested_entry_never_evicts_executing_artifact() {
        let mut inner_bytes =
            encode(OpCode::AddImm, &[local(0), FIRST_ARGUMENT_SLOT_INDEX as u8, 1]);
        inner_bytes.extend(encode(OpCode::Ret, &[local(0)]));
        let mut outer_bytes =
            encode(OpCode::Mov, &[local(0), (FIRST_ARGUMENT_SLOT_INDEX + 1) as u8]);
        outer_bytes.extend(encode(
            OpCode::Call,
            &[local(1), FIRST_ARGUMENT_SLOT_INDEX as u8, local(0), 1],
        ));
        outer_bytes.extend(encode(OpCode::Ret, &[local(1)]));

        let mut owned = ContextBuilder::new().build().unwrap();
        owned.with_jit_context(|context| {
            configure(context, 1, 100);
            context.raw().jit_dispatch().set_entry_limit_for_test(1);
            let inner = make_test_closure(context, inner_bytes, 1, 1);
            let outer = make_test_closure(context, outer_bytes, 2, 2);
            let raw = context.raw();
            let undefined = raw.undefined();
            let argument = raw.smi(8);

            let nested = expect_eval_ok(call_function(
                context,
                outer,
                undefined,
                &[inner.as_value(), argument],
            ));
            assert_eq!((*nested).as_raw_bits(), Value::raw_smi(9).as_raw_bits());
            assert_eq!(observations(context), (1, 1, 1));
            assert_coherent(context);

            // Once the outer activation pin is gone, ordinary LRU retirement may replace it.
            let later = expect_eval_ok(call_function(context, inner, undefined, &[argument]));
            assert_eq!((*later).as_raw_bits(), Value::raw_smi(9).as_raw_bits());
            assert_eq!(observations(context), (2, 2, 1));
            assert_coherent(context);
            context.raw().vm().debug_assert_stack_empty();
        });
    }

    #[test]
    fn cold_nested_frame_panic_throw_and_recovery_restore_exact_parent() {
        let mut inner_bytes =
            encode(OpCode::AddImm, &[local(0), FIRST_ARGUMENT_SLOT_INDEX as u8, 1]);
        inner_bytes.extend(encode(OpCode::Ret, &[local(0)]));
        let throw_bytes = encode(OpCode::Throw, &[FIRST_ARGUMENT_SLOT_INDEX as u8]);
        let mut outer_bytes =
            encode(OpCode::Mov, &[local(0), (FIRST_ARGUMENT_SLOT_INDEX + 1) as u8]);
        outer_bytes.extend(encode(
            OpCode::Call,
            &[local(1), FIRST_ARGUMENT_SLOT_INDEX as u8, local(0), 1],
        ));
        outer_bytes.extend(encode(OpCode::Ret, &[local(1)]));

        let mut owned = ContextBuilder::new().build().unwrap();
        owned.with_jit_context(|context| {
            configure(context, 1, 100);
            context.raw().jit_dispatch().set_entry_limit_for_test(1);
            let inner = make_test_closure(context, inner_bytes, 1, 1);
            let throwing = make_test_closure(context, throw_bytes, 0, 1);
            let outer = make_test_closure(context, outer_bytes, 2, 2);
            let raw = context.raw();
            let undefined = raw.undefined();
            let argument = raw.smi(8);

            let panicked = catch_unwind(AssertUnwindSafe(|| {
                with_test_active_jit_fallback_dispatch_panic(|| {
                    let _ = call_function(context, outer, undefined, &[inner.as_value(), argument]);
                })
            }));
            assert!(panicked.is_err());
            assert!(!context.has_registered_jit_frame());
            context.raw().vm().debug_assert_stack_empty();
            assert_coherent(context);

            let thrown = call_function(context, outer, undefined, &[throwing.as_value(), argument]);
            match thrown {
                Err(crate::runtime::eval_result::EvalError::Value(error)) => {
                    assert_eq!((*error).as_raw_bits(), Value::raw_smi(8).as_raw_bits());
                }
                _ => panic!("cold nested throw did not escape with the exact value"),
            }
            assert!(!context.has_registered_jit_frame());
            context.raw().vm().debug_assert_stack_empty();
            assert_coherent(context);

            let recovered = expect_eval_ok(call_function(
                context,
                outer,
                undefined,
                &[inner.as_value(), argument],
            ));
            assert_eq!((*recovered).as_raw_bits(), Value::raw_smi(9).as_raw_bits());
            assert!(!context.has_registered_jit_frame());
            context.raw().vm().debug_assert_stack_empty();
            assert_coherent(context);
        });
    }

    #[test]
    fn tier_throw_is_caught_by_interpreted_handler_or_escapes_uncaught_once() {
        let mut throwing_bytes = encode(OpCode::LoadImmediate, &[local(0), 91]);
        throwing_bytes.extend(encode(OpCode::Throw, &[local(0)]));

        let mut outer_bytes =
            encode(OpCode::Call, &[local(0), FIRST_ARGUMENT_SLOT_INDEX as u8, local(0), 0]);
        let call_end = outer_bytes.len();
        outer_bytes.extend(encode(OpCode::Ret, &[local(0)]));
        let handler_offset = outer_bytes.len();
        outer_bytes.extend(encode(OpCode::LoadImmediate, &[local(1), 44]));
        outer_bytes.extend(encode(OpCode::Ret, &[local(1)]));
        let mut handler = ExceptionHandlerBuilder::new(0, call_end);
        handler.handler = handler_offset;

        let mut owned = ContextBuilder::new().build().unwrap();
        owned.with_jit_context(|context| {
            configure(context, 1, 100);
            let throwing = make_test_closure(context, throwing_bytes, 1, 0);
            let catching = make_test_closure_with_handler(context, outer_bytes, 2, 1, handler);
            let undefined = context.raw().undefined();

            let caught =
                expect_eval_ok(call_function(context, catching, undefined, &[throwing.as_value()]));
            assert_eq!((*caught).as_raw_bits(), Value::raw_smi(44).as_raw_bits());
            assert!(!context.has_registered_jit_frame());
            context.raw().vm().debug_assert_stack_empty();
            assert_coherent(context);

            let uncaught = call_function(context, throwing, undefined, &[]);
            match uncaught {
                Err(crate::runtime::eval_result::EvalError::Value(error)) => {
                    assert_eq!((*error).as_raw_bits(), Value::raw_smi(91).as_raw_bits());
                }
                _ => panic!("tier throw did not escape once with its exact value"),
            }
            assert!(!context.has_registered_jit_frame());
            context.raw().vm().debug_assert_stack_empty();
            assert_coherent(context);
        });
    }

    #[cfg(feature = "alloc_error")]
    #[test]
    fn cold_nested_allocation_failure_restores_exact_parent_and_recovers() {
        let inner_bytes = encode(OpCode::Ret, &[FIRST_ARGUMENT_SLOT_INDEX as u8]);
        let mut outer_bytes =
            encode(OpCode::Mov, &[local(0), (FIRST_ARGUMENT_SLOT_INDEX + 1) as u8]);
        outer_bytes.extend(encode(
            OpCode::Call,
            &[local(1), FIRST_ARGUMENT_SLOT_INDEX as u8, local(0), 1],
        ));
        outer_bytes.extend(encode(OpCode::Ret, &[local(1)]));

        let mut owned = ContextBuilder::new().build().unwrap();
        owned.with_jit_context(|context| {
            configure(context, 1, 100);
            context.raw().jit_dispatch().set_entry_limit_for_test(1);
            let inner = make_test_closure(context, inner_bytes, 0, 1);
            let outer = make_test_closure(context, outer_bytes, 2, 2);
            let raw = context.raw();
            let undefined = raw.undefined();
            let argument = raw.smi(19);

            let failed = catch_unwind(AssertUnwindSafe(|| {
                with_test_active_jit_fallback_allocation_failure(|| {
                    let _ = call_function(context, outer, undefined, &[inner.as_value(), argument]);
                })
            }));
            assert!(failed.is_err());
            assert!(!context.has_registered_jit_frame());
            context.raw().vm().debug_assert_stack_empty();
            assert_coherent(context);

            let recovered = expect_eval_ok(call_function(
                context,
                outer,
                undefined,
                &[inner.as_value(), argument],
            ));
            assert_eq!((*recovered).as_raw_bits(), Value::raw_smi(19).as_raw_bits());
            context.raw().vm().debug_assert_stack_empty();
            assert_coherent(context);
        });
    }

    #[test]
    fn outer_resume_budget_never_validates_a_cold_callee_program() {
        let inner_bytes = encode(OpCode::Ret, &[FIRST_ARGUMENT_SLOT_INDEX as u8]);
        let mut outer_bytes = encode(OpCode::LoadImmediate, &[local(2), 0]);
        outer_bytes.extend(encode(OpCode::LoadImmediate, &[local(3), 2]));
        let loop_offset = outer_bytes.len();
        outer_bytes.extend(encode(OpCode::Mov, &[local(0), (FIRST_ARGUMENT_SLOT_INDEX + 1) as u8]));
        outer_bytes.extend(encode(
            OpCode::Call,
            &[local(1), FIRST_ARGUMENT_SLOT_INDEX as u8, local(0), 1],
        ));
        outer_bytes.extend(encode(OpCode::AddImm, &[local(2), local(2), 1]));
        outer_bytes.extend(encode(OpCode::LessThan, &[local(4), local(2), local(3)]));
        let branch_offset = outer_bytes.len();
        outer_bytes.extend(encode(
            OpCode::JumpTrue,
            &[
                local(4),
                (loop_offset as isize - branch_offset as isize) as i8 as u8,
            ],
        ));
        outer_bytes.extend(encode(OpCode::Ret, &[local(1)]));

        let mut owned = ContextBuilder::new().build().unwrap();
        owned.with_jit_context(|context| {
            configure(context, 1, 100);
            context.raw().jit_dispatch().set_entry_limit_for_test(1);
            let inner = make_test_closure(context, inner_bytes, 0, 1);
            let outer = make_test_closure(context, outer_bytes, 5, 2);
            let raw = context.raw();
            let undefined = raw.undefined();
            let argument = raw.smi(27);
            context
                .raw()
                .jit_dispatch()
                .request_next_entry_interrupt_for_test();

            let (interrupted, polls) =
                with_test_backedge_poll_behavior(TestBackedgePollBehavior::Normal, || {
                    catch_unwind(AssertUnwindSafe(|| {
                        let _ =
                            call_function(context, outer, undefined, &[inner.as_value(), argument]);
                    }))
                });
            assert!(interrupted.is_err());
            assert_eq!(
                polls.calls, 0,
                "the outer VM-resume budget, not a generated backedge helper, interrupted"
            );
            assert!(!context.has_registered_jit_frame());
            context.raw().vm().debug_assert_stack_empty();
            assert_coherent(context);

            let recovered = expect_eval_ok(call_function(
                context,
                outer,
                undefined,
                &[inner.as_value(), argument],
            ));
            assert_eq!((*recovered).as_raw_bits(), Value::raw_smi(27).as_raw_bits());
            context.raw().vm().debug_assert_stack_empty();
            assert_coherent(context);
        });
    }

    #[test]
    fn nested_tier_interrupt_and_panic_unwind_outer_resume_without_replay() {
        let inner_bytes = finite_loop(4);
        let mut outer_bytes =
            encode(OpCode::Call, &[local(0), FIRST_ARGUMENT_SLOT_INDEX as u8, local(0), 0]);
        outer_bytes.extend(encode(OpCode::Ret, &[local(0)]));

        let mut owned = ContextBuilder::new().build().unwrap();
        owned.with_jit_context(|context| {
            configure(context, 1, 100);
            let inner = make_test_closure(context, inner_bytes, 3, 0);
            let outer = make_test_closure(context, outer_bytes, 1, 1);
            let undefined = context.raw().undefined();

            let warmed = expect_eval_ok(call_function(context, inner, undefined, &[]));
            assert_eq!((*warmed).as_raw_bits(), Value::raw_smi(4).as_raw_bits());

            for behavior in [
                TestBackedgePollBehavior::Normal,
                TestBackedgePollBehavior::Panic,
            ] {
                context
                    .raw()
                    .jit_dispatch()
                    .request_next_nested_entry_interrupt_for_test();
                let (terminal, polls) = with_test_backedge_poll_behavior(behavior, || {
                    catch_unwind(AssertUnwindSafe(|| {
                        let _ = call_function(context, outer, undefined, &[inner.as_value()]);
                    }))
                });
                assert!(terminal.is_err());
                assert_eq!(polls.calls, 1);
                assert!(!context.has_registered_jit_frame());
                context.raw().vm().debug_assert_stack_empty();
                assert_coherent(context);
            }

            let recovered =
                expect_eval_ok(call_function(context, outer, undefined, &[inner.as_value()]));
            assert_eq!((*recovered).as_raw_bits(), Value::raw_smi(4).as_raw_bits());
            context.raw().vm().debug_assert_stack_empty();
            assert_coherent(context);
        });
    }

    #[test]
    fn nested_allocating_tier_failure_collects_once_and_recovers_without_replay() {
        let mut inner_bytes = encode(OpCode::NewObject, &[local(0), 0]);
        inner_bytes.extend(encode(OpCode::Ret, &[local(0)]));
        let mut outer_bytes =
            encode(OpCode::Call, &[local(0), FIRST_ARGUMENT_SLOT_INDEX as u8, local(0), 0]);
        outer_bytes.extend(encode(OpCode::Ret, &[local(0)]));

        let mut owned = ContextBuilder::new().build().unwrap();
        owned.with_jit_context(|context| {
            configure(context, 1, 100);
            let inner = make_test_closure(context, inner_bytes, 1, 0);
            let outer = make_test_closure(context, outer_bytes, 1, 1);
            let undefined = context.raw().undefined();
            assert!(expect_eval_ok(call_function(context, inner, undefined, &[])).is_object());

            for behavior in [
                TestHelperBehavior::ForceCollectionThenAllocationFailure,
                TestHelperBehavior::ForceCollectionThenPanic,
            ] {
                let (terminal, helper) = with_test_helper_behavior(behavior, || {
                    catch_unwind(AssertUnwindSafe(|| {
                        let _ = call_function(context, outer, undefined, &[inner.as_value()]);
                    }))
                });
                assert!(terminal.is_err());
                assert_eq!(helper.calls, 1, "committed nested helper must not replay");
                assert!(!context.has_registered_jit_frame());
                context.raw().vm().debug_assert_stack_empty();
                assert_coherent(context);
            }

            let (recovered, helper) = with_test_helper_behavior(TestHelperBehavior::Normal, || {
                call_function(context, outer, undefined, &[inner.as_value()])
            });
            assert!(expect_eval_ok(recovered).is_object());
            assert_eq!(helper.calls, 1);
            context.raw().vm().debug_assert_stack_empty();
            assert_coherent(context);
        });
    }

    #[test]
    fn extra_actual_arguments_survive_native_side_exit_and_feed_rest_parameter() {
        let mut bytes = encode(OpCode::RestParameter, &[local(0)]);
        bytes.extend(encode(OpCode::Ret, &[local(0)]));

        let mut owned = ContextBuilder::new().build().unwrap();
        owned.with_jit_context(|context| {
            configure(context, 1, 100);
            let closure = make_test_closure(context, bytes, 1, 1);
            let raw = context.raw();
            let undefined = raw.undefined();
            let first = raw.smi(4);
            let second = raw.smi(9);
            let third = raw.smi(11);

            let result =
                expect_eval_ok(call_function(context, closure, undefined, &[first, second, third]));
            let properties = result.as_object().array_properties().as_dense();
            assert_eq!(properties.len(), 2);
            assert_eq!(properties.as_slice()[0].as_raw_bits(), Value::raw_smi(9).as_raw_bits());
            assert_eq!(properties.as_slice()[1].as_raw_bits(), Value::raw_smi(11).as_raw_bits());

            let missing = expect_eval_ok(call_function(context, closure, undefined, &[]));
            assert_eq!(missing.as_object().array_properties().as_dense().len(), 0);
            assert_eq!(observations(context), (1, 2, 0));
            assert_coherent(context);
            context.raw().vm().debug_assert_stack_empty();
        });
    }

    #[test]
    fn strict_and_sloppy_receiver_rules_survive_nested_tier_entry() {
        let receiver_bytes = encode(OpCode::Ret, &[RECEIVER_SLOT_INDEX as u8]);
        let mut explicit_outer_bytes = encode(
            OpCode::CallWithReceiver,
            &[
                local(0),
                FIRST_ARGUMENT_SLOT_INDEX as u8,
                (FIRST_ARGUMENT_SLOT_INDEX + 1) as u8,
                local(0),
                0,
            ],
        );
        explicit_outer_bytes.extend(encode(OpCode::Ret, &[local(0)]));
        let mut implicit_outer_bytes =
            encode(OpCode::Call, &[local(0), FIRST_ARGUMENT_SLOT_INDEX as u8, local(0), 0]);
        implicit_outer_bytes.extend(encode(OpCode::Ret, &[local(0)]));

        let mut owned = ContextBuilder::new().build().unwrap();
        owned.with_jit_context(|context| {
            configure(context, 1, 100);
            let strict =
                make_semantic_test_closure(context, receiver_bytes.clone(), 0, 0, true, None, None);
            let sloppy =
                make_semantic_test_closure(context, receiver_bytes, 0, 0, false, None, None);
            let explicit_outer = make_test_closure(context, explicit_outer_bytes, 1, 2);
            let implicit_outer = make_test_closure(context, implicit_outer_bytes, 1, 1);
            let raw = context.raw();
            let undefined = raw.undefined();
            let primitive = raw.smi(31);
            let null = raw.null();
            let object_receiver = strict.as_value();
            let global_bits = raw.initial_realm().global_object().as_value().as_raw_bits();

            let strict_primitive = expect_eval_ok(call_function(
                context,
                explicit_outer,
                undefined,
                &[strict.as_value(), primitive],
            ));
            assert_eq!((*strict_primitive).as_raw_bits(), Value::raw_smi(31).as_raw_bits());

            let strict_implicit = expect_eval_ok(call_function(
                context,
                implicit_outer,
                undefined,
                &[strict.as_value()],
            ));
            assert!(strict_implicit.is_undefined());

            let sloppy_null = expect_eval_ok(call_function(
                context,
                explicit_outer,
                undefined,
                &[sloppy.as_value(), null],
            ));
            assert_eq!((*sloppy_null).as_raw_bits(), global_bits);

            let sloppy_primitive = expect_eval_ok(call_function(
                context,
                explicit_outer,
                undefined,
                &[sloppy.as_value(), primitive],
            ));
            assert!(sloppy_primitive.is_object());

            let sloppy_object = expect_eval_ok(call_function(
                context,
                explicit_outer,
                undefined,
                &[sloppy.as_value(), object_receiver],
            ));
            assert_eq!((*sloppy_object).as_raw_bits(), (*object_receiver).as_raw_bits());
            assert_coherent(context);
            context.raw().vm().debug_assert_stack_empty();
        });
    }

    #[test]
    fn construct_preserves_new_target_padding_and_base_derived_return_rules() {
        let new_target_bytes = encode(OpCode::Ret, &[local(0)]);
        let primitive_bytes = {
            let mut bytes = encode(OpCode::LoadImmediate, &[local(0), 5]);
            bytes.extend(encode(OpCode::Ret, &[local(0)]));
            bytes
        };
        let object_bytes = encode(OpCode::Ret, &[FIRST_ARGUMENT_SLOT_INDEX as u8]);

        let mut no_arg_outer_bytes = encode(
            OpCode::Construct,
            &[
                local(0),
                FIRST_ARGUMENT_SLOT_INDEX as u8,
                (FIRST_ARGUMENT_SLOT_INDEX + 1) as u8,
                local(0),
                0,
            ],
        );
        no_arg_outer_bytes.extend(encode(OpCode::Ret, &[local(0)]));
        let mut one_arg_outer_bytes =
            encode(OpCode::Mov, &[local(0), (FIRST_ARGUMENT_SLOT_INDEX + 2) as u8]);
        one_arg_outer_bytes.extend(encode(
            OpCode::Construct,
            &[
                local(1),
                FIRST_ARGUMENT_SLOT_INDEX as u8,
                (FIRST_ARGUMENT_SLOT_INDEX + 1) as u8,
                local(0),
                1,
            ],
        ));
        one_arg_outer_bytes.extend(encode(OpCode::Ret, &[local(1)]));

        let mut owned = ContextBuilder::new().build().unwrap();
        owned.with_jit_context(|context| {
            configure(context, 1, 100);
            let returns_new_target = make_semantic_test_closure(
                context,
                new_target_bytes,
                1,
                2,
                true,
                Some(true),
                Some(0),
            );
            let base_primitive = make_semantic_test_closure(
                context,
                primitive_bytes.clone(),
                1,
                0,
                true,
                Some(true),
                None,
            );
            let base_object = make_semantic_test_closure(
                context,
                object_bytes.clone(),
                0,
                1,
                true,
                Some(true),
                None,
            );
            let derived_object =
                make_semantic_test_closure(context, object_bytes, 0, 1, true, Some(false), None);
            let derived_primitive =
                make_semantic_test_closure(context, primitive_bytes, 1, 0, true, Some(false), None);
            let distinct_new_target = make_semantic_test_closure(
                context,
                encode(OpCode::Ret, &[RECEIVER_SLOT_INDEX as u8]),
                0,
                0,
                true,
                Some(true),
                None,
            );
            let no_arg_outer = make_test_closure(context, no_arg_outer_bytes, 1, 2);
            let one_arg_outer = make_test_closure(context, one_arg_outer_bytes, 2, 3);
            let raw = context.raw();
            let undefined = raw.undefined();
            let object_argument = distinct_new_target.as_value();

            let exact_new_target = expect_eval_ok(call_function(
                context,
                no_arg_outer,
                undefined,
                &[
                    returns_new_target.as_value(),
                    distinct_new_target.as_value(),
                ],
            ));
            assert_eq!(
                (*exact_new_target).as_raw_bits(),
                (*distinct_new_target.as_value()).as_raw_bits(),
                "new.target must occupy its dedicated local despite two missing formals"
            );

            let created_receiver = expect_eval_ok(call_function(
                context,
                no_arg_outer,
                undefined,
                &[base_primitive.as_value(), base_primitive.as_value()],
            ));
            assert!(created_receiver.is_object());

            let base_override = expect_eval_ok(call_function(
                context,
                one_arg_outer,
                undefined,
                &[
                    base_object.as_value(),
                    base_object.as_value(),
                    object_argument,
                ],
            ));
            assert_eq!((*base_override).as_raw_bits(), (*object_argument).as_raw_bits());

            let derived_override = expect_eval_ok(call_function(
                context,
                one_arg_outer,
                undefined,
                &[
                    derived_object.as_value(),
                    derived_object.as_value(),
                    object_argument,
                ],
            ));
            assert_eq!((*derived_override).as_raw_bits(), (*object_argument).as_raw_bits());

            let derived_error = call_function(
                context,
                no_arg_outer,
                undefined,
                &[derived_primitive.as_value(), derived_primitive.as_value()],
            );
            assert!(matches!(derived_error, Err(crate::runtime::eval_result::EvalError::Value(_))));
            assert_coherent(context);
            context.raw().vm().debug_assert_stack_empty();
        });
    }

    #[test]
    fn moving_gc_updates_active_dispatch_root_receiver_and_exact_argument_slots() {
        let mut bytes = encode(OpCode::NewObject, &[local(0), 0]);
        bytes.extend(encode(OpCode::Mov, &[local(1), RECEIVER_SLOT_INDEX as u8]));
        bytes.extend(encode(OpCode::Mov, &[local(2), FIRST_ARGUMENT_SLOT_INDEX as u8]));
        bytes.extend(encode(OpCode::Ret, &[local(1)]));

        let mut owned = ContextBuilder::new().build().unwrap();
        owned.with_jit_context(|context| {
            configure(context, 1, 100);
            let closure = make_test_closure(context, bytes, 3, 1);
            let mut raw = context.raw();
            let receiver = expect_eval_ok(raw.alloc_string("moving receiver")).as_value();
            let argument = expect_eval_ok(raw.alloc_string("moving argument")).as_value();
            let function_before = closure.function_ptr().as_ptr() as usize;
            let receiver_before = (*receiver).as_raw_bits();
            let argument_before = (*argument).as_raw_bits();

            let (result, helper) = with_test_helper_behavior(
                TestHelperBehavior::ForceCollectionAfterAllocation,
                || call_function(context, closure, receiver, &[argument]),
            );
            let result = expect_eval_ok(result);
            assert_eq!(helper.calls, 1);
            assert_ne!(closure.function_ptr().as_ptr() as usize, function_before);
            assert_ne!((*receiver).as_raw_bits(), receiver_before);
            assert_ne!((*argument).as_raw_bits(), argument_before);
            assert_eq!((*result).as_raw_bits(), (*receiver).as_raw_bits());
            assert!(result.is::<StringValue>());

            let (reused, helper) = with_test_helper_behavior(TestHelperBehavior::Normal, || {
                call_function(context, closure, receiver, &[argument])
            });
            let reused = expect_eval_ok(reused);
            assert_eq!(helper.calls, 1);
            assert_eq!((*reused).as_raw_bits(), (*receiver).as_raw_bits());

            let mut state_raw = context.raw();
            let roots_raw = state_raw;
            let state = state_raw.jit_dispatch();
            assert_eq!(
                state.rooted_function_address_for_test(roots_raw),
                Some(closure.function_ptr().as_ptr() as usize)
            );
            assert_eq!(state.observations_for_test(), (1, 2, 0));
            assert!(state.artifact_coherent_for_test(roots_raw));
            context.raw().vm().debug_assert_stack_empty();
        });
    }

    #[test]
    fn preflight_rejection_is_negative_cached_and_never_duplicates_execution() {
        let mut bytes = encode(OpCode::Neg, &[local(0), FIRST_ARGUMENT_SLOT_INDEX as u8]);
        bytes.extend(encode(OpCode::Ret, &[local(0)]));

        let mut owned = ContextBuilder::new().build().unwrap();
        owned.with_jit_context(|context| {
            configure(context, 1, 100);
            let closure = make_test_closure(context, bytes, 1, 1);
            let mut raw = context.raw();
            let receiver = raw.undefined();
            let argument = expect_eval_ok(raw.alloc_string("3")).as_value();

            for _ in 0..2 {
                let result = expect_eval_ok(call_function(context, closure, receiver, &[argument]));
                assert!(result.is_number());
            }

            assert_eq!(observations(context), (1, 0, 2));
            let mut state_raw = context.raw();
            let roots_raw = state_raw;
            let state = state_raw.jit_dispatch();
            assert_eq!(state.root_count_for_test(roots_raw), 1);
            assert!(state.artifact_coherent_for_test(roots_raw));
            context.raw().vm().debug_assert_stack_empty();
        });
    }

    #[test]
    fn moving_gc_before_clean_fallback_refreshes_enclosing_closure_and_receiver_roots() {
        let mut bytes = encode(OpCode::Mov, &[local(0), RECEIVER_SLOT_INDEX as u8]);
        bytes.extend(encode(OpCode::ToNumber, &[local(1), local(0)]));
        bytes.extend(encode(OpCode::Ret, &[local(0)]));

        let mut owned = ContextBuilder::new().build().unwrap();
        owned.with_jit_context(|context| {
            configure(context, 1, 100);
            let closure = make_test_closure(context, bytes, 2, 0);
            let mut raw = context.raw();
            let receiver = expect_eval_ok(raw.alloc_string("moving fallback receiver")).as_value();
            let function_before = closure.function_ptr().as_ptr() as usize;
            let receiver_before = (*receiver).as_raw_bits();
            raw.jit_dispatch()
                .collect_before_next_preentry_decision_for_test();

            let result = expect_eval_ok(call_function(context, closure, receiver, &[]));
            assert_ne!(closure.function_ptr().as_ptr() as usize, function_before);
            assert_ne!((*receiver).as_raw_bits(), receiver_before);
            assert_eq!((*result).as_raw_bits(), (*receiver).as_raw_bits());
            assert!(result.is::<StringValue>());
            assert_eq!(observations(context), (1, 0, 1));
            assert_coherent(context);

            let function_before_reuse = closure.function_ptr().as_ptr() as usize;
            let receiver_before_reuse = (*receiver).as_raw_bits();
            context
                .raw()
                .jit_dispatch()
                .collect_before_next_preentry_decision_for_test();
            let reused_negative = expect_eval_ok(call_function(context, closure, receiver, &[]));
            assert_ne!(closure.function_ptr().as_ptr() as usize, function_before_reuse);
            assert_ne!((*receiver).as_raw_bits(), receiver_before_reuse);
            assert_eq!((*reused_negative).as_raw_bits(), (*receiver).as_raw_bits());
            assert_eq!(observations(context), (1, 0, 2));
            context.raw().vm().debug_assert_stack_empty();
        });
    }

    #[test]
    fn noninitial_reentrant_new_object_falls_back_to_the_callee_realm_after_moving_gc() {
        let mut bytes = encode(OpCode::NewObject, &[local(0), 0]);
        bytes.extend(encode(OpCode::Ret, &[local(0)]));

        let mut owned = ContextBuilder::new().build().unwrap();
        owned.with_jit_context(|context| {
            configure(context, 1, 100);
            let closure = make_test_closure(context, bytes, 1, 0);
            let mut raw = context.raw();
            let other_realm = Realm::new(raw).unwrap();
            let initial_prototype = raw
                .initial_realm()
                .get_intrinsic(Intrinsic::ObjectPrototype);
            let other_prototype = other_realm.get_intrinsic(Intrinsic::ObjectPrototype);
            assert!(!(*initial_prototype).ptr_eq(&*other_prototype));

            let function_before = closure.function_ptr().as_ptr() as usize;
            let realm_before = other_realm.as_ptr() as usize;
            Heap::run_gc(raw, GcType::Normal);
            assert_ne!(closure.function_ptr().as_ptr() as usize, function_before);
            assert_ne!(other_realm.as_ptr() as usize, realm_before);

            let receiver = raw.undefined();
            let result =
                expect_eval_ok(raw.with_initial_realm_stack_frame(*other_realm, |mut raw| {
                    raw.vm().call_from_rust(closure.cast(), receiver, &[])
                }));
            let result_prototype = result
                .as_object()
                .prototype()
                .expect("NewObject has the callee realm Object prototype");
            assert!(result_prototype.ptr_eq(&*initial_prototype));
            assert!(!result_prototype.ptr_eq(&*other_prototype));
            assert_eq!(observations(context), (0, 0, 1));
            let mut raw = context.raw();
            let roots_raw = raw;
            assert_eq!(raw.jit_dispatch().root_count_for_test(roots_raw), 0);
            raw.vm().debug_assert_stack_empty();
        });
    }

    #[test]
    fn stale_missing_rx_artifact_is_terminal_without_recompile_or_fallback() {
        const CHILD_MARKER: &str = "WILD_BUZZARD_STALE_RX_ABORT_CHILD";

        if std::env::var_os(CHILD_MARKER).is_none() {
            use std::{os::unix::process::ExitStatusExt, process::Command};

            let output = Command::new(std::env::current_exe().expect("current test executable"))
                .args([
                    "--exact",
                    "runtime::jit::dispatch::tests::stale_missing_rx_artifact_is_terminal_without_recompile_or_fallback",
                    "--nocapture",
                ])
                .env(CHILD_MARKER, "1")
                .env("RUST_BACKTRACE", "0")
                .output()
                .expect("launch stale-cache teardown child");
            assert_eq!(
                output.status.signal(),
                Some(libc::SIGABRT),
                "corrupt dispatch/cache teardown must abort; status={:?}, stdout={}, stderr={}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
            return;
        }

        let bytes = encode(OpCode::Ret, &[RECEIVER_SLOT_INDEX as u8]);
        let mut owned = ContextBuilder::new().build().unwrap();
        owned.with_jit_context(|context| {
            configure(context, 1, 100);
            let closure = make_test_closure(context, bytes, 0, 0);
            let receiver = context.raw().smi(7);
            let result = expect_eval_ok(call_function(context, closure, receiver, &[]));
            assert_eq!((*result).as_raw_bits(), Value::raw_smi(7).as_raw_bits());
            context
                .raw()
                .jit_dispatch()
                .retire_first_code_only_for_test();

            let stale = catch_unwind(AssertUnwindSafe(|| {
                let _ = call_function(context, closure, receiver, &[]);
            }));
            assert!(stale.is_err());
            assert_eq!(observations(context), (1, 1, 0));
            assert!(!context.has_registered_jit_frame());
            context.raw().vm().debug_assert_stack_empty();

            let mut state_raw = context.raw();
            let roots_raw = state_raw;
            let state = state_raw.jit_dispatch();
            assert!(!state.artifact_coherent_for_test(roots_raw));
        });

        // `OwnedContext::drop` must observe that metadata still claims an RX artifact while the
        // exact cache mapping is missing and fail closed. Reaching the next statement is a bug.
        drop(owned);
        panic!("corrupt dispatch/cache teardown returned instead of aborting");
    }

    #[test]
    fn interrupt_and_poison_are_terminal_then_cleanup_allows_recovery() {
        let mut owned = ContextBuilder::new().build().unwrap();
        owned.with_jit_context(|context| {
            configure(context, 1, 100);
            let closure = make_test_closure(context, finite_loop(4), 3, 0);
            let receiver = context.raw().undefined();

            context
                .raw()
                .jit_dispatch()
                .request_next_entry_interrupt_for_test();

            let (interrupted, interrupt_polls) =
                with_test_backedge_poll_behavior(TestBackedgePollBehavior::Normal, || {
                    catch_unwind(AssertUnwindSafe(|| {
                        let _ = call_function(context, closure, receiver, &[]);
                    }))
                });
            assert!(interrupted.is_err());
            assert_eq!(interrupt_polls.calls, 1);
            assert!(!context.has_registered_jit_frame());
            context.raw().vm().debug_assert_stack_empty();
            assert_coherent(context);

            context
                .raw()
                .jit_dispatch()
                .set_interrupt_quantum_for_test(nonzero(1));
            let (exhausted, quantum_polls) =
                with_test_backedge_poll_behavior(TestBackedgePollBehavior::Normal, || {
                    catch_unwind(AssertUnwindSafe(|| {
                        let _ = call_function(context, closure, receiver, &[]);
                    }))
                });
            assert!(exhausted.is_err());
            assert_eq!(quantum_polls.calls, 1);
            assert!(!context.has_registered_jit_frame());
            context.raw().vm().debug_assert_stack_empty();
            assert_coherent(context);

            context
                .raw()
                .jit_dispatch()
                .set_interrupt_quantum_for_test(nonzero(100));
            let (poisoned, poison_polls) =
                with_test_backedge_poll_behavior(TestBackedgePollBehavior::Panic, || {
                    catch_unwind(AssertUnwindSafe(|| {
                        let _ = call_function(context, closure, receiver, &[]);
                    }))
                });
            assert!(poisoned.is_err());
            assert_eq!(poison_polls.calls, 1);
            assert!(!context.has_registered_jit_frame());
            context.raw().vm().debug_assert_stack_empty();
            assert_coherent(context);

            let (recovered, recovery_polls) =
                with_test_backedge_poll_behavior(TestBackedgePollBehavior::Normal, || {
                    call_function(context, closure, receiver, &[])
                });
            let recovered = expect_eval_ok(recovered);
            assert_eq!((*recovered).as_raw_bits(), Value::raw_smi(4).as_raw_bits());
            assert_eq!(recovery_polls.calls, 0);
            assert_eq!(observations(context), (1, 4, 0));
            assert_coherent(context);
        });
    }

    #[test]
    fn artifact_lru_eviction_releases_exact_generation_checked_root() {
        let bytes = encode(OpCode::Ret, &[RECEIVER_SLOT_INDEX as u8]);
        let mut owned = ContextBuilder::new().build().unwrap();
        owned.with_jit_context(|context| {
            configure(context, 1, 100);
            context.raw().jit_dispatch().set_entry_limit_for_test(2);

            let mut closures = Vec::new();
            let mut evicted_root = None;
            for value in 1..=3 {
                let closure = make_test_closure(context, bytes.clone(), 0, 0);
                closures.push(closure);
                let receiver = context.raw().smi(value);
                let result = expect_eval_ok(call_function(context, closure, receiver, &[]));
                assert_eq!((*result).as_raw_bits(), Value::raw_smi(value).as_raw_bits());
                if value == 1 {
                    evicted_root = context.raw().jit_dispatch().first_root_for_test();
                }
            }

            let evicted_root = evicted_root.expect("first function acquired a root");
            assert!(
                context
                    .raw()
                    .jit_dispatch_roots()
                    .get(evicted_root)
                    .is_none()
            );
            assert_eq!(observations(context), (3, 3, 0));
            assert_coherent(context);

            let mut state_raw = context.raw();
            let roots_raw = state_raw;
            let state = state_raw.jit_dispatch();
            assert_eq!(state.root_count_for_test(roots_raw), 2);
            state.shutdown(roots_raw);
            assert_eq!(state.root_count_for_test(roots_raw), 0);
            assert!(state.artifact_coherent_for_test(roots_raw));
            drop(closures);
        });
    }
}
