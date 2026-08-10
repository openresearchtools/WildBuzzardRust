//! Focused declaration and classic-loop conformance tests.

use wild_buzzard_js::{
    Context, Engine, ErrorKind, JsString, RealmOptions, SourceText, ValueSnapshot,
};

fn context() -> Context {
    Engine::default()
        .create_realm(RealmOptions::default())
        .context()
}

fn evaluate(context: &mut Context, source: &str) -> ValueSnapshot {
    let value = context
        .evaluate(&SourceText::new("control-flow.js", source))
        .unwrap();
    context.snapshot(&value).unwrap()
}

fn string(value: &str) -> ValueSnapshot {
    ValueSnapshot::String(JsString::from_utf8(value).unwrap())
}

fn assert_syntax_error(source: &str) {
    let error = Engine::default()
        .compile(&SourceText::new("early-error.js", source))
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::SyntaxError, "source: {source}");
    assert!(error.location().is_some(), "source: {source}");
}

#[test]
fn var_is_hoisted_without_reset_and_crosses_blocks() {
    let mut context = context();
    assert_eq!(
        evaluate(
            &mut context,
            r#"
                function before() {
                    if (false) { var unseen = 1; }
                    return typeof unseen;
                }
                function acrossBlock() {
                    { var answer = 42; }
                    { var answer; }
                    return answer;
                }
                before() + ":" + acrossBlock();
            "#,
        ),
        string("undefined:42")
    );
}

#[test]
fn body_functions_parameters_and_vars_share_the_variable_scope() {
    let mut context = context();
    assert_eq!(
        evaluate(
            &mut context,
            r"
                function choose(value) {
                    var value;
                    function selected() { return 1; }
                    var selected;
                    function selected() { return value; }
                    return selected();
                }
                choose(7);
            ",
        ),
        ValueSnapshot::Number(7.0)
    );
    assert_eq!(
        evaluate(
            &mut context,
            "function forward() { return later(); function later() { return 8; } } forward();",
        ),
        ValueSnapshot::Number(8.0)
    );
}

#[test]
fn persistent_global_var_redeclaration_preserves_values_and_preflights_conflicts() {
    let mut primary = context();
    assert_eq!(
        evaluate(
            &mut primary,
            "globalFunction(); function globalFunction() { return 6; }",
        ),
        ValueSnapshot::Number(6.0)
    );
    assert_eq!(
        evaluate(
            &mut primary,
            "let beforeGlobal = typeof laterGlobal; var laterGlobal = 3; beforeGlobal + ':' + laterGlobal;",
        ),
        string("undefined:3")
    );
    assert_eq!(
        evaluate(&mut primary, "var saved = 40; saved;"),
        ValueSnapshot::Number(40.0)
    );
    assert_eq!(
        evaluate(&mut primary, "var saved; saved = saved + 2; saved;"),
        ValueSnapshot::Number(42.0)
    );
    assert_eq!(
        evaluate(
            &mut primary,
            "function saved() { return 9; } var saved; saved();",
        ),
        ValueSnapshot::Number(9.0)
    );

    let error = primary
        .evaluate(&SourceText::new(
            "global-conflict.js",
            "var shouldNotExist = 1; let saved = 0;",
        ))
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::SyntaxError);
    assert_eq!(
        evaluate(&mut primary, "typeof shouldNotExist;"),
        string("undefined")
    );

    let mut reverse = context();
    evaluate(&mut reverse, "let lexical = 1;");
    let error = reverse
        .evaluate(&SourceText::new("reverse-conflict.js", "var lexical;"))
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::SyntaxError);
}

#[test]
fn parser_rejects_var_lexical_conflicts_at_each_scope_contour() {
    for source in [
        "let name; var name;",
        "var name; const name = 1;",
        "{ let name; { var name; } }",
        "{ function name() {} var name; }",
        "function f(parameter) { let parameter; }",
        "for (let name = 0; false; ) { var name; }",
        "try {} catch (name) { var name; }",
    ] {
        assert_syntax_error(source);
    }

    let mut context = context();
    assert_eq!(
        evaluate(&mut context, "{ { let name = 1; } var name = 2; name; }"),
        ValueSnapshot::Number(2.0)
    );
}

#[test]
fn parser_rejects_invalid_classic_for_heads() {
    for source in [
        "for (const value; false; ) {}",
        "for (var key in object) {}",
        "for (key of values) {}",
        "for (let value = 0; value < 1) {}",
        "for (let value = 0, value = 1; ; ) {}",
    ] {
        assert_syntax_error(source);
    }
}

#[test]
fn classic_for_supports_empty_expression_var_let_and_const_heads() {
    let mut context = context();
    assert_eq!(
        evaluate(
            &mut context,
            r"
                let empty = 0;
                for (;;) { empty = 1; break; }
                let expression = 0;
                for (expression = 1; expression < 3; expression = expression + 1) {}
                for (var variable = 0; variable < 2; variable = variable + 1) {}
                let lexical = 0;
                for (let index = 0; index < 2; index = index + 1) {
                    lexical = lexical + index;
                }
                let constant = 0;
                for (const once = 5; once === 5; ) { constant = once; break; }
                empty * 10000 + expression * 1000 + variable * 100 + lexical * 10 + constant;
            ",
        ),
        ValueSnapshot::Number(13_215.0)
    );
}

#[test]
fn classic_for_orders_initializer_test_body_and_update() {
    let mut context = context();
    assert_eq!(
        evaluate(
            &mut context,
            r#"
                let trace = "";
                let passes = 0;
                for (
                    trace = trace + "I";
                    (trace = trace + "T") && passes < 2;
                    trace = trace + "U"
                ) {
                    trace = trace + "B";
                    passes = passes + 1;
                }
                trace;
            "#,
        ),
        string("ITBUTBUT")
    );
}

#[test]
fn for_continue_runs_update_and_break_skips_it() {
    let mut context = context();
    assert_eq!(
        evaluate(
            &mut context,
            r"
                let updates = 0;
                let visits = 0;
                for (; updates < 5; updates = updates + 1) {
                    visits = visits + 1;
                    if (updates === 0) { continue; }
                    if (updates === 2) { break; }
                }
                updates * 10 + visits;
            ",
        ),
        ValueSnapshot::Number(23.0)
    );
}

#[test]
fn for_let_closures_capture_distinct_iteration_bindings() {
    let mut context = context();
    assert_eq!(
        evaluate(
            &mut context,
            r"
                let closures = [];
                for (let index = 0; index < 3; index = index + 1) {
                    closures.push(function () { return index; });
                }
                closures[0]() * 100 + closures[1]() * 10 + closures[2]();
            ",
        ),
        ValueSnapshot::Number(12.0)
    );
}

#[test]
fn for_let_freshens_before_the_first_test_and_hides_the_head_afterward() {
    let mut context = context();
    assert_eq!(
        evaluate(
            &mut context,
            r#"
                let value = "outside";
                let initializerClosure;
                let bodyClosure;
                let run = true;
                for (
                    let value = "initial", capture = initializerClosure = function () { return value; };
                    run;
                ) {
                    value = "iteration";
                    bodyClosure = function () { return value; };
                    run = false;
                }
                value + ":" + initializerClosure() + ":" + bodyClosure();
            "#,
        ),
        string("outside:initial:iteration")
    );
}

#[test]
fn for_var_closures_share_the_hoisted_binding() {
    let mut context = context();
    assert_eq!(
        evaluate(
            &mut context,
            r"
                let closures = [];
                for (var index = 0; index < 3; index = index + 1) {
                    closures.push(function () { return index; });
                }
                closures[0]() * 100 + closures[1]() * 10 + closures[2]();
            ",
        ),
        ValueSnapshot::Number(333.0)
    );
}

#[test]
fn loop_completion_values_preserve_undefined_body_and_abrupt_values() {
    let mut context = context();
    assert_eq!(
        evaluate(&mut context, "9; for (; false; ) {}"),
        ValueSnapshot::Undefined
    );
    assert_eq!(
        evaluate(
            &mut context,
            "for (var first = 0; first < 3; first = first + 1) { first; }",
        ),
        ValueSnapshot::Number(2.0)
    );
    assert_eq!(
        evaluate(
            &mut context,
            "for (var second = 0; second < 3; second = second + 1) { if (second === 1) { 7; break; } }",
        ),
        ValueSnapshot::Number(7.0)
    );
    assert_eq!(
        evaluate(
            &mut context,
            "for (var third = 0; third < 2; third = third + 1) { if (third === 0) { 4; continue; } break; }",
        ),
        ValueSnapshot::Number(4.0)
    );
}

#[test]
fn do_while_runs_body_first_and_honors_continue_break_and_completion() {
    let mut context = context();
    assert_eq!(
        evaluate(
            &mut context,
            r"
                let bodies = 0;
                let tests = 0;
                do { bodies = bodies + 1; continue; }
                while ((tests = tests + 1) < 2)
                bodies * 10 + tests;
            ",
        ),
        ValueSnapshot::Number(22.0)
    );
    assert_eq!(
        evaluate(&mut context, "9; do {} while (false)"),
        ValueSnapshot::Undefined
    );
    assert_eq!(
        evaluate(&mut context, "do { 4; break; } while (true)"),
        ValueSnapshot::Number(4.0)
    );
}

#[test]
fn typeof_only_suppresses_direct_unresolvable_identifier_errors() {
    let mut context = context();
    assert_eq!(
        evaluate(
            &mut context,
            "typeof missing === 'undefined' && typeof (alsoMissing) === 'undefined';",
        ),
        ValueSnapshot::Boolean(true)
    );

    let tdz = context
        .evaluate(&SourceText::new(
            "typeof-tdz.js",
            "typeof value; let value;",
        ))
        .unwrap_err();
    assert_eq!(tdz.kind(), ErrorKind::ReferenceError);
    assert!(tdz.message().contains("before initialization"));
    let location = tdz.location().unwrap();
    assert_eq!(location.source_name, "typeof-tdz.js");
    assert_eq!(location.span.start.line, 1);
    assert_eq!(location.span.start.column, 8);

    let member = context
        .evaluate(&SourceText::new(
            "typeof-member.js",
            "typeof missingObject.property;",
        ))
        .unwrap_err();
    assert_eq!(member.kind(), ErrorKind::ReferenceError);

    assert_eq!(
        evaluate(
            &mut context,
            r#"
                let caught = false;
                try { typeof (function () { throw "boom"; })(); }
                catch (error) { caught = error === "boom"; }
                caught;
            "#,
        ),
        ValueSnapshot::Boolean(true)
    );
}
