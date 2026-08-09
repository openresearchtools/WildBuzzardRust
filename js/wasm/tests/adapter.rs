use std::thread;

use wild_buzzard_wasm::{
    IdentityKind, LiveCounts, WASM_PAGE_BYTES, WasmError, WasmLimits, WasmProcess,
};

const EMPTY: &[u8] = &[0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00];
const ADD: &[u8] = &[
    0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, 0x01, 0x07, 0x01, 0x60, 0x02, 0x7f, 0x7f, 0x01,
    0x7f, 0x03, 0x02, 0x01, 0x00, 0x07, 0x07, 0x01, 0x03, 0x61, 0x64, 0x64, 0x00, 0x00, 0x0a, 0x09,
    0x01, 0x07, 0x00, 0x20, 0x00, 0x20, 0x01, 0x6a, 0x0b,
];
const SPIN: &[u8] = &[
    0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, 0x01, 0x04, 0x01, 0x60, 0x00, 0x00, 0x03, 0x02,
    0x01, 0x00, 0x07, 0x08, 0x01, 0x04, 0x73, 0x70, 0x69, 0x6e, 0x00, 0x00, 0x0a, 0x09, 0x01, 0x07,
    0x00, 0x03, 0x40, 0x0c, 0x00, 0x0b, 0x0b,
];
const START_SPIN: &[u8] = &[
    0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, 0x01, 0x04, 0x01, 0x60, 0x00, 0x00, 0x03, 0x02,
    0x01, 0x00, 0x08, 0x01, 0x00, 0x0a, 0x09, 0x01, 0x07, 0x00, 0x03, 0x40, 0x0c, 0x00, 0x0b, 0x0b,
];
const IMPORT: &[u8] = &[
    0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, 0x01, 0x04, 0x01, 0x60, 0x00, 0x00, 0x02, 0x09,
    0x01, 0x03, 0x65, 0x6e, 0x76, 0x01, 0x78, 0x00, 0x00,
];
const MEMORY_INITIAL_TWO: &[u8] = &[
    0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, 0x05, 0x03, 0x01, 0x00, 0x02,
];
const MEMORY_GROW: &[u8] = &[
    0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, 0x01, 0x06, 0x01, 0x60, 0x01, 0x7f, 0x01, 0x7f,
    0x03, 0x02, 0x01, 0x00, 0x05, 0x04, 0x01, 0x01, 0x01, 0x02, 0x07, 0x08, 0x01, 0x04, 0x67, 0x72,
    0x6f, 0x77, 0x00, 0x00, 0x0a, 0x08, 0x01, 0x06, 0x00, 0x20, 0x00, 0x40, 0x00, 0x0b,
];
const TABLE_INITIAL_TWO: &[u8] = &[
    0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, 0x04, 0x04, 0x01, 0x70, 0x00, 0x02,
];
const TABLE_GROW: &[u8] = &[
    0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, 0x01, 0x06, 0x01, 0x60, 0x01, 0x7f, 0x01, 0x7f,
    0x03, 0x02, 0x01, 0x00, 0x04, 0x05, 0x01, 0x70, 0x01, 0x01, 0x02, 0x07, 0x08, 0x01, 0x04, 0x67,
    0x72, 0x6f, 0x77, 0x00, 0x00, 0x0a, 0x0b, 0x01, 0x09, 0x00, 0xd0, 0x70, 0x20, 0x00, 0xfc, 0x0f,
    0x00, 0x0b,
];
const WRONG_SIGNATURE: &[u8] = &[
    0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, 0x01, 0x06, 0x01, 0x60, 0x01, 0x7e, 0x01, 0x7e,
    0x03, 0x02, 0x01, 0x00, 0x07, 0x05, 0x01, 0x01, 0x66, 0x00, 0x00, 0x0a, 0x06, 0x01, 0x04, 0x00,
    0x20, 0x00, 0x0b,
];
const MEMORY_EXPORT: &[u8] = &[
    0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, 0x05, 0x03, 0x01, 0x00, 0x01, 0x07, 0x0a, 0x01,
    0x06, 0x6d, 0x65, 0x6d, 0x6f, 0x72, 0x79, 0x02, 0x00,
];

fn process() -> WasmProcess {
    WasmProcess::new(WasmLimits::default()).unwrap()
}

#[test]
fn validates_compiles_instantiates_and_calls_pure_integer_binary() {
    let mut process = process();
    process.validate_module(ADD).unwrap();
    let module = process.compile_module(ADD).unwrap();
    let store = process.create_store().unwrap();
    let instance = process.instantiate(store, module).unwrap();

    assert_eq!(
        process
            .call_i32(store, module, instance, "add", &[20, 22])
            .unwrap(),
        [42]
    );
    assert_eq!(
        process.live_counts(),
        LiveCounts {
            modules: 1,
            stores: 1,
            instances: 1,
            resident_instances: 1,
        }
    );
}

#[test]
fn rejects_text_malformed_invalid_version_and_oversized_inputs() {
    let limits = WasmLimits {
        max_module_bytes: 16,
        ..WasmLimits::default()
    };
    let process = WasmProcess::new(limits).unwrap();

    assert!(matches!(
        process.validate_module(b"(module)"),
        Err(WasmError::InvalidBinaryMagic)
    ));
    assert!(matches!(
        process.validate_module(&EMPTY[..7]),
        Err(WasmError::TruncatedBinaryHeader { actual: 7 })
    ));
    let mut wrong_version = EMPTY.to_vec();
    wrong_version[4] = 2;
    assert!(matches!(
        process.validate_module(&wrong_version),
        Err(WasmError::InvalidBinaryVersion { .. })
    ));
    let malformed = [0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, 0x01, 0x01];
    assert!(matches!(
        process.validate_module(&malformed),
        Err(WasmError::ValidationFailed { .. })
    ));
    assert!(matches!(
        process.validate_module(&[0; 17]),
        Err(WasmError::ModuleTooLarge {
            actual: 17,
            maximum: 16
        })
    ));
}

#[test]
fn rejects_imports_before_module_admission() {
    let mut process = process();
    let error = process.compile_module(IMPORT).unwrap_err();
    assert!(matches!(
        error,
        WasmError::ImportsForbidden {
            count: 1,
            ref first_module,
            ref first_name,
        } if first_module == "env" && first_name == "x"
    ));
    assert_eq!(process.live_counts().modules, 0);
}

#[test]
fn enforces_initial_and_growing_linear_memory_policy() {
    let limits = WasmLimits {
        max_memory_bytes: WASM_PAGE_BYTES,
        ..WasmLimits::default()
    };
    let mut process = WasmProcess::new(limits).unwrap();
    let store = process.create_store().unwrap();

    let too_large = process.compile_module(MEMORY_INITIAL_TWO).unwrap();
    assert!(matches!(
        process.instantiate(store, too_large),
        Err(WasmError::InstantiationFailed { .. })
    ));

    let grow = process.compile_module(MEMORY_GROW).unwrap();
    let instance = process.instantiate(store, grow).unwrap();
    assert_eq!(
        process
            .call_i32(store, grow, instance, "grow", &[1])
            .unwrap(),
        [-1]
    );
}

#[test]
fn enforces_initial_and_growing_table_policy() {
    let limits = WasmLimits {
        max_table_elements: 1,
        ..WasmLimits::default()
    };
    let mut process = WasmProcess::new(limits).unwrap();
    let store = process.create_store().unwrap();

    let too_large = process.compile_module(TABLE_INITIAL_TWO).unwrap();
    assert!(matches!(
        process.instantiate(store, too_large),
        Err(WasmError::InstantiationFailed { .. })
    ));

    let grow = process.compile_module(TABLE_GROW).unwrap();
    let instance = process.instantiate(store, grow).unwrap();
    assert_eq!(
        process
            .call_i32(store, grow, instance, "grow", &[1])
            .unwrap(),
        [-1]
    );
}

#[test]
fn fuel_bounds_calls_and_start_functions() {
    let limits = WasmLimits {
        fuel_per_operation: 100,
        ..WasmLimits::default()
    };
    let mut process = WasmProcess::new(limits).unwrap();
    let store = process.create_store().unwrap();
    let spin = process.compile_module(SPIN).unwrap();
    let instance = process.instantiate(store, spin).unwrap();
    assert_eq!(
        process
            .call_i32(store, spin, instance, "spin", &[])
            .unwrap_err(),
        WasmError::FuelExhausted
    );

    let start_spin = process.compile_module(START_SPIN).unwrap();
    assert_eq!(
        process.instantiate(store, start_spin).unwrap_err(),
        WasmError::FuelExhausted
    );
}

#[test]
fn epoch_handle_interrupts_without_async_or_fibers() {
    let limits = WasmLimits {
        fuel_per_operation: 1_000_000_000,
        ..WasmLimits::default()
    };
    let mut process = WasmProcess::new(limits).unwrap();
    let store = process.create_store().unwrap();
    let module = process.compile_module(SPIN).unwrap();
    let instance = process.instantiate(store, module).unwrap();
    let interrupt = process.interrupt_handle();

    let requester = thread::spawn(move || interrupt.interrupt());
    let error = process
        .call_i32(store, module, instance, "spin", &[])
        .unwrap_err();
    requester.join().unwrap().unwrap();
    assert_eq!(error, WasmError::Interrupted);
}

#[test]
fn pending_interrupt_traps_once_and_the_store_can_run_again() {
    let mut process = process();
    let store = process.create_store().unwrap();
    let spin_module = process.compile_module(SPIN).unwrap();
    let spin_instance = process.instantiate(store, spin_module).unwrap();
    let add_module = process.compile_module(ADD).unwrap();
    let add_instance = process.instantiate(store, add_module).unwrap();

    process.interrupt_handle().interrupt().unwrap();
    assert_eq!(
        process
            .call_i32(store, spin_module, spin_instance, "spin", &[])
            .unwrap_err(),
        WasmError::Interrupted
    );
    assert_eq!(
        process
            .call_i32(store, add_module, add_instance, "add", &[20, 22])
            .unwrap(),
        [42]
    );
}

#[test]
fn rejects_foreign_stale_and_wrongly_associated_ids() {
    let mut first = process();
    let first_store = first.create_store().unwrap();
    let first_module = first.compile_module(ADD).unwrap();
    let other_module = first.compile_module(ADD).unwrap();
    let other_store = first.create_store().unwrap();
    let first_instance = first.instantiate(first_store, first_module).unwrap();

    assert_eq!(
        first
            .call_i32(other_store, first_module, first_instance, "add", &[1, 2])
            .unwrap_err(),
        WasmError::WrongStoreAssociation
    );
    assert_eq!(
        first
            .call_i32(first_store, other_module, first_instance, "add", &[1, 2])
            .unwrap_err(),
        WasmError::WrongModuleAssociation
    );

    let mut second = process();
    let second_store = second.create_store().unwrap();
    let second_module = second.compile_module(ADD).unwrap();
    let second_instance = second.instantiate(second_store, second_module).unwrap();
    assert_eq!(
        first.instantiate(first_store, second_module).unwrap_err(),
        WasmError::ForeignIdentity {
            kind: IdentityKind::Module
        }
    );
    assert_eq!(
        first.instantiate(second_store, first_module).unwrap_err(),
        WasmError::ForeignIdentity {
            kind: IdentityKind::Store
        }
    );
    assert_eq!(
        first
            .call_i32(first_store, first_module, second_instance, "add", &[1, 2])
            .unwrap_err(),
        WasmError::ForeignIdentity {
            kind: IdentityKind::Instance
        }
    );

    first
        .drop_instance(first_store, first_module, first_instance)
        .unwrap();
    assert_eq!(
        first
            .call_i32(first_store, first_module, first_instance, "add", &[1, 2])
            .unwrap_err(),
        WasmError::StaleIdentity {
            kind: IdentityKind::Instance
        }
    );
}

#[test]
fn drop_graph_and_reset_are_deterministic() {
    let mut process = process();
    let module = process.compile_module(ADD).unwrap();
    let store = process.create_store().unwrap();
    let instance = process.instantiate(store, module).unwrap();

    process.drop_instance(store, module, instance).unwrap();
    assert_eq!(
        process.drop_module(module).unwrap_err(),
        WasmError::ResourceInUse {
            kind: IdentityKind::Module,
            dependents: 1,
        }
    );
    assert_eq!(process.drop_store(store).unwrap(), 0);
    assert_eq!(
        process.drop_store(store).unwrap_err(),
        WasmError::StaleIdentity {
            kind: IdentityKind::Store
        }
    );
    process.drop_module(module).unwrap();
    assert_eq!(
        process.drop_module(module).unwrap_err(),
        WasmError::StaleIdentity {
            kind: IdentityKind::Module
        }
    );
    assert_eq!(
        process.live_counts(),
        LiveCounts {
            modules: 0,
            stores: 0,
            instances: 0,
            resident_instances: 0,
        }
    );

    let old_module = process.compile_module(ADD).unwrap();
    let old_store = process.create_store().unwrap();
    let _old_instance = process.instantiate(old_store, old_module).unwrap();
    let report = process.reset();
    assert_eq!(report.modules, 1);
    assert_eq!(report.stores, 1);
    assert_eq!(report.instances, 1);
    assert_eq!(report.resident_instances, 1);

    let new_store = process.create_store().unwrap();
    assert_eq!(
        process.instantiate(new_store, old_module).unwrap_err(),
        WasmError::StaleIdentity {
            kind: IdentityKind::Module
        }
    );
}

#[test]
fn dropping_store_cascades_live_instance_and_releases_module_dependency() {
    let mut process = process();
    let module = process.compile_module(ADD).unwrap();
    let doomed_store = process.create_store().unwrap();
    let surviving_store = process.create_store().unwrap();
    let instance = process.instantiate(doomed_store, module).unwrap();

    assert_eq!(process.drop_store(doomed_store).unwrap(), 1);
    assert_eq!(
        process
            .call_i32(surviving_store, module, instance, "add", &[1, 2])
            .unwrap_err(),
        WasmError::StaleIdentity {
            kind: IdentityKind::Instance
        }
    );
    process.drop_module(module).unwrap();
}

#[test]
fn reset_invalidates_each_descendant_identity_kind() {
    let mut process = process();
    let old_module = process.compile_module(ADD).unwrap();
    let old_store = process.create_store().unwrap();
    let old_instance = process.instantiate(old_store, old_module).unwrap();
    process.reset();

    let new_module = process.compile_module(ADD).unwrap();
    let new_store = process.create_store().unwrap();
    let new_instance = process.instantiate(new_store, new_module).unwrap();
    assert_eq!(
        process
            .drop_instance(new_store, new_module, old_instance)
            .unwrap_err(),
        WasmError::StaleIdentity {
            kind: IdentityKind::Instance
        }
    );
    assert_eq!(
        process.instantiate(old_store, new_module).unwrap_err(),
        WasmError::StaleIdentity {
            kind: IdentityKind::Store
        }
    );
    assert_eq!(
        process.instantiate(new_store, old_module).unwrap_err(),
        WasmError::StaleIdentity {
            kind: IdentityKind::Module
        }
    );
    assert_eq!(
        process
            .call_i32(new_store, new_module, new_instance, "add", &[19, 23])
            .unwrap(),
        [42]
    );
}

#[test]
fn drop_instance_association_failures_do_not_invalidate_the_instance() {
    let mut process = process();
    let module = process.compile_module(ADD).unwrap();
    let wrong_module = process.compile_module(ADD).unwrap();
    let store = process.create_store().unwrap();
    let wrong_store = process.create_store().unwrap();
    let instance = process.instantiate(store, module).unwrap();

    assert_eq!(
        process
            .drop_instance(wrong_store, module, instance)
            .unwrap_err(),
        WasmError::WrongStoreAssociation
    );
    assert_eq!(
        process
            .drop_instance(store, wrong_module, instance)
            .unwrap_err(),
        WasmError::WrongModuleAssociation
    );
    assert_eq!(
        process
            .call_i32(store, module, instance, "add", &[40, 2])
            .unwrap(),
        [42]
    );
    process.drop_instance(store, module, instance).unwrap();
}

#[test]
fn failed_instantiation_stays_charged_until_store_teardown() {
    let limits = WasmLimits {
        max_instances: 1,
        max_instances_per_store: 1,
        fuel_per_operation: 100,
        ..WasmLimits::default()
    };
    let mut process = WasmProcess::new(limits).unwrap();
    let module = process.compile_module(START_SPIN).unwrap();
    let store = process.create_store().unwrap();

    assert_eq!(
        process.instantiate(store, module).unwrap_err(),
        WasmError::FuelExhausted
    );
    assert_eq!(process.live_counts().instances, 0);
    assert_eq!(process.live_counts().resident_instances, 1);
    assert_eq!(
        process.drop_module(module).unwrap_err(),
        WasmError::ResourceInUse {
            kind: IdentityKind::Module,
            dependents: 1
        }
    );
    assert_eq!(
        process.instantiate(store, module).unwrap_err(),
        WasmError::CapacityExceeded {
            kind: IdentityKind::Instance,
            maximum: 1
        }
    );
    assert_eq!(process.drop_store(store).unwrap(), 0);
    process.drop_module(module).unwrap();
}

#[test]
fn resident_instance_limit_survives_identity_invalidation_until_store_drop() {
    let limits = WasmLimits {
        max_instances: 1,
        max_instances_per_store: 1,
        ..WasmLimits::default()
    };
    let mut process = WasmProcess::new(limits).unwrap();
    let module = process.compile_module(EMPTY).unwrap();
    let store = process.create_store().unwrap();
    let instance = process.instantiate(store, module).unwrap();
    process.drop_instance(store, module, instance).unwrap();

    assert_eq!(
        process.instantiate(store, module).unwrap_err(),
        WasmError::CapacityExceeded {
            kind: IdentityKind::Instance,
            maximum: 1
        }
    );
    process.drop_store(store).unwrap();
    let replacement_store = process.create_store().unwrap();
    process.instantiate(replacement_store, module).unwrap();
}

#[test]
fn unsupported_values_and_non_function_exports_never_cross_the_boundary() {
    let mut process = process();
    let store = process.create_store().unwrap();
    let wrong_signature = process.compile_module(WRONG_SIGNATURE).unwrap();
    let instance = process.instantiate(store, wrong_signature).unwrap();
    assert!(matches!(
        process.call_i32(store, wrong_signature, instance, "f", &[1]),
        Err(WasmError::UnsupportedSignature { .. })
    ));

    let memory_module = process.compile_module(MEMORY_EXPORT).unwrap();
    let memory_instance = process.instantiate(store, memory_module).unwrap();
    assert_eq!(
        process
            .call_i32(store, memory_module, memory_instance, "memory", &[])
            .unwrap_err(),
        WasmError::ExportNotFunction {
            name: "memory".to_owned()
        }
    );
}

#[test]
fn module_store_and_instance_capacities_are_hard() {
    let limits = WasmLimits {
        max_modules: 1,
        max_stores: 1,
        max_instances: 1,
        max_instances_per_store: 1,
        ..WasmLimits::default()
    };
    let mut process = WasmProcess::new(limits).unwrap();
    let module = process.compile_module(EMPTY).unwrap();
    assert_eq!(
        process.compile_module(EMPTY).unwrap_err(),
        WasmError::CapacityExceeded {
            kind: IdentityKind::Module,
            maximum: 1
        }
    );
    let store = process.create_store().unwrap();
    assert_eq!(
        process.create_store().unwrap_err(),
        WasmError::CapacityExceeded {
            kind: IdentityKind::Store,
            maximum: 1
        }
    );
    process.instantiate(store, module).unwrap();
    assert_eq!(
        process.instantiate(store, module).unwrap_err(),
        WasmError::CapacityExceeded {
            kind: IdentityKind::Instance,
            maximum: 1
        }
    );
}

#[test]
fn shutdown_invalidates_external_interrupt_handle() {
    let process = process();
    let interrupt = process.interrupt_handle();
    let report = process.shutdown();
    assert_eq!(report.modules, 0);
    assert_eq!(interrupt.interrupt().unwrap_err(), WasmError::RuntimeClosed);
}
