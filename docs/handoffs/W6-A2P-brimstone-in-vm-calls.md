# W6-A2P disabled Brimstone in-VM calls and constructors

- Task: Extend the disabled Brimstone baseline proof across ordinary bytecode `Call` and
  `Construct` execution, including recursion, actual/formal argument differences, exact receiver
  and `new.target` semantics, cache pressure, moving GC, and committed no-replay failure behavior.
- Owner: Agent 2, JavaScript and WebAssembly runtime.
- Status: Implemented and accepted by the locked external validation matrix below after hostile
  source review and correction. `baseline_jit` remains off by default and
  `PRODUCT_DISPATCH_ENABLED` remains a literal compile-time `false` constant.
- Repository baseline: Wild Buzzard `4f2c83ade33ee26eb6d0f6a8afabd9a4849c1fc6` and the canonical
  Brimstone source pin `bfb720f0afb8b2b28b27c22ee7091deb7d16b082`.
- Wild Buzzard paths changed:
  `js/brimstone/src/js/runtime/bytecode/{function,vm}.rs`,
  `js/brimstone/src/js/runtime/context.rs`,
  `js/brimstone/src/js/runtime/jit/{code_cache,continuation,dispatch}.rs`, and this handoff.
  No manifest, lockfile, product-admission constant, DOM surface, or browser host changed for this
  task.

This gate does not admit DOM or untrusted-page execution and is not a browser baseline tier or a
claim of Firefox parity.

## Observable call and construction behavior

Ordinary in-VM bytecode calls and constructions now reach the same test-policy dispatcher used by
the Rust-to-bytecode entry. The hook sits after ordinary receiver/construction-receiver formation,
where Brimstone already owns the JavaScript semantics. A successful native or continued return or
throw is committed and consumed directly; interruption, allocation failure, poison, malformed
state, and panic after entry are terminal. No committed bytecode effect is replayed by falling back
to the interpreter.

The captured layout still begins with locals and receiver, but formal arguments and actual
arguments are deliberately distinct:

```text
native inputs:     [local 0 ... local N-1, receiver, formal 0 ... formal F-1]
continuation data: [actual 0 ... actual A-1]
construct data:    exact new.target in its dedicated local
```

Missing formals are padded with `undefined`. Extra actual arguments remain rooted, traced, and
visible to argument/rest semantics rather than being discarded to satisfy the native slot shape.
Constructor `new.target` is recorded independently of both actual and formal arity, so missing
formals cannot shift or overwrite it.

The private real-VM continuation admits `Call`, `CallWithReceiver`, `CallVarargs`, `Construct`, and
`ConstructVarargs`, plus VM-side `RestParameter` and `NewUnmappedArguments`. Generated native code
still does not lower a call instruction: it side-exits at that exact instruction and the admitted
ordinary VM executes it. The nested callee can independently enter its own cached tier through the
ordinary hook. Dynamically invalid operands retain the ordinary JavaScript error path.

Regression evidence covers:

- missing and extra arguments, formal padding, extra values observed through rest/arguments, and
  exact receiver identity;
- strict receivers, sloppy nullish-to-global substitution, sloppy primitive boxing, and object
  receiver preservation;
- same-function recursion, mutual recursion, nested cold/rejected calls, and cache pressure;
- base-constructor primitive return selecting the created receiver, base object override, derived
  object return, and the derived primitive-return error;
- exact `new.target` through two missing formals;
- nested return, caught and uncaught throw, interruption, allocation failure, injected panic,
  moving collection, and subsequent context recovery;
- one-shot helper/call effects across forced moving GC, allocation failure, and panic, proving
  committed execution is not replayed.

A function with its own exception-handler table remains outside native binding admission. An
ordinary interpreted caller can nevertheless catch an exact throw returned by a tiered callee,
which proves that the new terminal/committed boundary preserves the existing caller handler path.

## Hostile-review correction: resume ownership

The initial integration could let a cold nested callee run under the outer continuation's verified
program and deterministic budget. That was structurally invalid even when simple tests happened to
return the expected value. The corrected source separates the active dispatcher from the program
and budget that belong only to the currently resumed artifact.

A nested cold, rejected, or pinned-capacity call now receives its own ordinary Rust-entry frame and
ordinary dispatch loop while safely reborrowing the same dispatcher through a higher-ranked
`Context::with_explicit_jit_dispatch` boundary. It can never validate or execute against its
caller's prepared program. A dedicated regression uses a cold callee whose bytecode the outer
program cannot validate; it returns normally rather than aborting or consuming the outer budget.

`resume_from_jit_side_exit` now owns a `RustCallFrameGuard`. On return, throw, allocation failure,
interruption, or unwind, it removes only well-formed descendant frames and then proves exact parent
stack pointer, frame pointer, and frame-depth restoration. This replaces the stale assumption that
the resumed frame could not call another JavaScript frame.

## Artifact lifetime and cache coherence

Loaded executable artifacts are `Rc` owned. Each synchronous activation holds a private
`LoadedPrototypePin`, so the exact RX mapping cannot be removed while native code or its
continuation is active. LRU retirement skips pinned mappings and reports bounded pinned capacity
instead of evicting active code. Explicit removal of a pinned entry fails closed.

Metadata retirement is coherent with that rule. Rejection of an active mapping leaves a deferred
retirement tombstone rather than fabricating successful removal; the last pin permits the mapping
to be retired at a safe boundary. Same-artifact recursion, mutual recursion, a full cache where
every candidate is active, generation/root checks, and shutdown/recovery tests exercise this
ownership model.

The dispatcher can be higher-ranked reborrowed for ordinary nested JavaScript calls without
placing a raw pointer, thread-local alias, or moving-GC handle outside the owning context. While
the pointer-free dispatcher state is temporarily out of `ContextCell`, a Rust host callback that
re-enters JavaScript deliberately interprets fail closed. Forced moving GC before the callback,
exact return/throw observation counts, panic cleanup, handle/frame restoration, and context reuse
are tested. Callback tier reentry is explicitly deferred; the fallback is part of this gate's
bounded contract, not an accidental product promise.

## Firefox reference evidence

Reference checkout: detached Firefox ESR153
`c19b7e89270787889495688244ec6ee8e79288a1`.

- `firefox/js/src/vm/Stack-inl.h:235-273`, `Stack.h:67-70,442-467`, and
  `jit/JitFrames.h:83-125` distinguish actual argument count from formal padding and preserve
  frame argument identity. `jit/VMFunctions.cpp:480-590` is relevant to rectifier/call behavior.
- Applicable regression references include `js/src/jit-test/tests/basic/testDifferingArgc.js`,
  `debug/Frame-arguments-07.js`, `basic/newTargetRectifier.js`, and `bug1993404.js`. Historical
  commit `04830ff9bcb739527f06a866510daddf3e5046fe` fixed the exact `new.target`/missing-formal class
  of defect guarded here.
- `firefox/js/src/vm/Interpreter.cpp:103-174,647-699` records strict/sloppy receiver behavior;
  `Interpreter.cpp:310-329,701-805,4368-4374` records `new.target` and construction behavior.
- `firefox/js/src/vm/Stack.cpp:192-245` and the constructor-return path in
  `jit/BaselineCodeGen.cpp:2635-2675` are references for base/derived constructor result
  selection. Applicable Test262 `new.target`, ordinary `new`, and `Reflect.construct` cases remain
  future differential inputs rather than copied fixtures.

Wild Buzzard uses its own bounded Rust ownership and continuation design. These references support
observable semantics; they are not a claim that Brimstone reproduces SpiderMonkey's architecture.

## Validation evidence

All Cargo build and test output was directed outside the repository under
`/home/user/Documents/wildbuzzardbuilds/w6-a2p/`, with incremental compilation disabled and
`TMPDIR` beneath that tree. Dependency resolution was offline. Stable validation used
`rustc 1.95.0 (59807616e 2026-04-14)` and `cargo 1.95.0 (f2d3ce0bd 2026-03-21)`.
Sanitizer validation used `nightly-2026-06-02`,
`rustc 1.98.0-nightly (6bdf43094 2026-06-01)`, and an instrumented standard library.

| Locked corrected-source gate | Result |
| --- | --- |
| default full workspace tests | 35 passed: 24 core plus 11 integration/snapshot; 0 failed |
| default `brimstone_core` library tests | 24 passed; 0 failed |
| `baseline_jit` `brimstone_core` library tests | 120 passed; 0 failed |
| unfiltered `baseline_jit,handle_stats` library tests | 124 passed; 0 failed |
| unfiltered `baseline_jit,alloc_error,gc_stress_test,handle_stats` library tests | 127 passed; 0 failed |
| strict all-target `baseline_jit` Clippy | passed with `-D warnings` |
| release `brimstone_core` check with `baseline_jit` | passed |
| warning-denied `brimstone_core` rustdoc with `baseline_jit` | passed with only the four inherited warning classes explicitly allowed below |
| nightly ASan/LSan JIT tests with forced moving GC, allocation errors, handle statistics, and `no_jemalloc` | 102 passed; 0 failed; 25 filtered; no address or leak diagnostic |
| rustfmt and `git diff --check` | passed |

Representative exact commands, run from `js/brimstone/`, were:

```sh
CARGO_NET_OFFLINE=true CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w6-a2p/baseline \
  CARGO_INCREMENTAL=0 TMPDIR=/home/user/Documents/wildbuzzardbuilds/w6-a2p/tmp \
  cargo +1.95.0 test --locked -p brimstone_core --features baseline_jit --lib

CARGO_NET_OFFLINE=true CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w6-a2p/handle-stats \
  CARGO_INCREMENTAL=0 TMPDIR=/home/user/Documents/wildbuzzardbuilds/w6-a2p/tmp \
  cargo +1.95.0 test --locked -p brimstone_core --features baseline_jit,handle_stats --lib

CARGO_NET_OFFLINE=true CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w6-a2p/stress \
  CARGO_INCREMENTAL=0 TMPDIR=/home/user/Documents/wildbuzzardbuilds/w6-a2p/tmp \
  cargo +1.95.0 test --locked -p brimstone_core \
  --features baseline_jit,alloc_error,gc_stress_test,handle_stats --lib

CARGO_NET_OFFLINE=true CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w6-a2p/clippy \
  CARGO_INCREMENTAL=0 TMPDIR=/home/user/Documents/wildbuzzardbuilds/w6-a2p/tmp \
  cargo +1.95.0 clippy --locked -p brimstone_core --features baseline_jit \
  --all-targets -- -D warnings

CARGO_NET_OFFLINE=true CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w6-a2p/release-check \
  CARGO_INCREMENTAL=0 TMPDIR=/home/user/Documents/wildbuzzardbuilds/w6-a2p/tmp \
  cargo +1.95.0 check --locked --release -p brimstone_core --features baseline_jit

CARGO_NET_OFFLINE=true CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w6-a2p/rustdoc-baseline \
  CARGO_INCREMENTAL=0 TMPDIR=/home/user/Documents/wildbuzzardbuilds/w6-a2p/tmp \
  RUSTDOCFLAGS='-D warnings -A rustdoc::broken_intra_doc_links \
  -A rustdoc::private_intra_doc_links -A rustdoc::bare_urls \
  -A rustdoc::invalid_html_tags' cargo +1.95.0 doc --locked -p brimstone_core \
  --no-deps --features baseline_jit

CARGO_NET_OFFLINE=true ASAN_OPTIONS='detect_leaks=1:halt_on_error=1' \
  CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w6-a2p/asan \
  CARGO_INCREMENTAL=0 TMPDIR=/home/user/Documents/wildbuzzardbuilds/w6-a2p/tmp \
  RUSTFLAGS='-Zsanitizer=address -Cdebuginfo=1' RUSTDOCFLAGS='-Zsanitizer=address' \
  cargo +nightly-2026-06-02 test -Zbuild-std --locked -p brimstone_core --lib \
  --target x86_64-unknown-linux-gnu --no-default-features \
  --features baseline_jit,gc_stress_test,alloc_error,handle_stats,no_jemalloc runtime::jit::
```

An initial fully strict rustdoc run exposed only inherited Brimstone-wide broken/private links,
bare URLs, and invalid HTML tags in untouched imported source. The accepted rustdoc gate denies all
other warnings and explicitly allows only those four inherited classes. Stable rustfmt reported
that the repository's nightly-only configuration keys were ignored and returned success.

## Workspace and protected-path audit

The shared workspace was concurrently dirty in other owner lanes. The W6-A2P write set is exactly
the six Brimstone source files and this handoff listed above. In particular, the protected existing
changes under `js/README.md`, `js/src/**`, and `js/tests/**` were not read as implementation inputs,
formatted, staged, or modified by this task. Root `Cargo.toml`, `Cargo.lock`, toolchain, CI,
program-status, and product files were not changed by this task. No repository-local `target/`,
sanitizer output, debug symbols, or packaging artifact was created.

## Boundaries and next work

`baseline_jit` remains nondefault and `PRODUCT_DISPATCH_ENABLED` remains compile-time `false`.
This gate has no DOM entry or untrusted-page permission. It does not add general native call
lowering, properties/caches, native handled-exception metadata, callback tier reentry, noninitial
realm product admission, OSR, deoptimization, invalidation, debugger/unwind metadata, complete
native stack maps, an optimizing tier, browser watchdog/interrupt integration, browser resource
policy, Test262/WPT completion, or a performance gate.

The fixed root/code bounds, interpreted callback reentry, call-threshold admission, and private
side-exit continuation are contained proof policies, not a browser-scale runtime. Remaining raw
context/handle lifetime migration, complete moving-GC/rooting audit, broader fuzz/Miri/sanitizer
coverage, generational/incremental collection, and product-tier architecture remain open.
