use crate::WasmError;

/// Size in bytes of a standard core WebAssembly page.
pub const WASM_PAGE_BYTES: usize = 65_536;

/// Hard process/store/execution limits for the first adapter gate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WasmLimits {
    pub max_module_bytes: usize,
    pub max_modules: usize,
    pub max_stores: usize,
    pub max_instances: usize,
    pub max_instances_per_store: usize,
    pub max_memory_bytes: usize,
    pub max_memories_per_store: usize,
    pub max_table_elements: usize,
    pub max_tables_per_store: usize,
    pub fuel_per_operation: u64,
    pub max_wasm_stack_bytes: usize,
    pub max_call_parameters: usize,
    pub max_call_results: usize,
    pub max_export_name_bytes: usize,
}

impl Default for WasmLimits {
    fn default() -> Self {
        Self {
            max_module_bytes: 1 << 20,
            max_modules: 64,
            max_stores: 16,
            max_instances: 128,
            max_instances_per_store: 16,
            max_memory_bytes: 16 << 20,
            max_memories_per_store: 4,
            max_table_elements: 16_384,
            max_tables_per_store: 4,
            fuel_per_operation: 1_000_000,
            max_wasm_stack_bytes: 512 << 10,
            max_call_parameters: 16,
            max_call_results: 16,
            max_export_name_bytes: 1_024,
        }
    }
}

impl WasmLimits {
    pub(crate) fn validate(&self) -> Result<(), WasmError> {
        require_at_least("max_module_bytes", self.max_module_bytes, 8)?;
        require_nonzero("max_modules", self.max_modules)?;
        require_nonzero("max_stores", self.max_stores)?;
        require_nonzero("max_instances", self.max_instances)?;
        require_nonzero("max_instances_per_store", self.max_instances_per_store)?;
        require_at_least("max_memory_bytes", self.max_memory_bytes, WASM_PAGE_BYTES)?;
        require_nonzero("max_memories_per_store", self.max_memories_per_store)?;
        require_nonzero("max_table_elements", self.max_table_elements)?;
        require_nonzero("max_tables_per_store", self.max_tables_per_store)?;
        require_at_least("max_wasm_stack_bytes", self.max_wasm_stack_bytes, 64 << 10)?;
        require_nonzero("max_call_parameters", self.max_call_parameters)?;
        require_nonzero("max_call_results", self.max_call_results)?;
        require_nonzero("max_export_name_bytes", self.max_export_name_bytes)?;

        if self.fuel_per_operation == 0 {
            return Err(WasmError::InvalidLimit {
                name: "fuel_per_operation",
                reason: "must be nonzero",
            });
        }
        if self.max_instances_per_store > self.max_instances {
            return Err(WasmError::InvalidLimit {
                name: "max_instances_per_store",
                reason: "must not exceed max_instances",
            });
        }
        for (name, value) in [
            ("max_modules", self.max_modules),
            ("max_stores", self.max_stores),
            ("max_instances", self.max_instances),
        ] {
            if u32::try_from(value).is_err() {
                return Err(WasmError::InvalidLimit {
                    name,
                    reason: "must fit in the opaque 32-bit slot space",
                });
            }
        }
        Ok(())
    }
}

fn require_nonzero(name: &'static str, value: usize) -> Result<(), WasmError> {
    require_at_least(name, value, 1)
}

fn require_at_least(name: &'static str, value: usize, minimum: usize) -> Result<(), WasmError> {
    if value < minimum {
        Err(WasmError::InvalidLimit {
            name,
            reason: "is below the minimum admitted value",
        })
    } else {
        Ok(())
    }
}
