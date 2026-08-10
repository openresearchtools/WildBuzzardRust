//! Symbol identity, property-key ordering, embedding, and tracing contracts.

use wild_buzzard_js::{
    Context, Engine, ErrorKind, JsString, RealmOptions, SourceText, ValueSnapshot, ValueType,
};

fn context() -> Context {
    Engine::default()
        .create_realm(RealmOptions::default())
        .context()
}

fn evaluate(context: &mut Context, source: &str) -> ValueSnapshot {
    let value = context
        .evaluate(&SourceText::new("symbols.js", source))
        .unwrap();
    context.snapshot(&value).unwrap()
}

#[test]
fn symbol_primitives_have_fresh_identity_and_exact_optional_descriptions() {
    let mut context = context();
    assert_eq!(
        evaluate(
            &mut context,
            r#"
                let absent = Symbol();
                let explicitUndefined = Symbol(undefined);
                let empty = Symbol("");
                let lone = Symbol("\uD800");
                let prototypeDescriptor = Object.getOwnPropertyDescriptor(Symbol, "prototype");
                typeof absent === "symbol"
                    && absent !== explicitUndefined
                    && absent !== Symbol()
                    && empty !== Symbol("")
                    && absent.description === undefined
                    && explicitUndefined.description === undefined
                    && empty.description === ""
                    && lone.description === "\uD800"
                    && lone.toString() === "Symbol(\uD800)"
                    && lone.valueOf() === lone
                    && prototypeDescriptor.writable === false
                    && prototypeDescriptor.enumerable === false
                    && prototypeDescriptor.configurable === false;
            "#,
        ),
        ValueSnapshot::Boolean(true)
    );

    assert_eq!(
        evaluate(
            &mut context,
            r#"
                let descriptionRejected = false;
                let stringRejected = false;
                let valueRejected = false;
                try { Symbol.prototype.description; }
                catch (error) { descriptionRejected = error.name === "TypeError"; }
                try { Symbol.prototype.toString(); }
                catch (error) { stringRejected = error.name === "TypeError"; }
                try { Symbol.prototype.valueOf(); }
                catch (error) { valueRejected = error.name === "TypeError"; }
                descriptionRejected && stringRejected && valueRejected;
            "#,
        ),
        ValueSnapshot::Boolean(true)
    );

    let error = context
        .evaluate(&SourceText::new("symbol-new.js", "new Symbol();"))
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::TypeError);

    assert_eq!(
        evaluate(
            &mut context,
            r#"
                let symbol = Symbol("coercion");
                let rejected = false;
                let constructorRejected = false;
                try { "prefix" + symbol; }
                catch (error) { rejected = error.name === "TypeError"; }
                try { Symbol(symbol); }
                catch (error) { constructorRejected = error.name === "TypeError"; }
                rejected && constructorRejected;
            "#,
        ),
        ValueSnapshot::Boolean(true)
    );
}

#[test]
fn typeof_unresolvable_identifier_returns_undefined() {
    let mut context = context();
    assert_eq!(
        evaluate(&mut context, "typeof unresolvableIdentifier;"),
        ValueSnapshot::String(JsString::from_utf8("undefined").unwrap())
    );
}

#[test]
fn symbol_keys_are_identity_based_and_share_descriptor_operations() {
    let mut context = context();
    assert_eq!(
        evaluate(
            &mut context,
            r#"
                let first = Symbol("same");
                let second = Symbol("same");
                let object = {};
                object.same = 3;
                object[first] = 1;
                Object.defineProperty(object, second, {
                    value: 2,
                    writable: true,
                    configurable: true
                });
                let descriptor = Object.getOwnPropertyDescriptor(object, first);
                object[first] === 1
                    && object[second] === 2
                    && object.same === 3
                    && first !== "same"
                    && Object.hasOwn(object, first)
                    && Object.hasOwn(object, second)
                    && Object.hasOwn(object, "same")
                    && descriptor.value === 1
                    && delete object[first]
                    && !Object.hasOwn(object, first)
                    && Object.hasOwn(object, second);
            "#,
        ),
        ValueSnapshot::Boolean(true)
    );
}

#[test]
fn ordinary_own_keys_follow_category_order_and_stable_insertion_rules() {
    let mut context = context();
    assert_eq!(
        evaluate(
            &mut context,
            r#"
                (function () {
                let first = Symbol("first");
                let second = Symbol("second");
                let object = {};
                object.z = 1;
                object["2"] = 2;
                object[first] = 3;
                object["1"] = 4;
                object.y = 5;
                object.x = 6;
                object[second] = 7;
                Object.defineProperty(object, "z", { value: 8 });
                Object.defineProperty(object, second, { value: 8 });
                delete object.y;
                object.y = 9;
                delete object[first];
                object[first] = 10;

                let keys = Reflect.ownKeys(object);
                let names = Object.getOwnPropertyNames(object);
                let symbols = Object.getOwnPropertySymbols(object);
                return keys.length === 7
                    && keys[0] === "1"
                    && keys[1] === "2"
                    && keys[2] === "z"
                    && keys[3] === "x"
                    && keys[4] === "y"
                    && keys[5] === second
                    && keys[6] === first
                    && names.length === 5
                    && names[0] === keys[0]
                    && names[4] === keys[4]
                    && symbols.length === 2
                    && symbols[0] === second
                    && symbols[1] === first;
                })();
            "#,
        ),
        ValueSnapshot::Boolean(true)
    );
}

#[test]
fn array_boundary_and_function_own_keys_preserve_their_required_order() {
    let mut context = context();
    assert_eq!(
        evaluate(
            &mut context,
            r#"
                (function () {
                let marker = Symbol("array");
                let array = [];
                array[3] = "three";
                array[1] = "one";
                array.note = "named";
                array[marker] = "symbol";
                let keys = Reflect.ownKeys(array);
                return keys.length === 5
                    && keys[0] === "1"
                    && keys[1] === "3"
                    && keys[2] === "length"
                    && keys[3] === "note"
                    && keys[4] === marker;
                })();
            "#,
        ),
        ValueSnapshot::Boolean(true)
    );

    assert_eq!(
        evaluate(
            &mut context,
            r#"
                (function () {
                let boundary = {};
                boundary["4294967295"] = 1;
                boundary["4294967294"] = 2;
                boundary["12"] = 3;
                boundary["12345678900"] = 4;
                let keys = Reflect.ownKeys(boundary);
                return keys[0] === "12"
                    && keys[1] === "4294967294"
                    && keys[2] === "4294967295"
                    && keys[3] === "12345678900";
                })();
            "#,
        ),
        ValueSnapshot::Boolean(true)
    );

    assert_eq!(
        evaluate(
            &mut context,
            r#"
                (function () {
                function sample() {}
                let keys = Reflect.ownKeys(sample);
                return keys.length === 3
                    && keys[0] === "length"
                    && keys[1] === "name"
                    && keys[2] === "prototype";
                })();
            "#,
        ),
        ValueSnapshot::Boolean(true)
    );
}

#[test]
fn embedding_uses_rooted_symbols_and_exact_descriptions_without_identity_tokens() {
    let mut context = context();
    let absent = context.symbol(None).unwrap();
    assert_eq!(context.snapshot(&absent).unwrap(), ValueSnapshot::Symbol);
    assert_eq!(context.value_type(&absent).unwrap(), ValueType::Symbol);
    assert_eq!(context.symbol_description(&absent).unwrap(), None);

    let empty_description = JsString::from_code_units(&[]).unwrap();
    let empty = context.symbol(Some(&empty_description)).unwrap();
    assert_eq!(
        context.symbol_description(&empty).unwrap(),
        Some(empty_description)
    );

    let description = JsString::from_code_units(&[u16::from(b'a'), 0xD800]).unwrap();
    let first = context.symbol(Some(&description)).unwrap();
    let second = context.symbol(Some(&description)).unwrap();
    assert_eq!(
        context.symbol_description(&first).unwrap(),
        Some(description.clone())
    );

    let object = context.object();
    let value = context.number(42.0);
    context
        .set_property_by_symbol(&object, &first, &value)
        .unwrap();
    let first_property = context.get_property_by_symbol(&object, &first).unwrap();
    assert_eq!(
        context.snapshot(&first_property).unwrap(),
        ValueSnapshot::Number(42.0)
    );
    let second_property = context.get_property_by_symbol(&object, &second).unwrap();
    assert_eq!(
        context.snapshot(&second_property).unwrap(),
        ValueSnapshot::Undefined
    );
}

#[test]
fn embedding_symbol_operations_reject_cross_realm_values() {
    let engine = Engine::default();
    let first_realm = engine.create_realm(RealmOptions::default());
    let second_realm = engine.create_realm(RealmOptions::default());
    let first_context = first_realm.context();
    let mut second_context = second_realm.context();
    let symbol = first_context.symbol(None).unwrap();
    let object = second_context.object();

    let error = second_context
        .get_property_by_symbol(&object, &symbol)
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::TypeError);
}

#[test]
fn symbol_property_keys_trace_until_deletion_and_then_become_collectible() {
    let mut context = context();
    let object = context.object();
    let description = JsString::from_utf8("traced key").unwrap();
    let symbol = context.symbol(Some(&description)).unwrap();
    let value = context.number(1.0);
    context
        .set_property_by_symbol(&object, &symbol, &value)
        .unwrap();
    context
        .define_global("savedSymbolObject", &object, false)
        .unwrap();
    drop(symbol);

    let report = context.collect_garbage().unwrap();
    assert_eq!(report.reclaimed.symbols, 0);
    let recovered = context
        .evaluate(&SourceText::new(
            "recover-symbol.js",
            "Object.getOwnPropertySymbols(savedSymbolObject)[0];",
        ))
        .unwrap();
    assert_eq!(
        context.symbol_description(&recovered).unwrap(),
        Some(description)
    );
    drop(recovered);

    let deleted = context
        .evaluate(&SourceText::new(
            "delete-symbol.js",
            r"
                (function () {
                    let key = Object.getOwnPropertySymbols(savedSymbolObject)[0];
                    return delete savedSymbolObject[key];
                })();
            ",
        ))
        .unwrap();
    assert_eq!(
        context.snapshot(&deleted).unwrap(),
        ValueSnapshot::Boolean(true)
    );
    drop(deleted);
    let report = context.collect_garbage().unwrap();
    assert_eq!(report.reclaimed.symbols, 1);
}

#[test]
fn symbol_description_outlives_the_temporary_string_value_that_created_it() {
    let mut context = context();
    let symbol = context
        .evaluate(&SourceText::new(
            "symbol-description-gc.js",
            "Symbol('exact \\uD800 description');",
        ))
        .unwrap();
    let report = context.collect_garbage().unwrap();
    assert_eq!(report.reclaimed.symbols, 0);
    assert_eq!(report.reclaimed.strings, 1);
    assert_eq!(
        context.symbol_description(&symbol).unwrap(),
        Some(
            JsString::from_code_units(&[
                u16::from(b'e'),
                u16::from(b'x'),
                u16::from(b'a'),
                u16::from(b'c'),
                u16::from(b't'),
                u16::from(b' '),
                0xD800,
                u16::from(b' '),
                u16::from(b'd'),
                u16::from(b'e'),
                u16::from(b's'),
                u16::from(b'c'),
                u16::from(b'r'),
                u16::from(b'i'),
                u16::from(b'p'),
                u16::from(b't'),
                u16::from(b'i'),
                u16::from(b'o'),
                u16::from(b'n'),
            ])
            .unwrap()
        )
    );
    drop(symbol);
    assert_eq!(context.collect_garbage().unwrap().reclaimed.symbols, 1);
}

#[test]
fn reflect_own_keys_rejects_symbol_primitives() {
    let mut context = context();
    let error = context
        .evaluate(&SourceText::new(
            "reflect-symbol-target.js",
            "Reflect.ownKeys(Symbol('target'));",
        ))
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::TypeError);
}
