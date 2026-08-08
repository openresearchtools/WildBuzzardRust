//! End-to-end tests for the implemented ECMAScript subset.

use wild_buzzard_js::{
    Context, Engine, ErrorKind, ExecutionLimits, RealmOptions, SourceText, ValueSnapshot,
};

fn context() -> Context {
    Engine::default()
        .create_realm(RealmOptions::default())
        .context()
}

fn evaluate(context: &mut Context, source: &str) -> ValueSnapshot {
    let value = context
        .evaluate(&SourceText::new("semantics.js", source))
        .unwrap();
    context.snapshot(&value).unwrap()
}

#[test]
fn primitives_arithmetic_comparison_and_logical_values() {
    let mut context = context();
    let result = evaluate(
        &mut context,
        r#"
            let numeric = 1 + 2 * 3 - 4 / 2 + 5 % 2;
            let comparisons = numeric === 6 && "3" > "2" && 2 <= 2;
            comparisons && ("value:" + numeric);
        "#,
    );
    assert_eq!(result, ValueSnapshot::String("value:6".to_owned()));
}

#[test]
fn abstract_and_strict_equality_cover_numeric_edges() {
    let mut context = context();
    assert_eq!(
        evaluate(
            &mut context,
            r#"
                null == undefined
                    && "2" == 2
                    && false == 0
                    && !(NaN === NaN)
                    && 0 === -0
                    && !({} == null)
                    && +"" === 0
                    && !((+"0x") === (+"0x"));
            "#,
        ),
        ValueSnapshot::Boolean(true)
    );
}

#[test]
fn lexical_shadowing_assignment_and_loop_control() {
    let mut context = context();
    assert_eq!(
        evaluate(
            &mut context,
            r"
                let x = 1;
                { let x = 40; x = x + 2; }
                while (x < 8) {
                    x = x + 1;
                    if (x === 3) { continue; }
                    if (x === 7) { break; }
                }
                x;
            ",
        ),
        ValueSnapshot::Number(7.0)
    );
}

#[test]
fn lexical_tdz_applies_before_declaration_and_through_closures() {
    let mut context = context();
    let error = context
        .evaluate(&SourceText::new(
            "tdz.js",
            r"
                {
                    function read() { return value; }
                    read();
                    let value = 1;
                }
            ",
        ))
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::ReferenceError);
    assert!(error.message().contains("before initialization"));
    assert_eq!(error.location().unwrap().source_name, "tdz.js");
    assert!(!error.stack().is_empty());
}

#[test]
fn const_assignment_is_a_type_error() {
    let mut context = context();
    let error = context
        .evaluate(&SourceText::new(
            "const.js",
            "const answer = 42; answer = 0;",
        ))
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::TypeError);
    assert!(error.message().contains("constant binding"));
}

#[test]
fn top_level_lexical_bindings_persist_across_scripts() {
    let mut context = context();
    assert_eq!(
        evaluate(&mut context, "let value = 40; value;"),
        ValueSnapshot::Number(40.0)
    );
    assert_eq!(
        evaluate(&mut context, "value = value + 2; value;"),
        ValueSnapshot::Number(42.0)
    );
    let error = context
        .evaluate(&SourceText::new("duplicate.js", "let value = 0;"))
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::SyntaxError);
    assert_eq!(
        evaluate(&mut context, "value;"),
        ValueSnapshot::Number(42.0)
    );
}

#[test]
fn closures_retain_and_mutate_captured_environments() {
    let mut context = context();
    assert_eq!(
        evaluate(
            &mut context,
            r"
                function makeCounter(start) {
                    let value = start;
                    return function(step) {
                        value = value + step;
                        return value;
                    };
                }
                let counter = makeCounter(2);
                counter(3) + counter(4);
            ",
        ),
        ValueSnapshot::Number(14.0)
    );
}

#[test]
fn declarations_are_instantiated_before_execution_for_recursion() {
    let mut context = context();
    assert_eq!(
        evaluate(
            &mut context,
            r"
                let result = factorial(6);
                function factorial(n) {
                    if (n <= 1) { return 1; }
                    return n * factorial(n - 1);
                }
                result;
            ",
        ),
        ValueSnapshot::Number(720.0)
    );
}

#[test]
fn objects_support_computed_access_mutation_and_method_this() {
    let mut context = context();
    assert_eq!(
        evaluate(
            &mut context,
            r#"
                let object = {
                    value: 2,
                    add: function(amount) {
                        this.value = this.value + amount;
                        return this.value;
                    }
                };
                object.add(5);
                object["value"];
            "#,
        ),
        ValueSnapshot::Number(7.0)
    );
}

#[test]
fn throw_catch_and_finally_preserve_completion_rules() {
    let mut context = context();
    assert_eq!(
        evaluate(
            &mut context,
            r#"
                function caught() {
                    try { throw "boom"; }
                    catch (value) { return value + "!"; }
                    finally { 1; }
                }
                function overridden() {
                    try { return 1; }
                    finally { return 2; }
                }
                caught() + overridden();
            "#,
        ),
        ValueSnapshot::String("boom!2".to_owned())
    );
}

#[test]
fn runtime_errors_are_catchable_error_objects() {
    let mut context = context();
    assert_eq!(
        evaluate(
            &mut context,
            r"
                try { missingBinding; }
                catch (error) { error.name; }
            ",
        ),
        ValueSnapshot::String("ReferenceError".to_owned())
    );
}

#[test]
fn uncaught_throw_preserves_a_rooted_exception_value() {
    let mut context = context();
    let error = context
        .evaluate(&SourceText::new("throw.js", "throw 17;"))
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Exception);
    let thrown = error.exception_value().unwrap();
    assert_eq!(
        context.snapshot(thrown).unwrap(),
        ValueSnapshot::Number(17.0)
    );
}

#[test]
fn automatic_semicolon_insertion_after_return_observes_newline() {
    let mut context = context();
    assert_eq!(
        evaluate(
            &mut context,
            r"
                function value() {
                    return
                    99;
                }
                value();
            ",
        ),
        ValueSnapshot::Undefined
    );
}

#[test]
fn execution_step_limit_stops_infinite_loop_deterministically() {
    let mut context = context();
    context.set_execution_limits(ExecutionLimits {
        max_steps: 64,
        max_call_depth: 32,
    });
    let error = context
        .evaluate(&SourceText::new("loop.js", "while (true) {}"))
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::ResourceLimit);
    assert_eq!(error.location().unwrap().source_name, "loop.js");
}

#[test]
fn call_depth_limit_produces_range_error_and_stack() {
    let mut context = context();
    context.set_execution_limits(ExecutionLimits {
        max_steps: 10_000,
        max_call_depth: 8,
    });
    let error = context
        .evaluate(&SourceText::new(
            "recursion.js",
            "function recurse() { return recurse(); } recurse();",
        ))
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::RangeError);
    assert_eq!(error.stack().len(), 8);
}

#[test]
fn syntax_errors_have_precise_source_locations() {
    let engine = Engine::default();
    let error = engine
        .compile(&SourceText::new("broken.js", "let value = ;"))
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::SyntaxError);
    let location = error.location().unwrap();
    assert_eq!(location.source_name, "broken.js");
    assert_eq!(location.span.start.line, 1);
    assert_eq!(location.span.start.column, 13);
}
