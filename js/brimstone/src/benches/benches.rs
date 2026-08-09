use std::{
    rc::Rc,
    time::{Duration, Instant},
};

use bitflags::bitflags;
use brimstone_core::{
    common::{
        constants::MEGABYTE_BYTES, options::OptionsBuilder,
        serialized_heap::get_default_serialized_heap,
    },
    parser::{
        ParseContext,
        analyze::{AnalyzedProgramResult, analyze},
        parse_module, parse_script,
        parser::ParseProgramResult,
        source::Source,
    },
    runtime::{
        ContextBuilder, EvalResult, OwnedContext,
        bytecode::generator::{BytecodeProgramGenerator, BytecodeScript},
    },
};
use criterion::{Criterion, criterion_group, criterion_main};

/// A test with a setup, routine, and cleanup phase. Only the routine phase is measured.
pub fn isolated_test<I, O, S, R, C>(
    c: &mut Criterion,
    name: &str,
    mut setup: S,
    mut routine: R,
    mut cleanup: C,
) where
    S: FnMut() -> I,
    R: FnMut(I) -> O,
    C: FnMut(O),
{
    c.bench_function(name, |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;

            for _ in 0..iters {
                // Perform setup for the test
                let input = setup();

                // Only measure the time taken by the routine
                let start = Instant::now();
                let output = routine(input);
                total += start.elapsed();

                cleanup(output);
            }

            total
        });
    });
}

bitflags! {
    #[derive(Clone, Copy)]
    struct TestFlags: u8 {
        const ANNEX_B = 1 << 0;
        const MODULE = 1 << 1;
    }
}

fn setup_step(file: &str, flags: TestFlags) -> (OwnedContext, ParseContext) {
    // Use a 10 MB heap size
    let options = OptionsBuilder::new()
        .annex_b(flags.contains(TestFlags::ANNEX_B))
        .min_heap_size(10 * MEGABYTE_BYTES)
        .build()
        .unwrap();
    let cx = ContextBuilder::new()
        .set_options(Rc::new(options))
        .build()
        .unwrap();
    let source = Rc::new(Source::new_from_file(&format!("benches/{file}")).unwrap());
    let pcx = ParseContext::new(source);
    (cx, pcx)
}

fn parse_step<'a>(
    (cx, pcx): (OwnedContext, ParseContext),
    flags: TestFlags,
) -> (OwnedContext, ParseContext, ParseProgramResult<'a>) {
    let parse_result = if flags.contains(TestFlags::MODULE) {
        parse_module(&pcx, cx.options().clone()).unwrap()
    } else {
        parse_script(&pcx, cx.options().clone()).unwrap()
    };

    // Break the lifetime but keep parse context around for lifetime of AST
    let parse_result = unsafe {
        std::mem::transmute::<ParseProgramResult<'_>, ParseProgramResult<'a>>(parse_result)
    };

    (cx, pcx, parse_result)
}

fn analyze_step<'a>(
    (cx, pcx, parse_result): (OwnedContext, ParseContext, ParseProgramResult<'a>),
) -> (OwnedContext, ParseContext, AnalyzedProgramResult<'a>) {
    let analyzed_result = analyze(parse_result).unwrap();
    (cx, pcx, analyzed_result)
}

fn generate_step(
    (cx, _pcx, analyzed_result): (OwnedContext, ParseContext, AnalyzedProgramResult),
) -> (OwnedContext, BytecodeScript) {
    // Fix the lifetime
    let analyzed_result = unsafe {
        std::mem::transmute::<AnalyzedProgramResult<'_>, AnalyzedProgramResult<'_>>(analyzed_result)
    };

    // The generated program is consumed before `cx` is dropped by this benchmark pipeline.
    let raw = unsafe { cx.raw_context_unchecked() };
    let bytecode_program = BytecodeProgramGenerator::generate_from_parse_script_result(
        raw,
        &analyzed_result,
        raw.initial_realm(),
    )
    .unwrap();
    (cx, bytecode_program)
}

fn execute_step(
    (mut cx, bytecode_script): (OwnedContext, BytecodeScript),
) -> (OwnedContext, EvalResult<()>) {
    let result = unsafe { cx.run_script_unchecked(bytecode_script) };
    (cx, result)
}

fn cleanup2_step<T>((cx, x1): (OwnedContext, T)) {
    std::mem::drop(x1);
    std::mem::drop(cx);
}

fn cleanup3_step<T, U>((cx, x1, x2): (OwnedContext, T, U)) {
    std::mem::drop(x2);
    std::mem::drop(x1);
    std::mem::drop(cx);
}

/// Benchmark just the parsing phase.
fn bench_program_parser(c: &mut Criterion, file: &str, flags: TestFlags) {
    isolated_test(
        c,
        &format!("{file} > parse"),
        || setup_step(file, flags),
        |input| parse_step(input, flags),
        cleanup3_step,
    );
}

/// Benchmark all phases of program execution.
/// - Parse
/// - Analyze
/// - Bytecode generation
/// - Bytecode execution
fn bench_program_all_steps(c: &mut Criterion, file: &str, flags: TestFlags) {
    bench_program_parser(c, file, flags);

    // Isolate analysis phase
    isolated_test(
        c,
        &format!("{file} > analyze"),
        || {
            let setup_result = setup_step(file, flags);
            parse_step(setup_result, flags)
        },
        |parse_result| analyze_step(parse_result),
        cleanup3_step,
    );

    // Isolate bytecode generation phase
    isolated_test(
        c,
        &format!("{file} > generate"),
        || {
            let setup_result = setup_step(file, flags);
            let parse_result = parse_step(setup_result, flags);
            analyze_step(parse_result)
        },
        generate_step,
        cleanup2_step,
    );

    // Isolate bytecode generation phase
    isolated_test(
        c,
        &format!("{file} > execute"),
        || {
            let setup_result = setup_step(file, flags);
            let parse_result = parse_step(setup_result, flags);
            let analyzed_result = analyze_step(parse_result);
            generate_step(analyzed_result)
        },
        execute_step,
        cleanup2_step,
    );
}

fn init() {
    brimstone_serialized_heap::init();
}

/// Benchmark context creation.
fn context_benches(c: &mut Criterion) {
    init();

    isolated_test(
        c,
        "context creation",
        || {},
        |_| {
            let options = OptionsBuilder::new().serialized_heap(None).build().unwrap();
            let cx = ContextBuilder::new()
                .set_options(Rc::new(options))
                .build()
                .unwrap();
            (cx, ())
        },
        cleanup2_step,
    );

    isolated_test(
        c,
        "context deserialization",
        || {},
        |_| {
            let serialized_heap = get_default_serialized_heap().unwrap();
            let options = OptionsBuilder::new()
                .serialized_heap(Some(serialized_heap))
                .build()
                .unwrap();
            let cx = ContextBuilder::new()
                .set_options(Rc::new(options))
                .build()
                .unwrap();
            (cx, ())
        },
        cleanup2_step,
    );
}

pub fn program_benches(c: &mut Criterion) {
    init();

    bench_program_all_steps(c, "empty.js", TestFlags::empty());
    bench_program_parser(c, "fixtures/acorn.js", TestFlags::empty());
    bench_program_parser(c, "fixtures/react.js", TestFlags::empty());
    bench_program_parser(c, "fixtures/typescript.js", TestFlags::empty());
}

criterion_group!(context, context_benches);
criterion_group!(program, program_benches);
criterion_main!(context, program);
