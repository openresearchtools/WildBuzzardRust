/// Explicit core-proposal decisions for the first adapter gate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProposalPolicy {
    pub core_modules: bool,
    pub tail_calls: bool,
    pub branch_hints: bool,
    pub custom_page_sizes: bool,
    pub threads: bool,
    pub shared_everything_threads: bool,
    pub reference_types: bool,
    pub function_references: bool,
    pub wide_arithmetic: bool,
    pub wasm_gc: bool,
    pub simd: bool,
    pub relaxed_simd: bool,
    pub deterministic_relaxed_simd: bool,
    pub bulk_memory: bool,
    pub multi_value: bool,
    pub multi_memory: bool,
    pub memory64: bool,
    pub extended_const: bool,
    pub stack_switching: bool,
    pub exception_handling: bool,
    pub legacy_exceptions: bool,
    pub component_model: bool,
}

/// The exact initial runtime proposal policy.
pub const INITIAL_PROPOSAL_POLICY: ProposalPolicy = ProposalPolicy {
    core_modules: true,
    tail_calls: true,
    branch_hints: false,
    custom_page_sizes: false,
    threads: false,
    shared_everything_threads: false,
    reference_types: true,
    function_references: true,
    wide_arithmetic: false,
    wasm_gc: false,
    simd: true,
    relaxed_simd: true,
    deterministic_relaxed_simd: true,
    bulk_memory: true,
    multi_value: true,
    multi_memory: true,
    memory64: false,
    extended_const: true,
    stack_switching: false,
    exception_handling: true,
    legacy_exceptions: false,
    component_model: false,
};

/// Explicit ambient-capability decisions for this no-host gate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CapabilityPolicy {
    pub imports: bool,
    pub host_functions: bool,
    pub wasi: bool,
    pub filesystem: bool,
    pub sockets: bool,
    pub http: bool,
    pub environment: bool,
    pub clocks: bool,
    pub randomness: bool,
    pub wat: bool,
    pub native_deserialization: bool,
    pub compiled_code_cache: bool,
    pub async_fibers: bool,
}

/// Every ambient capability is denied in the first adapter gate.
pub const INITIAL_CAPABILITY_POLICY: CapabilityPolicy = CapabilityPolicy {
    imports: false,
    host_functions: false,
    wasi: false,
    filesystem: false,
    sockets: false,
    http: false,
    environment: false,
    clocks: false,
    randomness: false,
    wat: false,
    native_deserialization: false,
    compiled_code_cache: false,
    async_fibers: false,
};
