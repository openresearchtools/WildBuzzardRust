//! Contract tests for the public embedding, rooting, callback, and job APIs.

use std::cell::RefCell;
use std::rc::Rc;

use wild_buzzard_js::{
    Context, Engine, ErrorKind, JsError, JsResult, RealmOptions, SourceText, ValueSnapshot,
};

#[test]
fn compiled_scripts_are_reusable_across_realms() {
    let engine = Engine::default();
    let script = engine
        .compile(&SourceText::new("shared.js", "20 + 22;"))
        .unwrap();
    let mut first = engine
        .create_realm(RealmOptions {
            name: "first".to_owned(),
        })
        .context();
    let mut second = engine
        .create_realm(RealmOptions {
            name: "second".to_owned(),
        })
        .context();

    let first_value = first.evaluate_script(&script).unwrap();
    let second_value = second.evaluate_script(&script).unwrap();
    assert_eq!(
        first.snapshot(&first_value).unwrap(),
        ValueSnapshot::Number(42.0)
    );
    assert_eq!(
        second.snapshot(&second_value).unwrap(),
        ValueSnapshot::Number(42.0)
    );
}

#[test]
fn roots_are_shared_by_clones_and_removed_on_last_drop() {
    let engine = Engine::default();
    let realm = engine.create_realm(RealmOptions::default());
    let context = realm.context();
    let initial = context.heap_statistics().roots;
    let value = context.string("rooted");
    assert_eq!(context.heap_statistics().roots, initial + 1);
    let clone = value.clone();
    assert_eq!(context.heap_statistics().roots, initial + 1);
    drop(value);
    assert_eq!(context.heap_statistics().roots, initial + 1);
    drop(clone);
    assert_eq!(context.heap_statistics().roots, initial);
}

#[test]
fn cross_realm_values_are_rejected_without_exposing_handles() {
    let engine = Engine::default();
    let first_realm = engine.create_realm(RealmOptions::default());
    let second_realm = engine.create_realm(RealmOptions::default());
    let first = first_realm.context();
    let second = second_realm.context();
    let value = first.number(1.0);
    let error = second.snapshot(&value).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::TypeError);
    assert!(error.message().contains("different realm"));
}

#[test]
fn host_functions_receive_and_return_only_rooted_values() {
    let engine = Engine::default();
    let realm = engine.create_realm(RealmOptions::default());
    let mut context = realm.context();
    context
        .define_host_function(
            "sum",
            2,
            |context: &mut Context,
             _: &wild_buzzard_js::RootedValue,
             arguments: &[wild_buzzard_js::RootedValue]| {
                let [left, right] = arguments else {
                    return Err(JsError::new(
                        ErrorKind::TypeError,
                        "sum needs two arguments",
                    ));
                };
                let ValueSnapshot::Number(left) = context.snapshot(left)? else {
                    return Err(JsError::new(ErrorKind::TypeError, "left is not a number"));
                };
                let ValueSnapshot::Number(right) = context.snapshot(right)? else {
                    return Err(JsError::new(ErrorKind::TypeError, "right is not a number"));
                };
                Ok(context.number(left + right))
            },
        )
        .unwrap();

    let result = context
        .evaluate(&SourceText::new("host.js", "sum(19, 23);"))
        .unwrap();
    assert_eq!(
        context.snapshot(&result).unwrap(),
        ValueSnapshot::Number(42.0)
    );
}

#[test]
fn embedding_can_build_objects_and_install_function_properties() {
    let engine = Engine::default();
    let realm = engine.create_realm(RealmOptions::default());
    let mut context = realm.context();
    let object = context.object();
    let value = context.string("Wild Buzzard");
    context.set_property(&object, "name", &value).unwrap();
    context.define_global("browser", &object, false).unwrap();

    let result = context
        .evaluate(&SourceText::new("object.js", "browser.name;"))
        .unwrap();
    assert_eq!(
        context.snapshot(&result).unwrap(),
        ValueSnapshot::String("Wild Buzzard".to_owned())
    );
}

#[test]
fn embedding_can_call_script_functions_with_an_explicit_this_value() {
    let engine = Engine::default();
    let realm = engine.create_realm(RealmOptions::default());
    let mut context = realm.context();
    let function = context
        .evaluate(&SourceText::new(
            "call.js",
            "(function(addend) { return this.base + addend; });",
        ))
        .unwrap();
    let receiver = context.object();
    let base = context.number(40.0);
    context.set_property(&receiver, "base", &base).unwrap();
    let addend = context.number(2.0);
    let result = context.call(&function, Some(&receiver), &[addend]).unwrap();
    assert_eq!(
        context.snapshot(&result).unwrap(),
        ValueSnapshot::Number(42.0)
    );
    let length = context.get_property(&function, "length").unwrap();
    assert_eq!(
        context.snapshot(&length).unwrap(),
        ValueSnapshot::Number(1.0)
    );
}

#[test]
fn jobs_are_fifo_and_stop_at_the_first_error() {
    let engine = Engine::default();
    let realm = engine.create_realm(RealmOptions::default());
    let mut context = realm.context();
    let events = Rc::new(RefCell::new(Vec::new()));

    let first = Rc::clone(&events);
    context.enqueue_job(move |_: &mut Context| -> JsResult<()> {
        first.borrow_mut().push(1);
        Ok(())
    });
    let second = Rc::clone(&events);
    context.enqueue_job(move |_: &mut Context| -> JsResult<()> {
        second.borrow_mut().push(2);
        Err(JsError::new(ErrorKind::TypeError, "job failed"))
    });
    let third = Rc::clone(&events);
    context.enqueue_job(move |_: &mut Context| -> JsResult<()> {
        third.borrow_mut().push(3);
        Ok(())
    });

    let error = context.run_jobs().unwrap_err();
    assert_eq!(error.completed, 1);
    assert_eq!(error.error.kind(), ErrorKind::TypeError);
    assert_eq!(&*events.borrow(), &[1, 2]);
    assert_eq!(context.pending_job_count(), 1);
    assert_eq!(context.run_jobs().unwrap(), 1);
    assert_eq!(&*events.borrow(), &[1, 2, 3]);
}

#[test]
fn host_errors_cross_the_call_boundary_with_script_stack_context() {
    let engine = Engine::default();
    let realm = engine.create_realm(RealmOptions::default());
    let mut context = realm.context();
    context
        .define_host_function(
            "fail",
            0,
            |_: &mut Context,
             _: &wild_buzzard_js::RootedValue,
             _: &[wild_buzzard_js::RootedValue]|
             -> JsResult<wild_buzzard_js::RootedValue> {
                Err(JsError::new(ErrorKind::TypeError, "host failure"))
            },
        )
        .unwrap();
    let error = context
        .evaluate(&SourceText::new(
            "host-stack.js",
            "function wrapper() { return fail(); } wrapper();",
        ))
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::TypeError);
    assert!(
        error
            .stack()
            .iter()
            .any(|frame| frame.function_name == "wrapper")
    );
    assert!(
        error
            .stack()
            .iter()
            .any(|frame| frame.function_name == "fail")
    );
}
