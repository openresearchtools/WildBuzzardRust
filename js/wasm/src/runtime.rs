use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use wasmtime::{
    Collector, Config, Engine, Extern, Instance, InstanceAllocationStrategy, Module, Store,
    StoreLimits, StoreLimitsBuilder, Strategy, Trap, Val, ValType, WasmBacktraceDetails,
};

use crate::identity::{InstanceId, ModuleId, StoreId};
use crate::policy::INITIAL_PROPOSAL_POLICY;
use crate::registry::{Key, Registry};
use crate::{IdentityKind, WasmError, WasmLimits, WasmScalarType, WasmScalarValue};

static NEXT_OWNER: AtomicU64 = AtomicU64::new(1);

struct ModuleEntry {
    module: Module,
    resident_instances: usize,
}

struct StoreState {
    limits: StoreLimits,
}

struct StoreEntry {
    store: Store<StoreState>,
    resident_modules: Vec<ModuleId>,
    last_interrupt_sequence: u64,
}

#[derive(Clone, Copy)]
struct InstanceEntry {
    instance: Instance,
    module: ModuleId,
    store: StoreId,
}

struct InterruptControl {
    alive: AtomicBool,
    poisoned: AtomicBool,
    sequence: AtomicU64,
}

/// A capability-free external interruption handle.
///
/// This handle can only advance the owning engine's epoch. It cannot inspect a store, instance,
/// module, or Wasmtime handle.
#[derive(Clone)]
pub struct InterruptHandle {
    engine: Engine,
    control: Arc<InterruptControl>,
}

impl InterruptHandle {
    /// Requests terminal interruption of execution in the owning process engine.
    pub fn interrupt(&self) -> Result<(), WasmError> {
        if !self.control.alive.load(Ordering::Acquire) {
            return Err(WasmError::RuntimeClosed);
        }
        if self.control.poisoned.load(Ordering::Acquire) {
            return Err(WasmError::InterruptSequenceExhausted);
        }

        if self
            .control
            .sequence
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_add(1)
            })
            .is_err()
        {
            self.control.poisoned.store(true, Ordering::Release);
            self.engine.increment_epoch();
            return Err(WasmError::InterruptSequenceExhausted);
        }

        self.engine.increment_epoch();
        Ok(())
    }
}

/// Counts of currently reachable adapter identities and resident Wasmtime instances.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LiveCounts {
    pub modules: usize,
    pub stores: usize,
    pub instances: usize,
    /// Instances remain resident until their store is removed, even if their adapter ID was
    /// explicitly invalidated.
    pub resident_instances: usize,
}

/// Counts invalidated by a process reset or shutdown.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CleanupReport {
    pub modules: usize,
    pub stores: usize,
    pub instances: usize,
    pub resident_instances: usize,
}

/// Process-scoped owner of the sole Wasmtime engine and all opaque Wasm resources.
pub struct WasmProcess {
    engine: Engine,
    control: Arc<InterruptControl>,
    owner: u64,
    limits: WasmLimits,
    modules: Registry<ModuleEntry>,
    stores: Registry<StoreEntry>,
    instances: Registry<InstanceEntry>,
    resident_instances: usize,
}

impl WasmProcess {
    /// Creates a process-scoped engine with the exact initial feature and proposal policy.
    pub fn new(limits: WasmLimits) -> Result<Self, WasmError> {
        limits.validate()?;
        let engine = build_engine(&limits)?;
        let owner = allocate_owner()?;
        let control = Arc::new(InterruptControl {
            alive: AtomicBool::new(true),
            poisoned: AtomicBool::new(false),
            sequence: AtomicU64::new(0),
        });

        Ok(Self {
            engine,
            control,
            owner,
            limits,
            modules: Registry::new(),
            stores: Registry::new(),
            instances: Registry::new(),
            resident_instances: 0,
        })
    }

    /// Returns the immutable hard-limit policy for this process owner.
    pub fn limits(&self) -> &WasmLimits {
        &self.limits
    }

    /// Returns a cloneable handle which can only request epoch interruption.
    pub fn interrupt_handle(&self) -> InterruptHandle {
        InterruptHandle {
            engine: self.engine.clone(),
            control: Arc::clone(&self.control),
        }
    }

    /// Validates a bounded core binary without admitting or compiling it.
    pub fn validate_module(&self, bytes: &[u8]) -> Result<(), WasmError> {
        self.ensure_open()?;
        self.validate_binary_header(bytes)?;
        Module::validate(&self.engine, bytes).map_err(|error| WasmError::ValidationFailed {
            detail: error.to_string(),
        })
    }

    /// Validates and compiles a bounded, import-free core binary.
    pub fn compile_module(&mut self, bytes: &[u8]) -> Result<ModuleId, WasmError> {
        self.ensure_open()?;
        if self.modules.active() >= self.limits.max_modules {
            return Err(WasmError::CapacityExceeded {
                kind: IdentityKind::Module,
                maximum: self.limits.max_modules,
            });
        }
        self.validate_module(bytes)?;
        let reservation = self
            .modules
            .reserve(self.limits.max_modules, IdentityKind::Module)?;

        let module = match Module::new(&self.engine, bytes) {
            Ok(module) => module,
            Err(error) => {
                self.modules.cancel(reservation);
                return Err(WasmError::CompilationFailed {
                    detail: error.to_string(),
                });
            }
        };

        let import_error = {
            let mut imports = module.imports();
            let import_count = imports.len();
            imports.next().map(|first| WasmError::ImportsForbidden {
                count: import_count,
                first_module: first.module().to_owned(),
                first_name: first.name().to_owned(),
            })
        };
        if let Some(error) = import_error {
            self.modules.cancel(reservation);
            return Err(error);
        }

        let key = self.modules.commit(
            reservation,
            ModuleEntry {
                module,
                resident_instances: 0,
            },
        )?;
        Ok(ModuleId::new(self.owner, key))
    }

    /// Creates an empty, resource-limited Wasmtime store.
    pub fn create_store(&mut self) -> Result<StoreId, WasmError> {
        self.ensure_open()?;
        let reservation = self
            .stores
            .reserve(self.limits.max_stores, IdentityKind::Store)?;
        let store_limits = StoreLimitsBuilder::new()
            .memory_size(self.limits.max_memory_bytes)
            .table_elements(self.limits.max_table_elements)
            .instances(self.limits.max_instances_per_store)
            .memories(self.limits.max_memories_per_store)
            .tables(self.limits.max_tables_per_store)
            .trap_on_grow_failure(false)
            .build();
        let mut store = Store::new(
            &self.engine,
            StoreState {
                limits: store_limits,
            },
        );
        store.limiter(|state| &mut state.limits);
        store.epoch_deadline_trap();

        let key = self.stores.commit(
            reservation,
            StoreEntry {
                store,
                resident_modules: Vec::new(),
                last_interrupt_sequence: self.control.sequence.load(Ordering::Acquire),
            },
        )?;
        Ok(StoreId::new(self.owner, key))
    }

    /// Instantiates an admitted module with an empty import list.
    ///
    /// Fuel and the epoch deadline are installed before Wasmtime runs a possible start function.
    pub fn instantiate(
        &mut self,
        store_id: StoreId,
        module_id: ModuleId,
    ) -> Result<InstanceId, WasmError> {
        self.ensure_open()?;
        let module_key = self.module_key(module_id)?;
        let store_key = self.store_key(store_id)?;
        if self.resident_instances >= self.limits.max_instances {
            return Err(WasmError::CapacityExceeded {
                kind: IdentityKind::Instance,
                maximum: self.limits.max_instances,
            });
        }

        let module = self
            .modules
            .get(module_key)
            .ok_or(WasmError::StaleIdentity {
                kind: IdentityKind::Module,
            })?
            .module
            .clone();
        if module.imports().next().is_some() {
            return Err(WasmError::InternalInvariant {
                detail: "an admitted module contains an import",
            });
        }

        let resident_in_store = self
            .stores
            .get(store_key)
            .ok_or(WasmError::StaleIdentity {
                kind: IdentityKind::Store,
            })?
            .resident_modules
            .len();
        if resident_in_store >= self.limits.max_instances_per_store {
            return Err(WasmError::CapacityExceeded {
                kind: IdentityKind::Instance,
                maximum: self.limits.max_instances_per_store,
            });
        }

        let reservation = self
            .instances
            .reserve(self.limits.max_instances, IdentityKind::Instance)?;
        let fuel = self.limits.fuel_per_operation;
        {
            let store_entry = self
                .stores
                .get_mut(store_key)
                .ok_or(WasmError::StaleIdentity {
                    kind: IdentityKind::Store,
                })?;
            if store_entry.resident_modules.try_reserve(1).is_err() {
                self.instances.cancel(reservation);
                return Err(WasmError::HostAllocationFailed);
            }
            if let Err(error) = prepare_execution(store_entry, &self.control, fuel) {
                self.instances.cancel(reservation);
                return Err(error);
            }
            store_entry.resident_modules.push(module_id);
        }
        // Wasmtime releases instance allocations only with the Store and does not expose whether
        // a failed instantiation allocated before it failed. Charge every attempt conservatively
        // until store teardown so failures cannot bypass process or module dependency limits.
        self.resident_instances += 1;
        self.modules
            .get_mut(module_key)
            .ok_or(WasmError::InternalInvariant {
                detail: "module disappeared after synchronous instantiation",
            })?
            .resident_instances += 1;

        let instance = {
            let store_entry =
                self.stores
                    .get_mut(store_key)
                    .ok_or(WasmError::InternalInvariant {
                        detail: "store disappeared before synchronous instantiation",
                    })?;
            match Instance::new(&mut store_entry.store, &module, &[]) {
                Ok(instance) => instance,
                Err(error) => {
                    let mapped = map_execution_error(error, ExecutionPhase::Instantiation);
                    record_interrupt_if_needed(store_entry, &self.control, &mapped);
                    self.instances.cancel(reservation);
                    return Err(mapped);
                }
            }
        };

        let key = self.instances.commit(
            reservation,
            InstanceEntry {
                instance,
                module: module_id,
                store: store_id,
            },
        )?;
        Ok(InstanceId::new(self.owner, key))
    }

    /// Calls an exported function through the original i32-only value contract.
    ///
    /// This remains a checked compatibility wrapper over the scalar call primitive. Functions
    /// with any non-i32 parameter or result retain the original `UnsupportedSignature` behavior.
    pub fn call_i32(
        &mut self,
        store_id: StoreId,
        module_id: ModuleId,
        instance_id: InstanceId,
        export_name: &str,
        arguments: &[i32],
    ) -> Result<Vec<i32>, WasmError> {
        let results = self.call_scalar_primitive(
            store_id,
            module_id,
            instance_id,
            export_name,
            CallArguments::I32(arguments),
        )?;
        let mut output = Vec::new();
        output
            .try_reserve(results.len())
            .map_err(|_| WasmError::HostAllocationFailed)?;
        for value in results {
            match value {
                Val::I32(value) => output.push(value),
                _ => {
                    return Err(WasmError::InternalInvariant {
                        detail: "Wasmtime returned a value outside the checked i32 signature",
                    });
                }
            }
        }
        Ok(output)
    }

    /// Calls an exported function whose parameters and results are admitted scalar values.
    ///
    /// Only `i32`, `i64`, `f32`, and `f64` signatures are accepted. Floating-point values cross
    /// this API as exact IEEE-754 bits. Vector, reference, and GC values are rejected before the
    /// function is invoked.
    pub fn call_scalars(
        &mut self,
        store_id: StoreId,
        module_id: ModuleId,
        instance_id: InstanceId,
        export_name: &str,
        arguments: &[WasmScalarValue],
    ) -> Result<Vec<WasmScalarValue>, WasmError> {
        let results = self.call_scalar_primitive(
            store_id,
            module_id,
            instance_id,
            export_name,
            CallArguments::Scalars(arguments),
        )?;
        let mut output = Vec::new();
        output
            .try_reserve(results.len())
            .map_err(|_| WasmError::HostAllocationFailed)?;
        for value in results {
            output.push(scalar_from_wasmtime(value)?);
        }
        Ok(output)
    }

    fn call_scalar_primitive(
        &mut self,
        store_id: StoreId,
        module_id: ModuleId,
        instance_id: InstanceId,
        export_name: &str,
        arguments: CallArguments<'_>,
    ) -> Result<Vec<Val>, WasmError> {
        self.ensure_open()?;
        let _module_key = self.module_key(module_id)?;
        let store_key = self.store_key(store_id)?;
        let instance_key = self.instance_key(instance_id)?;
        let instance_entry = *self
            .instances
            .get(instance_key)
            .ok_or(WasmError::StaleIdentity {
                kind: IdentityKind::Instance,
            })?;
        if instance_entry.store != store_id {
            return Err(WasmError::WrongStoreAssociation);
        }
        if instance_entry.module != module_id {
            return Err(WasmError::WrongModuleAssociation);
        }
        if export_name.len() > self.limits.max_export_name_bytes {
            return Err(WasmError::ExportNameTooLong {
                actual: export_name.len(),
                maximum: self.limits.max_export_name_bytes,
            });
        }

        let fuel = self.limits.fuel_per_operation;
        let maximum_parameters = self.limits.max_call_parameters;
        let maximum_results = self.limits.max_call_results;
        let store_entry = self
            .stores
            .get_mut(store_key)
            .ok_or(WasmError::StaleIdentity {
                kind: IdentityKind::Store,
            })?;
        let export = instance_entry
            .instance
            .get_export(&mut store_entry.store, export_name)
            .ok_or_else(|| WasmError::ExportNotFound {
                name: export_name.to_owned(),
            })?;
        let function = match export {
            Extern::Func(function) => function,
            _ => {
                return Err(WasmError::ExportNotFunction {
                    name: export_name.to_owned(),
                });
            }
        };
        let function_type = function.ty(&store_entry.store);
        let parameter_count = function_type.params().len();
        let result_count = function_type.results().len();
        let parameters_supported = function_type
            .params()
            .all(|ty| arguments.contract().supports(&ty));
        let results_supported = function_type
            .results()
            .all(|ty| arguments.contract().supports(&ty));
        if parameter_count > maximum_parameters
            || result_count > maximum_results
            || !parameters_supported
            || !results_supported
        {
            return Err(arguments
                .contract()
                .unsupported_signature(parameter_count, result_count));
        }
        if arguments.len() != parameter_count {
            return Err(WasmError::WrongArgumentCount {
                expected: parameter_count,
                actual: arguments.len(),
            });
        }

        if let CallArguments::Scalars(values) = arguments {
            for (index, (expected, actual)) in function_type
                .params()
                .zip(values.iter().copied())
                .enumerate()
            {
                let expected =
                    scalar_type_from_wasmtime(&expected).ok_or(WasmError::InternalInvariant {
                        detail: "checked scalar parameter type became unsupported",
                    })?;
                let actual = actual.value_type();
                if actual != expected {
                    return Err(WasmError::ArgumentTypeMismatch {
                        index,
                        expected,
                        actual,
                    });
                }
            }
        }

        let mut parameters = Vec::new();
        parameters
            .try_reserve(parameter_count)
            .map_err(|_| WasmError::HostAllocationFailed)?;
        match arguments {
            CallArguments::I32(values) => {
                parameters.extend(values.iter().copied().map(Val::I32));
            }
            CallArguments::Scalars(values) => {
                parameters.extend(values.iter().copied().map(scalar_to_wasmtime));
            }
        }
        let mut results = Vec::new();
        results
            .try_reserve(result_count)
            .map_err(|_| WasmError::HostAllocationFailed)?;
        for ty in function_type.results() {
            results.push(scalar_placeholder(&ty).ok_or(WasmError::InternalInvariant {
                detail: "checked scalar result type became unsupported",
            })?);
        }

        prepare_execution(store_entry, &self.control, fuel)?;
        if let Err(error) = function.call(&mut store_entry.store, &parameters, &mut results) {
            let mapped = map_execution_error(error, ExecutionPhase::Call);
            record_interrupt_if_needed(store_entry, &self.control, &mapped);
            return Err(mapped);
        }

        Ok(results)
    }

    /// Invalidates one instance identity after checking its exact store and module association.
    ///
    /// Wasmtime owns instance allocations at store granularity, so resident resource accounting
    /// is released only by [`Self::drop_store`].
    pub fn drop_instance(
        &mut self,
        store_id: StoreId,
        module_id: ModuleId,
        instance_id: InstanceId,
    ) -> Result<(), WasmError> {
        self.ensure_open()?;
        let _module_key = self.module_key(module_id)?;
        let _store_key = self.store_key(store_id)?;
        let instance_key = self.instance_key(instance_id)?;
        let instance = self
            .instances
            .get(instance_key)
            .ok_or(WasmError::StaleIdentity {
                kind: IdentityKind::Instance,
            })?;
        if instance.store != store_id {
            return Err(WasmError::WrongStoreAssociation);
        }
        if instance.module != module_id {
            return Err(WasmError::WrongModuleAssociation);
        }
        self.instances
            .remove(instance_key)
            .ok_or(WasmError::InternalInvariant {
                detail: "validated instance could not be invalidated",
            })?;
        Ok(())
    }

    /// Removes a store and atomically invalidates all of its remaining instance identities.
    pub fn drop_store(&mut self, store_id: StoreId) -> Result<usize, WasmError> {
        self.ensure_open()?;
        let store_key = self.store_key(store_id)?;

        let (module_ids, resident_count) = {
            let store = self.stores.get(store_key).ok_or(WasmError::StaleIdentity {
                kind: IdentityKind::Store,
            })?;
            let mut module_ids = Vec::new();
            module_ids
                .try_reserve(store.resident_modules.len())
                .map_err(|_| WasmError::HostAllocationFailed)?;
            module_ids.extend_from_slice(&store.resident_modules);
            (module_ids, store.resident_modules.len())
        };

        let mut module_charges = Vec::<(Key, usize)>::new();
        module_charges
            .try_reserve(module_ids.len())
            .map_err(|_| WasmError::HostAllocationFailed)?;
        for module_id in module_ids {
            let key = self.module_key(module_id)?;
            if let Some((_, count)) = module_charges
                .iter_mut()
                .find(|(existing, _)| *existing == key)
            {
                *count += 1;
            } else {
                module_charges.push((key, 1));
            }
        }
        for (key, count) in &module_charges {
            let module = self.modules.get(*key).ok_or(WasmError::InternalInvariant {
                detail: "resident instance refers to a missing module",
            })?;
            if module.resident_instances < *count {
                return Err(WasmError::InternalInvariant {
                    detail: "resident module count underflow",
                });
            }
        }
        if resident_count > self.resident_instances {
            return Err(WasmError::InternalInvariant {
                detail: "process resident instance count underflow",
            });
        }

        let mut instance_keys = Vec::new();
        instance_keys
            .try_reserve(self.instances.active())
            .map_err(|_| WasmError::HostAllocationFailed)?;
        for key in self.instances.keys() {
            let instance = self
                .instances
                .get(key)
                .ok_or(WasmError::InternalInvariant {
                    detail: "live instance registry key did not resolve",
                })?;
            if instance.store == store_id {
                instance_keys.push(key);
            }
        }

        for key in &instance_keys {
            self.instances
                .remove(*key)
                .ok_or(WasmError::InternalInvariant {
                    detail: "store descendant disappeared during cascade invalidation",
                })?;
        }
        self.stores
            .remove(store_key)
            .ok_or(WasmError::InternalInvariant {
                detail: "validated store could not be removed",
            })?;
        for (key, count) in module_charges {
            self.modules
                .get_mut(key)
                .ok_or(WasmError::InternalInvariant {
                    detail: "resident module disappeared during store removal",
                })?
                .resident_instances -= count;
        }
        self.resident_instances -= resident_count;
        Ok(instance_keys.len())
    }

    /// Removes a module only when no resident store instance still depends on it.
    pub fn drop_module(&mut self, module_id: ModuleId) -> Result<(), WasmError> {
        self.ensure_open()?;
        let module_key = self.module_key(module_id)?;
        let dependents = self
            .modules
            .get(module_key)
            .ok_or(WasmError::StaleIdentity {
                kind: IdentityKind::Module,
            })?
            .resident_instances;
        if dependents != 0 {
            return Err(WasmError::ResourceInUse {
                kind: IdentityKind::Module,
                dependents,
            });
        }
        self.modules
            .remove(module_key)
            .ok_or(WasmError::InternalInvariant {
                detail: "validated module could not be removed",
            })?;
        Ok(())
    }

    /// Invalidates all identities and tears down stores in descendant-first order.
    pub fn reset(&mut self) -> CleanupReport {
        self.reset_internal()
    }

    /// Returns current identity and resident-resource counts.
    pub fn live_counts(&self) -> LiveCounts {
        LiveCounts {
            modules: self.modules.active(),
            stores: self.stores.active(),
            instances: self.instances.active(),
            resident_instances: self.resident_instances,
        }
    }

    /// Closes this process owner, invalidates interrupt handles, and tears down every resource.
    pub fn shutdown(mut self) -> CleanupReport {
        self.control.alive.store(false, Ordering::Release);
        self.reset_internal()
    }

    fn reset_internal(&mut self) -> CleanupReport {
        let report = CleanupReport {
            modules: self.modules.active(),
            stores: self.stores.active(),
            instances: self.instances.active(),
            resident_instances: self.resident_instances,
        };
        self.instances.invalidate_all();
        self.stores.invalidate_all();
        self.modules.invalidate_all();
        self.resident_instances = 0;
        report
    }

    fn validate_binary_header(&self, bytes: &[u8]) -> Result<(), WasmError> {
        if bytes.len() > self.limits.max_module_bytes {
            return Err(WasmError::ModuleTooLarge {
                actual: bytes.len(),
                maximum: self.limits.max_module_bytes,
            });
        }
        if bytes.len() < 8 {
            return Err(WasmError::TruncatedBinaryHeader {
                actual: bytes.len(),
            });
        }
        if bytes[..4] != [0x00, 0x61, 0x73, 0x6d] {
            return Err(WasmError::InvalidBinaryMagic);
        }
        let found = [bytes[4], bytes[5], bytes[6], bytes[7]];
        if found != [0x01, 0x00, 0x00, 0x00] {
            return Err(WasmError::InvalidBinaryVersion { found });
        }
        Ok(())
    }

    fn ensure_open(&self) -> Result<(), WasmError> {
        if self.control.alive.load(Ordering::Acquire) {
            Ok(())
        } else {
            Err(WasmError::RuntimeClosed)
        }
    }

    fn module_key(&self, id: ModuleId) -> Result<Key, WasmError> {
        self.checked_key(id.owner(), id.key(), IdentityKind::Module, &self.modules)
    }

    fn store_key(&self, id: StoreId) -> Result<Key, WasmError> {
        self.checked_key(id.owner(), id.key(), IdentityKind::Store, &self.stores)
    }

    fn instance_key(&self, id: InstanceId) -> Result<Key, WasmError> {
        self.checked_key(
            id.owner(),
            id.key(),
            IdentityKind::Instance,
            &self.instances,
        )
    }

    fn checked_key<T>(
        &self,
        owner: u64,
        key: Key,
        kind: IdentityKind,
        registry: &Registry<T>,
    ) -> Result<Key, WasmError> {
        if owner != self.owner {
            return Err(WasmError::ForeignIdentity { kind });
        }
        if registry.get(key).is_none() {
            return Err(WasmError::StaleIdentity { kind });
        }
        Ok(key)
    }
}

impl Drop for WasmProcess {
    fn drop(&mut self) {
        self.control.alive.store(false, Ordering::Release);
        self.reset_internal();
    }
}

#[derive(Clone, Copy)]
enum CallArguments<'a> {
    I32(&'a [i32]),
    Scalars(&'a [WasmScalarValue]),
}

impl CallArguments<'_> {
    fn len(self) -> usize {
        match self {
            Self::I32(values) => values.len(),
            Self::Scalars(values) => values.len(),
        }
    }

    fn contract(self) -> ScalarCallContract {
        match self {
            Self::I32(_) => ScalarCallContract::I32Only,
            Self::Scalars(_) => ScalarCallContract::AllScalars,
        }
    }
}

#[derive(Clone, Copy)]
enum ScalarCallContract {
    I32Only,
    AllScalars,
}

impl ScalarCallContract {
    fn supports(self, ty: &ValType) -> bool {
        match self {
            Self::I32Only => matches!(ty, ValType::I32),
            Self::AllScalars => scalar_type_from_wasmtime(ty).is_some(),
        }
    }

    fn unsupported_signature(self, parameters: usize, results: usize) -> WasmError {
        match self {
            Self::I32Only => WasmError::UnsupportedSignature {
                parameters,
                results,
            },
            Self::AllScalars => WasmError::UnsupportedScalarSignature {
                parameters,
                results,
            },
        }
    }
}

fn scalar_type_from_wasmtime(ty: &ValType) -> Option<WasmScalarType> {
    match ty {
        ValType::I32 => Some(WasmScalarType::I32),
        ValType::I64 => Some(WasmScalarType::I64),
        ValType::F32 => Some(WasmScalarType::F32),
        ValType::F64 => Some(WasmScalarType::F64),
        ValType::V128 | ValType::Ref(_) => None,
    }
}

fn scalar_to_wasmtime(value: WasmScalarValue) -> Val {
    match value {
        WasmScalarValue::I32(value) => Val::I32(value),
        WasmScalarValue::I64(value) => Val::I64(value),
        WasmScalarValue::F32Bits(bits) => Val::F32(bits),
        WasmScalarValue::F64Bits(bits) => Val::F64(bits),
    }
}

fn scalar_placeholder(ty: &ValType) -> Option<Val> {
    match ty {
        ValType::I32 => Some(Val::I32(0)),
        ValType::I64 => Some(Val::I64(0)),
        ValType::F32 => Some(Val::F32(0)),
        ValType::F64 => Some(Val::F64(0)),
        ValType::V128 | ValType::Ref(_) => None,
    }
}

fn scalar_from_wasmtime(value: Val) -> Result<WasmScalarValue, WasmError> {
    match value {
        Val::I32(value) => Ok(WasmScalarValue::I32(value)),
        Val::I64(value) => Ok(WasmScalarValue::I64(value)),
        Val::F32(bits) => Ok(WasmScalarValue::F32Bits(bits)),
        Val::F64(bits) => Ok(WasmScalarValue::F64Bits(bits)),
        Val::V128(_)
        | Val::FuncRef(_)
        | Val::ExternRef(_)
        | Val::AnyRef(_)
        | Val::ExnRef(_)
        | Val::ContRef(_) => Err(WasmError::InternalInvariant {
            detail: "Wasmtime returned a value outside the checked scalar signature",
        }),
    }
}

#[derive(Clone, Copy)]
enum ExecutionPhase {
    Instantiation,
    Call,
}

fn prepare_execution(
    store: &mut StoreEntry,
    control: &InterruptControl,
    fuel: u64,
) -> Result<(), WasmError> {
    store
        .store
        .set_fuel(fuel)
        .map_err(|_| WasmError::InternalInvariant {
            detail: "fuel was not enabled on the process engine",
        })?;
    store.store.epoch_deadline_trap();

    // Install the deadline first. A request before the subsequent sequence load is observed by
    // the sequence mismatch; a request after it advances the engine epoch past this deadline.
    store.store.set_epoch_deadline(1);
    let sequence = control.sequence.load(Ordering::Acquire);
    if control.poisoned.load(Ordering::Acquire) {
        store.store.set_epoch_deadline(0);
        return Err(WasmError::InterruptSequenceExhausted);
    }
    if sequence != store.last_interrupt_sequence {
        store.store.set_epoch_deadline(0);
    }
    Ok(())
}

fn record_interrupt_if_needed(
    store: &mut StoreEntry,
    control: &InterruptControl,
    error: &WasmError,
) {
    if matches!(error, WasmError::Interrupted) {
        store.last_interrupt_sequence = control.sequence.load(Ordering::Acquire);
    }
}

fn map_execution_error(error: wasmtime::Error, phase: ExecutionPhase) -> WasmError {
    if error
        .downcast_ref::<Trap>()
        .is_some_and(|trap| *trap == Trap::OutOfFuel)
    {
        return WasmError::FuelExhausted;
    }
    if error
        .downcast_ref::<Trap>()
        .is_some_and(|trap| *trap == Trap::Interrupt)
    {
        return WasmError::Interrupted;
    }

    match phase {
        ExecutionPhase::Instantiation => WasmError::InstantiationFailed {
            detail: error.to_string(),
        },
        ExecutionPhase::Call => WasmError::ExecutionTrap {
            detail: error.to_string(),
        },
    }
}

fn allocate_owner() -> Result<u64, WasmError> {
    NEXT_OWNER
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            current.checked_add(1)
        })
        .map_err(|_| WasmError::IdentitySpaceExhausted)
}

fn build_engine(limits: &WasmLimits) -> Result<Engine, WasmError> {
    let proposals = INITIAL_PROPOSAL_POLICY;
    let mut config = Config::new();
    config
        .target("x86_64-unknown-linux-gnu")
        .map_err(|error| WasmError::EngineCreation {
            detail: error.to_string(),
        })?;
    config.strategy(Strategy::Cranelift);
    config.collector(Collector::DeferredReferenceCounting);
    config.allocation_strategy(InstanceAllocationStrategy::OnDemand);
    config.consume_fuel(true);
    config.epoch_interruption(true);
    config.max_wasm_stack(limits.max_wasm_stack_bytes);
    config.debug_info(false);
    config.debug_symbols(false);
    config.wasm_backtrace_details(WasmBacktraceDetails::Disable);
    config.wasm_backtrace_max_frames(None);
    config.native_unwind_info(false);
    config.cranelift_nan_canonicalization(true);
    config.memory_reservation(limits.max_memory_bytes as u64);
    config.memory_reservation_for_growth(0);
    config.memory_may_move(false);

    config.gc_support(true);
    config.wasm_tail_call(proposals.tail_calls);
    config.wasm_branch_hinting(proposals.branch_hints);
    config.wasm_custom_page_sizes(proposals.custom_page_sizes);
    config.wasm_threads(proposals.threads);
    config.wasm_shared_everything_threads(proposals.shared_everything_threads);
    config.wasm_reference_types(proposals.reference_types);
    config.wasm_function_references(proposals.function_references);
    config.wasm_wide_arithmetic(proposals.wide_arithmetic);
    config.wasm_gc(proposals.wasm_gc);
    config.wasm_simd(proposals.simd);
    config.wasm_relaxed_simd(proposals.relaxed_simd);
    config.relaxed_simd_deterministic(proposals.deterministic_relaxed_simd);
    config.wasm_bulk_memory(proposals.bulk_memory);
    config.wasm_multi_value(proposals.multi_value);
    config.wasm_multi_memory(proposals.multi_memory);
    config.wasm_memory64(proposals.memory64);
    config.wasm_extended_const(proposals.extended_const);
    config.wasm_stack_switching(proposals.stack_switching);
    config.wasm_exceptions(proposals.exception_handling);
    set_legacy_exceptions(&mut config, proposals.legacy_exceptions);

    Engine::new(&config).map_err(|error| WasmError::EngineCreation {
        detail: error.to_string(),
    })
}

#[allow(
    deprecated,
    reason = "the first runtime gate explicitly disables every proposal toggle exposed by v47"
)]
fn set_legacy_exceptions(config: &mut Config, enabled: bool) {
    config.wasm_legacy_exceptions(enabled);
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use super::WasmProcess;
    use crate::{WasmError, WasmLimits};

    const EMPTY: &[u8] = &[0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00];

    #[test]
    fn interrupt_sequence_overflow_poisons_future_execution_without_wrap() {
        let mut process = WasmProcess::new(WasmLimits::default()).unwrap();
        let module = process.compile_module(EMPTY).unwrap();
        let store = process.create_store().unwrap();
        process.control.sequence.store(u64::MAX, Ordering::Release);
        let interrupt = process.interrupt_handle();

        assert_eq!(
            interrupt.interrupt().unwrap_err(),
            WasmError::InterruptSequenceExhausted
        );
        assert!(process.control.poisoned.load(Ordering::Acquire));
        assert_eq!(process.control.sequence.load(Ordering::Acquire), u64::MAX);
        assert_eq!(
            process.instantiate(store, module).unwrap_err(),
            WasmError::InterruptSequenceExhausted
        );
        assert_eq!(process.live_counts().resident_instances, 0);
        assert_eq!(
            interrupt.interrupt().unwrap_err(),
            WasmError::InterruptSequenceExhausted
        );
    }
}
