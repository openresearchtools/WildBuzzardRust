//! Ordinary-object, descriptor, prototype, construction, and Array contracts.

use wild_buzzard_js::{Context, Engine, ErrorKind, RealmOptions, SourceText, ValueSnapshot};

fn context() -> Context {
    Engine::default()
        .create_realm(RealmOptions::default())
        .context()
}

fn evaluate(context: &mut Context, source: &str) -> ValueSnapshot {
    let value = context
        .evaluate(&SourceText::new("object-model.js", source))
        .unwrap();
    context.snapshot(&value).unwrap()
}

#[test]
fn inherited_data_properties_shadow_on_the_receiver() {
    let mut context = context();
    assert_eq!(
        evaluate(
            &mut context,
            r#"
                let prototype = { value: 1 };
                Object.defineProperty(prototype, "locked", { value: 5 });
                let child = Object.create(prototype);
                child.value = 2;
                child.locked = 6;
                let shadowed = prototype.value === 1
                    && child.value === 2
                    && Object.hasOwn(child, "value");
                let deleted = delete child.value;
                shadowed
                    && deleted
                    && child.value === 1
                    && child.locked === 5
                    && !Object.hasOwn(child, "locked")
                    && !Object.hasOwn(child, "value");
            "#,
        ),
        ValueSnapshot::Boolean(true)
    );
}

#[test]
fn inherited_accessors_receive_the_original_receiver() {
    let mut context = context();
    assert_eq!(
        evaluate(
            &mut context,
            r#"
                let prototype = {};
                Object.defineProperty(prototype, "value", {
                    get: function () { return this.hidden; },
                    set: function (next) { this.hidden = next; },
                    configurable: true
                });
                let child = Object.create(prototype);
                child.hidden = 4;
                let before = child.value;
                child.value = 7;
                before === 4
                    && child.hidden === 7
                    && !Object.hasOwn(child, "value");
            "#,
        ),
        ValueSnapshot::Boolean(true)
    );
}

#[test]
fn descriptors_preserve_missing_fields_and_use_same_value() {
    let mut context = context();
    assert_eq!(
        evaluate(
            &mut context,
            r#"
                let object = {};
                Object.defineProperty(object, "fixed", { value: NaN });
                Object.defineProperty(object, "fixed", { value: NaN });
                let descriptor = Object.getOwnPropertyDescriptor(object, "fixed");
                object.fixed = 9;
                !(object.fixed === object.fixed)
                    && descriptor.writable === false
                    && descriptor.enumerable === false
                    && descriptor.configurable === false
                    && delete object.fixed === false;
            "#,
        ),
        ValueSnapshot::Boolean(true)
    );

    let error = context
        .evaluate(&SourceText::new(
            "signed-zero.js",
            r#"
                let signed = {};
                Object.defineProperty(signed, "zero", { value: 0 });
                Object.defineProperty(signed, "zero", { value: -0 });
            "#,
        ))
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::TypeError);
}

#[test]
fn descriptor_objects_use_inherited_fields_and_validate_accessors() {
    let mut context = context();
    assert_eq!(
        evaluate(
            &mut context,
            r#"
                let inherited = { enumerable: true, value: 12 };
                let descriptor = Object.create(inherited);
                let object = {};
                Object.defineProperty(object, "value", descriptor);
                let actual = Object.getOwnPropertyDescriptor(object, "value");
                actual.value === 12
                    && actual.enumerable === true
                    && actual.writable === false;
            "#,
        ),
        ValueSnapshot::Boolean(true)
    );

    let error = context
        .evaluate(&SourceText::new(
            "invalid-accessor.js",
            "Object.defineProperty({}, 'bad', { get: null });",
        ))
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::TypeError);
}

#[test]
fn accessor_updates_distinguish_absent_fields_from_explicit_undefined() {
    let mut context = context();
    assert_eq!(
        evaluate(
            &mut context,
            r#"
                let object = {};
                function first() { return 1; }
                function second() { return 2; }
                Object.defineProperty(object, "replaceable", {
                    get: first,
                    configurable: true
                });
                Object.defineProperty(object, "replaceable", { get: undefined });

                Object.defineProperty(object, "fixed", { get: first });
                Object.defineProperty(object, "fixed", { get: first });
                let rejected = false;
                try { Object.defineProperty(object, "fixed", { get: second }); }
                catch (error) { rejected = error.name === "TypeError"; }

                let replaceable = Object.getOwnPropertyDescriptor(object, "replaceable");
                replaceable.get === undefined
                    && replaceable.set === undefined
                    && object.replaceable === undefined
                    && object.fixed === 1
                    && rejected;
            "#,
        ),
        ValueSnapshot::Boolean(true)
    );
}

#[test]
fn prototype_mutation_is_observable_and_rejects_cycles() {
    let mut context = context();
    assert_eq!(
        evaluate(
            &mut context,
            r#"
                let first = {};
                let second = {};
                Object.setPrototypeOf(first, second);
                let rejected = false;
                try { Object.setPrototypeOf(second, first); }
                catch (error) { rejected = error.name === "TypeError"; }
                rejected
                    && Object.getPrototypeOf(first) === second
                    && Object.getPrototypeOf(second) === Object.prototype;
            "#,
        ),
        ValueSnapshot::Boolean(true)
    );
}

#[test]
fn non_extensible_objects_reject_new_properties_but_keep_existing_writes() {
    let mut context = context();
    assert_eq!(
        evaluate(
            &mut context,
            r#"
                let object = { existing: 1 };
                let prototype = Object.getPrototypeOf(object);
                Object.preventExtensions(object);
                object.missing = 2;
                object.existing = 3;
                let defineRejected = false;
                try { Object.defineProperty(object, "another", { value: 4 }); }
                catch (error) { defineRejected = error.name === "TypeError"; }
                Object.setPrototypeOf(object, prototype);
                !Object.isExtensible(object)
                    && !Object.hasOwn(object, "missing")
                    && object.existing === 3
                    && defineRejected;
            "#,
        ),
        ValueSnapshot::Boolean(true)
    );
}

#[test]
fn ordinary_construction_uses_function_prototypes_and_return_override() {
    let mut context = context();
    assert_eq!(
        evaluate(
            &mut context,
            r"
                function Constructor(value) { this.value = value; return 1; }
                function Override() { return { chosen: true }; }
                function Fallback() { this.fallback = true; }
                Fallback.prototype = 0;
                let instance = new Constructor(42);
                let replacement = new Override();
                let fallback = new Fallback();
                instance.value === 42
                    && Object.getPrototypeOf(instance) === Constructor.prototype
                    && instance.constructor === Constructor
                    && replacement.chosen === true
                    && fallback.fallback === true
                    && Object.getPrototypeOf(fallback) === Object.prototype;
            ",
        ),
        ValueSnapshot::Boolean(true)
    );
}

#[test]
fn constructibility_is_checked_after_argument_evaluation() {
    let mut context = context();
    assert_eq!(
        evaluate(
            &mut context,
            r"
                let calls = 0;
                function argument() { calls = calls + 1; return 0; }
                try { new (Object.create(null))(argument()); }
                catch (error) { }
                calls === 1;
            ",
        ),
        ValueSnapshot::Boolean(true)
    );
}

#[test]
fn array_elisions_indices_and_deletion_keep_holes_distinct() {
    let mut context = context();
    assert_eq!(
        evaluate(
            &mut context,
            r#"
                let array = [1, , 3];
                let storedUndefined = [undefined];
                let prototype = { 1: 7 };
                Object.setPrototypeOf(array, prototype);
                let inherited = array[1];
                let deleted = delete array[2];
                array["01"] = 8;
                array["4294967295"] = 9;
                array.length === 3
                    && inherited === 7
                    && !Object.hasOwn(array, "1")
                    && deleted
                    && !Object.hasOwn(array, "2")
                    && Object.hasOwn(storedUndefined, "0")
                    && storedUndefined[0] === undefined
                    && Object.hasOwn(array, "01")
                    && Object.hasOwn(array, "4294967295");
            "#,
        ),
        ValueSnapshot::Boolean(true)
    );
}

#[test]
fn array_length_truncation_is_descending_and_reports_partial_failure() {
    let mut context = context();
    assert_eq!(
        evaluate(
            &mut context,
            r#"
                let array = [0, 1, 2, 3];
                Object.defineProperty(array, "2", { configurable: false });
                let rejected = false;
                try {
                    Object.defineProperty(array, "length", {
                        value: 1,
                        writable: false
                    });
                } catch (error) { rejected = error.name === "TypeError"; }
                let length = Object.getOwnPropertyDescriptor(array, "length");
                rejected
                    && array.length === 3
                    && Object.hasOwn(array, "2")
                    && !Object.hasOwn(array, "3")
                    && length.writable === false;
            "#,
        ),
        ValueSnapshot::Boolean(true)
    );
}

#[test]
fn array_constructor_push_pop_and_is_array_form_a_coherent_nucleus() {
    let mut context = context();
    assert_eq!(
        evaluate(
            &mut context,
            r#"
                let holes = Array(3);
                let values = new Array(1, 2);
                let pushed = values.push(3);
                let popped = values.pop();
                Array.isArray(holes)
                    && Array.isArray(values)
                    && !Array.isArray({})
                    && holes.length === 3
                    && !Object.hasOwn(holes, "0")
                    && pushed === 3
                    && popped === 3
                    && values.length === 2;
            "#,
        ),
        ValueSnapshot::Boolean(true)
    );
}

#[test]
fn array_length_range_and_non_writable_failures_are_explicit() {
    let mut context = context();
    assert_eq!(
        evaluate(
            &mut context,
            r#"
                let maximum = Array(4294967295);
                maximum["4294967294"] = 1;
                let overflow = false;
                try { maximum.push(2); }
                catch (error) { overflow = error.name === "RangeError"; }

                let frozen = [];
                Object.defineProperty(frozen, "length", { writable: false });
                let pushRejected = false;
                let popRejected = false;
                try { frozen.push(); }
                catch (error) { pushRejected = error.name === "TypeError"; }
                try { frozen.pop(); }
                catch (error) { popRejected = error.name === "TypeError"; }
                maximum.length === 4294967295
                    && overflow
                    && pushRejected
                    && popRejected;
            "#,
        ),
        ValueSnapshot::Boolean(true)
    );

    let error = context
        .evaluate(&SourceText::new("array-range.js", "Array(1.5);"))
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::RangeError);
}

#[test]
fn function_metadata_is_represented_by_real_descriptors() {
    let mut context = context();
    assert_eq!(
        evaluate(
            &mut context,
            r#"
                function sample(left, right) { }
                let name = Object.getOwnPropertyDescriptor(sample, "name");
                let length = Object.getOwnPropertyDescriptor(sample, "length");
                let prototype = Object.getOwnPropertyDescriptor(sample, "prototype");
                let anonymous = Object.getOwnPropertyDescriptor(function () {}, "name");
                name.value === "sample"
                    && name.writable === false
                    && name.enumerable === false
                    && name.configurable === true
                    && length.value === 2
                    && prototype.writable === true
                    && prototype.configurable === false
                    && anonymous.value === "";
            "#,
        ),
        ValueSnapshot::Boolean(true)
    );
}

#[test]
fn collector_traces_prototypes_accessors_arrays_and_function_cycles() {
    let mut context = context();
    let live = context
        .evaluate(&SourceText::new(
            "object-graph-gc.js",
            r#"
                (function () {
                    let prototype = { inherited: "kept" };
                    Object.defineProperty(prototype, "computed", {
                        get: function () { return this.own + this.inherited; }
                    });
                    let array = [prototype];
                    let child = Object.create(prototype);
                    child.own = "also kept";
                    child.array = array;
                    return child;
                })();
            "#,
        ))
        .unwrap();

    let first_collection = context.collect_garbage().unwrap();
    let computed = context.get_property(&live, "computed").unwrap();
    assert_eq!(
        context.snapshot(&computed).unwrap(),
        ValueSnapshot::String("also keptkept".to_owned())
    );
    drop(computed);
    drop(live);
    let report = context.collect_garbage().unwrap();
    assert!(report.reclaimed.objects >= 4);
    assert!(first_collection.reclaimed.functions + report.reclaimed.functions >= 2);
    assert!(report.reclaimed.strings >= 4);
}
