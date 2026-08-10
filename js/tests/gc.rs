//! Contract tests for explicit tracing collection and embedding safe points.

use wild_buzzard_js::{
    CollectionErrorKind, Context, Engine, JsResult, JsString, RealmOptions, RootedValue,
    SourceText, ValueSnapshot,
};

fn context() -> Context {
    Engine::default()
        .create_realm(RealmOptions::default())
        .context()
}

fn string(value: &str) -> ValueSnapshot {
    ValueSnapshot::String(JsString::from_utf8(value).unwrap())
}

#[test]
fn rooted_values_and_their_property_graph_survive_collection() {
    let mut context = context();
    let object = context.object();
    let text = context.string("rooted child").unwrap();
    context.set_property(&object, "text", &text).unwrap();
    drop(text);

    let report = context.collect_garbage().unwrap();
    assert_eq!(report.reclaimed.total(), 0);
    let property = context.get_property(&object, "text").unwrap();
    assert_eq!(context.snapshot(&property).unwrap(), string("rooted child"));
}

#[test]
fn global_bindings_are_permanent_trace_roots() {
    let mut context = context();
    let object = context.object();
    let label = context.string("global child").unwrap();
    context.set_property(&object, "label", &label).unwrap();
    context.define_global("saved", &object, false).unwrap();
    drop(label);
    drop(object);

    context.collect_garbage().unwrap();
    let result = context
        .evaluate(&SourceText::new("global-gc.js", "saved.label;"))
        .unwrap();
    assert_eq!(context.snapshot(&result).unwrap(), string("global child"));
}

#[test]
fn unreachable_mixed_cycles_reclaim_every_allocation_kind() {
    let mut context = context();
    let result = context
        .evaluate(&SourceText::new(
            "dead-cycle.js",
            r#"
                (function () {
                    let text = "dead";
                    let object = {};
                    object.self = object;
                    let closure = function () { return object; };
                    closure.text = text;
                    object.closure = closure;
                })();
            "#,
        ))
        .unwrap();
    drop(result);

    let report = context.collect_garbage().unwrap();
    // Each script function owns a traced `name` string and constructor
    // prototype object in addition to the explicitly allocated graph.
    assert_eq!(report.reclaimed.strings, 3);
    assert_eq!(report.reclaimed.objects, 3);
    assert_eq!(report.reclaimed.functions, 2);
    assert_eq!(report.reclaimed.environments, 1);
    assert_eq!(report.after.environments, 2);
}

#[test]
fn rooted_closures_keep_captured_environments_alive() {
    let mut context = context();
    let closure = context
        .evaluate(&SourceText::new(
            "live-closure.js",
            r#"
                (function () {
                    let captured = "closure value";
                    return function () { return captured; };
                })();
            "#,
        ))
        .unwrap();

    context.collect_garbage().unwrap();
    let result = context.call(&closure, None, &[]).unwrap();
    assert_eq!(context.snapshot(&result).unwrap(), string("closure value"));

    drop(result);
    drop(closure);
    let report = context.collect_garbage().unwrap();
    assert_eq!(report.reclaimed.strings, 2);
    assert_eq!(report.reclaimed.objects, 1);
    assert_eq!(report.reclaimed.functions, 1);
    assert!(report.reclaimed.environments >= 1);
}

#[test]
fn rooted_loop_closure_keeps_its_distinct_iteration_environment_alive() {
    let mut context = context();
    let closure = context
        .evaluate(&SourceText::new(
            "loop-closure-gc.js",
            r"
                (function () {
                    let closures = [];
                    for (let index = 0; index < 2; index = index + 1) {
                        closures.push(function () { return index; });
                    }
                    return closures[1];
                })();
            ",
        ))
        .unwrap();

    context.collect_garbage().unwrap();
    let result = context.call(&closure, None, &[]).unwrap();
    assert_eq!(
        context.snapshot(&result).unwrap(),
        ValueSnapshot::Number(1.0)
    );

    drop(result);
    drop(closure);
    let report = context.collect_garbage().unwrap();
    assert!(report.reclaimed.environments >= 2);
}

#[test]
fn catch_environment_reachability_survives_an_intervening_collection() {
    let mut context = context();
    let closure = context
        .evaluate(&SourceText::new(
            "catch-closure-gc.js",
            r#"
                (function () {
                    try { throw "kept by catch"; }
                    catch (value) { return function () { return value; }; }
                })();
            "#,
        ))
        .unwrap();

    context.collect_garbage().unwrap();
    let result = context.call(&closure, None, &[]).unwrap();
    assert_eq!(context.snapshot(&result).unwrap(), string("kept by catch"));
}

#[test]
fn dropping_the_last_root_permits_reclamation() {
    let mut context = context();
    let object = context.object();
    let text = context.string("last root").unwrap();
    context.set_property(&object, "text", &text).unwrap();
    drop(text);

    context.collect_garbage().unwrap();
    drop(object);
    let report = context.collect_garbage().unwrap();
    assert_eq!(report.reclaimed.objects, 1);
    assert_eq!(report.reclaimed.strings, 1);
}

#[test]
fn reusable_slots_do_not_grow_capacity_or_revive_stale_roots() {
    let mut context = context();
    let baseline = context.heap_statistics().arenas.objects;
    let first = context.object();
    let capacity = context.heap_statistics().arenas.objects.capacity;
    drop(first);

    context.collect_garbage().unwrap();
    let swept = context.heap_statistics().arenas.objects;
    assert_eq!(swept.capacity, capacity);
    assert_eq!(swept.live, baseline.live);
    assert_eq!(swept.reusable, baseline.reusable + 1);

    let second = context.object();
    let reused = context.heap_statistics().arenas.objects;
    assert_eq!(reused.capacity, capacity);
    assert_eq!(reused.live, baseline.live + 1);
    assert_eq!(reused.reusable, baseline.reusable);
    assert_eq!(context.snapshot(&second).unwrap(), ValueSnapshot::Object);
}

#[test]
fn repeated_collection_is_idempotent_apart_from_diagnostics() {
    let mut context = context();
    let value = context.string("temporary").unwrap();
    drop(value);

    let first = context.collect_garbage().unwrap();
    let second = context.collect_garbage().unwrap();
    assert_eq!(first.reclaimed.strings, 1);
    assert_eq!(second.reclaimed.total(), 0);
    assert_eq!(first.after.strings, second.after.strings);
    assert_eq!(first.after.objects, second.after.objects);
    assert_eq!(first.after.functions, second.after.functions);
    assert_eq!(first.after.environments, second.after.environments);
    assert_eq!(second.after.collections, first.after.collections + 1);
}

#[test]
fn host_callback_and_job_entries_reject_collection_deterministically() {
    let mut context = context();
    context
        .define_host_function(
            "tryCollection",
            0,
            |context: &mut Context, _: &RootedValue, _: &[RootedValue]| {
                let error = context.collect_garbage().unwrap_err();
                Ok(context.boolean(error.kind() == CollectionErrorKind::ActiveExecution))
            },
        )
        .unwrap();

    let result = context
        .evaluate(&SourceText::new("host-gc.js", "tryCollection();"))
        .unwrap();
    assert_eq!(
        context.snapshot(&result).unwrap(),
        ValueSnapshot::Boolean(true)
    );

    context.enqueue_job(|context: &mut Context| -> JsResult<()> {
        let error = context.collect_garbage().unwrap_err();
        assert_eq!(error.kind(), CollectionErrorKind::ActiveExecution);
        Ok(())
    });
    assert_eq!(context.run_jobs().unwrap(), 1);
    context.collect_garbage().unwrap();
}

#[test]
fn rooted_values_captured_by_host_functions_and_jobs_are_enumerated() {
    let mut context = context();
    let host_value = context.string("host persistent root").unwrap();
    let captured_by_host = host_value.clone();
    context
        .define_host_function(
            "readCaptured",
            0,
            move |_: &mut Context, _: &RootedValue, _: &[RootedValue]| Ok(captured_by_host.clone()),
        )
        .unwrap();
    drop(host_value);

    context.collect_garbage().unwrap();
    let result = context
        .evaluate(&SourceText::new("host-root.js", "readCaptured();"))
        .unwrap();
    assert_eq!(
        context.snapshot(&result).unwrap(),
        string("host persistent root")
    );
    drop(result);

    let job_value = context.string("queued persistent root").unwrap();
    let captured_by_job = job_value.clone();
    drop(job_value);
    context.enqueue_job(move |context: &mut Context| -> JsResult<()> {
        assert_eq!(
            context.snapshot(&captured_by_job)?,
            string("queued persistent root")
        );
        Ok(())
    });

    context.collect_garbage().unwrap();
    assert_eq!(context.run_jobs().unwrap(), 1);
    let report = context.collect_garbage().unwrap();
    assert_eq!(report.reclaimed.strings, 1);
}

#[test]
fn failed_execution_restores_the_safe_point_and_collector_usability() {
    let mut context = context();
    let error = context
        .evaluate(&SourceText::new(
            "failed-execution-gc.js",
            r"
                (function () {
                    let object = {};
                    object.self = object;
                    missingBinding;
                })();
            ",
        ))
        .unwrap_err();
    assert!(error.message().contains("missingBinding"));

    let report = context.collect_garbage().unwrap();
    assert_eq!(report.reclaimed.objects, 2);
    assert!(report.reclaimed.functions >= 1);
    assert!(report.reclaimed.environments >= 1);
    context.collect_garbage().unwrap();
}

#[test]
fn rooted_exception_values_survive_until_the_error_is_dropped() {
    let mut context = context();
    let error = context
        .evaluate(&SourceText::new(
            "exception-gc.js",
            r"
                (function () {
                    let object = {};
                    object.self = object;
                    throw object;
                })();
            ",
        ))
        .unwrap_err();
    let exception = error.exception_value().unwrap();
    assert_eq!(context.snapshot(exception).unwrap(), ValueSnapshot::Object);

    context.collect_garbage().unwrap();
    assert_eq!(context.snapshot(exception).unwrap(), ValueSnapshot::Object);
    drop(error);

    let report = context.collect_garbage().unwrap();
    assert_eq!(report.reclaimed.objects, 1);
}

#[test]
fn rooted_lone_surrogates_survive_collection_exactly() {
    let mut context = context();
    let rooted = context
        .string_from_code_units(&[0xd800, 0, 0xdc00])
        .unwrap();

    let report = context.collect_garbage().unwrap();
    assert_eq!(report.reclaimed.total(), 0);
    let ValueSnapshot::String(snapshot) = context.snapshot(&rooted).unwrap() else {
        panic!("expected a string snapshot");
    };
    assert_eq!(snapshot.as_code_units(), &[0xd800, 0, 0xdc00]);
}

#[test]
fn owned_property_key_outlives_its_originating_string_handle() {
    let mut context = context();
    let object = context.object();
    let value = context.number(42.0);
    let rooted_key = context.string_from_code_units(&[0xd800]).unwrap();
    let ValueSnapshot::String(key) = context.snapshot(&rooted_key).unwrap() else {
        panic!("expected a string snapshot");
    };
    context.set_property_by_key(&object, &key, &value).unwrap();
    drop(rooted_key);

    let report = context.collect_garbage().unwrap();
    assert_eq!(report.reclaimed.strings, 1);
    let property = context.get_property_by_key(&object, &key).unwrap();
    assert_eq!(
        context.snapshot(&property).unwrap(),
        ValueSnapshot::Number(42.0)
    );
}
