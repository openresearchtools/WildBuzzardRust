use wild_buzzard_wasm::{
    INITIAL_CAPABILITY_POLICY, INITIAL_PROPOSAL_POLICY, WasmError, WasmLimits, WasmProcess,
};

const SHARED_MEMORY: &[u8] = &[
    0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, 0x05, 0x04, 0x01, 0x03, 0x01, 0x01,
];
const MEMORY64: &[u8] = &[
    0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, 0x05, 0x03, 0x01, 0x04, 0x01,
];
const GC_STRUCT_TYPE: &[u8] = &[
    0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, 0x01, 0x03, 0x01, 0x5f, 0x00,
];
const CUSTOM_PAGE_SIZE: &[u8] = &[
    0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, 0x05, 0x04, 0x01, 0x08, 0x01, 0x00,
];
const WIDE_ARITHMETIC: &[u8] = &[
    0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, 0x01, 0x0a, 0x01, 0x60, 0x04, 0x7e, 0x7e, 0x7e,
    0x7e, 0x02, 0x7e, 0x7e, 0x03, 0x02, 0x01, 0x00, 0x0a, 0x0e, 0x01, 0x0c, 0x00, 0x20, 0x00, 0x20,
    0x01, 0x20, 0x02, 0x20, 0x03, 0xfc, 0x13, 0x0b,
];
const MULTI_MEMORY: &[u8] = &[
    0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, 0x05, 0x05, 0x02, 0x00, 0x01, 0x00, 0x01,
];
const SIMD_I32_RESULT: &[u8] = &[
    0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, 0x01, 0x05, 0x01, 0x60, 0x00, 0x01, 0x7f, 0x03,
    0x02, 0x01, 0x00, 0x07, 0x06, 0x01, 0x02, 0x6f, 0x6b, 0x00, 0x00, 0x0a, 0x19, 0x01, 0x17, 0x00,
    0xfd, 0x0c, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0xfd, 0x1b, 0x00, 0x0b,
];

#[test]
fn manifest_selects_only_the_admitted_local_wasmtime_features() {
    let manifest = include_str!("../Cargo.toml");
    let exact_dependency = "wasmtime = { version = \"=47.0.3\", path = \"../wasmtime/crates/wasmtime\", default-features = false, features = [\"std\", \"runtime\", \"cranelift\", \"gc\", \"gc-drc\", \"threads\"] }";
    assert!(manifest.contains("[workspace]"));
    assert!(manifest.contains(exact_dependency));
    assert!(!manifest.contains("[dev-dependencies]"));
}

#[test]
fn lockfile_excludes_unadmitted_products_and_network_sources() {
    let lockfile = include_str!("../Cargo.lock");
    for package in [
        "name = \"wat\"",
        "name = \"wasmtime-cache\"",
        "name = \"wasmtime-cli\"",
        "name = \"wasmtime-fiber\"",
        "name = \"wasmtime-wasi\"",
        "name = \"wasmtime-wasi-http\"",
        "name = \"wasmtime-winch\"",
    ] {
        assert!(!lockfile.contains(package), "unexpected package: {package}");
    }
    assert!(!lockfile.contains("source = \"git+"));
}

#[test]
fn runtime_configures_every_exposed_v47_proposal_toggle() {
    let runtime = include_str!("../src/runtime.rs");
    for call in [
        "wasm_tail_call(proposals.tail_calls)",
        "wasm_branch_hinting(proposals.branch_hints)",
        "wasm_custom_page_sizes(proposals.custom_page_sizes)",
        "wasm_threads(proposals.threads)",
        "wasm_shared_everything_threads(proposals.shared_everything_threads)",
        "wasm_reference_types(proposals.reference_types)",
        "wasm_function_references(proposals.function_references)",
        "wasm_wide_arithmetic(proposals.wide_arithmetic)",
        "wasm_gc(proposals.wasm_gc)",
        "wasm_simd(proposals.simd)",
        "wasm_relaxed_simd(proposals.relaxed_simd)",
        "wasm_bulk_memory(proposals.bulk_memory)",
        "wasm_multi_value(proposals.multi_value)",
        "wasm_multi_memory(proposals.multi_memory)",
        "wasm_memory64(proposals.memory64)",
        "wasm_extended_const(proposals.extended_const)",
        "wasm_stack_switching(proposals.stack_switching)",
        "wasm_exceptions(proposals.exception_handling)",
        "wasm_legacy_exceptions(enabled)",
    ] {
        assert!(
            runtime.contains(call),
            "missing explicit config call: {call}"
        );
    }
}

#[test]
fn public_policy_denies_ambient_capabilities_and_unadmitted_proposals() {
    let capabilities = INITIAL_CAPABILITY_POLICY;
    assert!(!capabilities.imports);
    assert!(!capabilities.host_functions);
    assert!(!capabilities.wasi);
    assert!(!capabilities.filesystem);
    assert!(!capabilities.sockets);
    assert!(!capabilities.http);
    assert!(!capabilities.environment);
    assert!(!capabilities.clocks);
    assert!(!capabilities.randomness);
    assert!(!capabilities.wat);
    assert!(!capabilities.native_deserialization);
    assert!(!capabilities.compiled_code_cache);
    assert!(!capabilities.async_fibers);

    let proposals = INITIAL_PROPOSAL_POLICY;
    assert!(proposals.core_modules);
    assert!(proposals.reference_types);
    assert!(proposals.function_references);
    assert!(proposals.simd);
    assert!(proposals.relaxed_simd);
    assert!(proposals.deterministic_relaxed_simd);
    assert!(!proposals.wasm_gc);
    assert!(!proposals.threads);
    assert!(!proposals.shared_everything_threads);
    assert!(!proposals.memory64);
    assert!(!proposals.stack_switching);
    assert!(!proposals.custom_page_sizes);
    assert!(!proposals.branch_hints);
    assert!(!proposals.wide_arithmetic);
    assert!(!proposals.component_model);
}

#[test]
fn disabled_proposals_are_rejected_by_binary_validation() {
    let process = WasmProcess::new(WasmLimits::default()).unwrap();
    for binary in [
        SHARED_MEMORY,
        MEMORY64,
        GC_STRUCT_TYPE,
        CUSTOM_PAGE_SIZE,
        WIDE_ARITHMETIC,
    ] {
        assert!(matches!(
            process.validate_module(binary),
            Err(WasmError::ValidationFailed { .. })
        ));
    }
}

#[test]
fn admitted_reference_simd_and_core_proposals_validate() {
    let process = WasmProcess::new(WasmLimits::default()).unwrap();
    process.validate_module(MULTI_MEMORY).unwrap();
    process.validate_module(SIMD_I32_RESULT).unwrap();
}

#[test]
fn first_party_runtime_contains_no_unsafe_or_raw_capability_entrypoints() {
    let sources = [
        include_str!("../src/error.rs"),
        include_str!("../src/identity.rs"),
        include_str!("../src/limits.rs"),
        include_str!("../src/policy.rs"),
        include_str!("../src/registry.rs"),
        include_str!("../src/runtime.rs"),
    ];
    for source in sources {
        assert!(!source.contains("unsafe {"));
        assert!(!source.contains("unsafe fn"));
    }
    let runtime = include_str!("../src/runtime.rs");
    for forbidden in [
        "Linker",
        "deserialize(",
        "deserialize_file",
        "from_file(",
        "Func::new",
        "Memory::new",
    ] {
        assert!(
            !runtime.contains(forbidden),
            "unexpected capability/raw entrypoint: {forbidden}"
        );
    }
}
