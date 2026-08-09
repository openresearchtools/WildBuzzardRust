use std::rc::Rc;

use brimstone_core::{
    common::{
        error::{FormatOptions, print_error_message_and_exit},
        options::{Args, Options},
        terminal::stderr_should_use_colors,
    },
    parser::source::Source,
    runtime::{BsResult, ContextBuilder, OwnedContext, alloc_error::AllocResult},
};
use clap::Parser;

fn parse_options(args: &Args) -> Rc<Options> {
    match Options::new_from_args(args) {
        Ok(options) => Rc::new(options),
        Err(err) => print_error_message_and_exit(&err.to_string()),
    }
}

fn create_context(args: &Args) -> AllocResult<OwnedContext> {
    let options = parse_options(args);
    let mut cx = ContextBuilder::new().set_options(options).build()?;

    cx.install_optional_globals()?;

    #[cfg(feature = "gc_stress_test")]
    {
        cx.enable_gc_stress_test();
    }

    Ok(cx)
}

fn evaluate(cx: &mut OwnedContext, args: &Args) -> BsResult<()> {
    for file in &args.files {
        let source = Rc::new(Source::new_from_file(file)?);

        if args.module {
            cx.evaluate_module(source)?;
        } else {
            cx.evaluate_script(source)?;
        }
    }

    Ok(())
}

fn unwrap_error_or_exit<T>(cx: &OwnedContext, result: BsResult<T>) -> T {
    match result {
        Ok(value) => value,
        Err(err) => {
            let supports_color = stderr_should_use_colors(cx.options());
            let format_options = FormatOptions::new(supports_color);

            // Error formatting is still an upstream raw-context API. The token is used only for
            // this call and cannot outlive `cx`.
            let raw = unsafe { cx.raw_context_unchecked() };
            print_error_message_and_exit(&err.format(raw, &format_options));
        }
    }
}

/// Wrapper to pretty print errors
fn main() {
    // Global initialization
    brimstone_serialized_heap::init();

    let args = Args::parse();
    let mut cx = create_context(&args).expect("Failed to create initial Context");
    let result = evaluate(&mut cx, &args);

    #[cfg(feature = "handle_stats")]
    println!("{:?}", cx.handle_stats());

    unwrap_error_or_exit(&cx, result);
}
