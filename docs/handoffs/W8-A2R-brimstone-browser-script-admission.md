# W8-A2R: bounded Brimstone browser classic-script admission

## Status and decision

W8-A2R is implementation-complete as a contained browser-embedding seam for trusted integration
and regression work. Hostile review rejected the first candidate; all three panic/allocation
findings were corrected, and an independent corrective-delta review returned GO with no remaining
finding. It is **not approved for untrusted web content**. The implementation starts from Wild
Buzzard commit `1c7f4aa2d78fafc43172bfbb0ad50580f703afa6` and retains the adopted Brimstone upstream
baseline `bfb720f0afb8b2b28b27c22ee7091deb7d16b082`.

The exact W8-A2R write set is:

- `js/brimstone/src/js/runtime/browser_script.rs`
- `js/brimstone/src/js/runtime/bytecode/vm.rs`
- `js/brimstone/src/js/runtime/context.rs`
- `js/brimstone/src/js/runtime/gc/heap.rs`
- `js/brimstone/src/js/runtime/mod.rs`
- `js/brimstone/src/js/runtime/tasks.rs`
- `docs/handoffs/W8-A2R-brimstone-browser-script-admission.md`

The four W7-A2Q JIT files and its handoff were frozen during this wave. No root manifest,
lockfile, product-dispatch constant, transitional `js/src` implementation, transitional `js/tests`
fixture, DOM/browser path, or Firefox reference file was changed.

The contained GO is precise: an owner of one Brimstone context can synchronously parse, compile,
and evaluate a bounded UTF-8 classic script in that context's exact initial realm, receive a typed
pointer-free outcome and work report, report the primary result, and then explicitly request one
bounded promise-job checkpoint. The context is mechanically reusable after success, JavaScript
throw, typed policy termination, engine allocation failure, or forced moving collection. An
unexpected engine panic is terminal: it permanently poisons browser admission for that context.

The untrusted-content decision remains NO-GO. W8-A2R does not repair Brimstone's remaining raw
context/handle aliases or moving-collector safety debt; it is not a DOM binding, HTML script
loader, browser event loop, origin/principal boundary, or complete resource governor.

## Public embedding contract

`OwnedContext::with_browser_script_realm` creates the only safe admission authority:

```rust
owned.with_browser_script_realm(|realm| {
    let result = realm.execute_classic(request, limits, &interrupt);
    // Report the primary result first, following HTML classic-script ordering.
    let checkpoint = realm.perform_microtask_checkpoint(limits, &interrupt);
});
```

The callback is higher-ranked over a fresh realm lifetime. `BrowserScriptRealm` contains an exact
context token branded by `&mut OwnedContext`; its fields and constructor are private, and no raw
context, realm pointer, GC handle, or moving value can be returned through the safe API. This wave
intentionally admits only the caller-owned context's initial realm. A later DOM global/realm
factory must add origin and process identity instead of weakening this brand.

`ClassicScriptRequest` borrows:

- valid Rust UTF-8 source, capped at 16 MiB;
- a nonempty NUL-free filename/source identity, capped at 4 KiB;
- an optional NUL-free base metadata string, capped at 8 KiB.

The base string is validated provenance only. It is not parsed or used for URL resolution in this
wave. Source-map identity, line/column origin, referrer policy, CSP, SRI, charset decoding, muted
errors, and current-script state belong to the later loader contract.

`ClassicScriptOutcome` separates:

- successful classic completion;
- an uncaught JavaScript value;
- parse, analysis, and bytecode-generation diagnostics;
- external interruption and deadline expiration;
- source, metadata, opcode, managed-allocation, recursion, and diagnostic resource failures;
- a busy or permanently poisoned runtime and an unexpected caught engine panic.

`MicrotaskCheckpointOutcome` separately reports an empty completed checkpoint, a thrown job,
interruption, a resource limit, busy or poisoned runtime, or engine panic. Completion values never
expose a GC address. Primitives are copied, strings expose only UTF-16 code-unit length, and BigInt,
Symbol, Object, or unknown heap kinds are reduced to a kind-only summary. Diagnostics are copied
with fallible reservation, at most eight entries and 16 KiB of combined rendered text.

Like Firefox's browser classic-script path, this boundary requests no script return value. A
normally completed classic script therefore reports `Success(Undefined)`; representative scripts
assert their own observable result before completion.

## Admission, cleanup, reuse, and poison sequence

One context-owned `BrowserScriptAdmissionState` exists only during a synchronous admission. It
contains limits, an `Arc<AtomicBool>` cancellation flag, deadline, fixed opcode counters, scalar
work totals, and a typed termination reason; it contains no JavaScript pointer.

Admission proceeds in this order:

1. Validate source and metadata without installing runtime state, then reject a permanently
   poisoned browser context without entering VM, GC, or task state.
2. Reject nested admission, a non-idle VM, or a new classic script while rooted jobs from the
   previous script await a checkpoint.
3. Install the owner-thread state and an outer handle-scope guard.
4. Poll at phase boundaries, parse, analyze, generate bytecode, enter the exact initial realm, and
   evaluate.
5. Count and poll every actual interpreter opcode before effects. Wide encodings count the
   underlying operation rather than a synthetic prefix. Comparison/branch fusion is disabled only
   during this admission so it cannot skip a poll.
6. Return a pointer-free primary outcome without draining jobs.
7. Prove the VM stack pointer, frame pointer, and frame count are idle, remove admission state, and
   snapshot the rooted pending-job count.

External cancellation and policy limits use a private, exact `panic_any` payload so they unwind
through ordinary VM frame and handle RAII without becoming JavaScript-catchable exceptions. Those
known terminations prove idle VM state, clear queued jobs, and permit reuse. A JavaScript throw is a
normal primary outcome and preserves queued jobs for the explicit checkpoint.

An unexpected Rust panic is different. The first candidate incorrectly treated a safe pre-effect
test panic as proof that arbitrary parser, compiler, builtin, or moving-GC panics permit reuse.
Hostile review rejected that claim. The corrected path sets a permanent context-owned browser
poison, consumes only pointer-free admission counters, and returns `EnginePanic` without inspecting
VM, GC, or task state. Every later execute/checkpoint returns `RuntimePoisoned` with zero work and
does not enter those subsystems. Product code must retire the context and terminate its future
content process. This bounded poison does not make Brimstone's broader `OwnedContext` APIs or drop
after arbitrary internal corruption sound; that remains an explicit untrusted-content NO-GO.

## Resource and interruption accounting

`ClassicScriptLimits::new` rejects zero scalar limits and caps caller-selected limits at fixed
defense-in-depth maxima. Zero wall time is deliberately accepted as an immediate-deadline request.

| Resource | Default | Hard maximum | Poll/accounting point |
| --- | ---: | ---: | --- |
| interpreter opcodes | 50,000,000 | 100,000,000 | before every actual opcode effect |
| managed-heap allocation requests | 128 MiB | 256 MiB | immediately before a successful heap allocation commit |
| JavaScript frame depth | 256 | 512 | before every VM frame admission |
| jobs per explicit checkpoint | 100,000 | 1,000,000 | before dequeueing each rooted job |
| wall time | 10 s | 30 s | phase, opcode, managed allocation, frame, and job polls |

The cancellation flag is acquire/release atomic and may be set by another thread; all evaluation
remains on the context owner thread. External cancellation has priority over deadline and scalar
resource checks at each poll.

Managed allocation accounting is cumulative requested bytes for successful Brimstone heap
allocations during the admission. An allocation attempt which first collects or grows the heap is
charged only when the eventual allocation commits. It is not a physical-RSS, permanent-heap,
native-library, parser-vector, analyzer, bytecode-buffer, or host-stack bound. Parser/compiler and
some builtin Rust loops are polled only at phase boundaries, so they can delay deadline or external
interruption. Those omissions must be closed before untrusted input.

## Explicit microtask checkpoint

`execute_classic` deliberately leaves promise jobs in Brimstone's existing GC-traced task queue.
While jobs remain, another classic-script admission returns `RuntimeBusy`; the embedding must first
handle the primary result and invoke `perform_microtask_checkpoint` with its own job, opcode,
allocation, recursion, deadline, and cancellation limits.

The checkpoint counts before dequeue, traces queued task values through the existing root visitor,
and uses a fresh handle scope for each job. A policy termination, allocation failure, or thrown job
clears the remaining queue fail-closed. An unexpected engine panic instead poisons the context and
does not inspect the potentially compromised queue. Brimstone does not yet provide the HTML host
hook needed to report a job exception/rejection and continue draining with correct global and
incumbent-realm behavior. Clearing is therefore a contained safety policy, not claimed HTML parity.

This split is intentional. Firefox ESR enters the exact global realm, executes with `noScriptRval`,
reports the primary classic-script exception, and lets its embedding-controlled cleanup perform
the microtask checkpoint. The referenced WPT requires the observable order `body`, `global-error`,
then `microtask` for a classic script which queues a microtask and throws. Automatically draining
inside `execute_classic` would make the future HTML host unable to reproduce that order.

Root review found and corrected a handle-scope lifetime defect in the first implementation: the
`EvalError::Value` for a thrown job could be carried out of the checkpoint's outer handle scope and
only then summarized. The corrected boundary reduces every thrown value to a pointer-free
`MicrotaskCheckpointOutcome` while that outer scope is still alive, and only that summary crosses
the guard. A regression exposes Brimstone's test-only GC, creates a live finalization-registry
cleanup job which throws, forces collection, verifies the exact string-length summary, verifies
fail-closed queue clearing, and reuses the same context afterward.

Hostile review then found that a VM allocation failure after `Promise.then` left the job queued and
that `EmitError::Alloc` was mislabeled as an ordinary compiler diagnostic. Its external fixed-size
64 MiB heap reproducer proved the stale job could run in a later checkpoint. The corrected primary
path clears every queued job on `EvalError::Alloc`, reports zero pending work, and admits a fresh
script; bytecode-generator allocation failure now produces the same typed
`ResourceLimit(EngineAllocation)` outcome.

## Representative behavior and opcode evidence

The W8 fixture set executes:

- object creation plus named property definition, read, and write;
- nested functions and closures, calls, scope/global reads and writes, arithmetic, branches, and
  return;
- parse failure, scalar throw, and repeated globals in the same realm;
- a promise reaction retained across a thrown primary script, then drained by the explicit
  checkpoint in the same realm;
- a forced-GC finalization-registry cleanup job which throws, whose value is summarized before its
  outer handle scope closes;
- an external cross-thread interrupt, immediate deadline, opcode cap, allocation cap, recursion
  cap, and job cap, followed by successful reuse;
- an exact 64 MiB engine-allocation failure after scheduling a Promise job, proving the job is
  discarded and a fresh script can enter without a checkpoint;
- a direct bytecode-generator allocation-failure classification;
- an injected interpreter panic followed only by zero-work `RuntimePoisoned` rejections;
- repeated object/string work while collection is forced at managed-heap safepoints.

One captured object/property/closure fixture produced this directional interpreter histogram:

| Opcode | Count |
| --- | ---: |
| `LoadImmediate`, `LoadFromScope` | 4 each |
| `Ret`, `CheckTdz` | 3 each |
| `Call`, `Add`, `NewClosure`, `GetNamedProperty`, `StoreToScope` | 2 each |
| `Mov`, `LoadUndefined`, `LoadGlobal`, `StoreGlobal`, `StrictNotEqual`, `JumpFalse`, `NewObject`, `SetNamedProperty`, `DefineNamedProperty`, `PushFunctionScope` | 1 each |

This is a single regression fixture, not a site workload or performance conclusion. It does give
the next tier work an evidence-based starting point: scope/global loads and stores, named property
operations, ordinary calls/returns, closure creation, object construction, arithmetic, and branch
paths dominate this first normal-script slice.

Browser admission suppresses Brimstone's disabled baseline-JIT dispatcher even when tests compile
the `baseline_jit` feature. Every report therefore records `jit_enabled = false`, zero native
entries, and zero side exits. `PRODUCT_DISPATCH_ENABLED` remains the unchanged literal
compile-time `false`; W8-A2R neither enables nor broadens product JIT admission.

## Firefox ESR reference and history

The ignored, detached, full-history Firefox ESR153 checkout remained read-only at
`c19b7e89270787889495688244ec6ee8e79288a1`.

- `firefox/dom/script/ScriptLoader.cpp:2834-2888` uses the request URI as filename, records
  line/column and introduction type, sets run-once and `noScriptRval`, attaches source-map data,
  and applies muted-error policy. W8 implements only bounded source/filename/base transport; the
  rest remains a loader/DOM contract.
- `firefox/dom/script/ScriptLoader.cpp:3831-3902` owns the current `EvaluateScript` flow. It creates
  a microtask/entry scope, obtains the exact global, enters it with `JSAutoRealm`, instantiates the
  classic script, executes it, and converts the evaluation exception to browser error behavior.
- `firefox/js/src/jsfriendapi.h:155-189` explicitly says browser embeddings normally own promise
  scheduling and must trigger job processing at the correct time after script evaluation.
- `firefox/testing/web-platform/tests/html/semantics/scripting-1/the-script-element/microtasks/evaluation-order-1.html`
  records the required throw/error-report/microtask order tested by W8's split boundary.
- Blame identifies `dccad45b5de98560088aae59c2bac5902d59d670` (2022-02-21, Bug 1742437) as
  the change which introduced the current `EvaluateScript` region. Full history also records
  `de848dc97f17cdc30703c236ee5b5c77172be2e9` (2022-11-17, Bug 1632975) for a checkpoint before
  script processing and `a0b4db6641b81d40c1fe911d0a8e1946a9a0456a` (2017-05-10, Bug 1357958)
  for SpiderMonkey's default internal Promise-job handling.

These references establish observable ordering and embedding responsibilities. W8 does not port
SpiderMonkey or Gecko's architecture.

## Validation evidence

All build, test, lint, documentation, and sanitizer output stayed outside the repository. Owner
gates used `/home/user/Documents/wildbuzzardbuilds/w8-a2r/`; corrected-source root and sanitizer
gates used the paths recorded below. Incremental compilation was disabled and dependency resolution
was offline. Stable validation used Rust/Cargo 1.95.0. Sanitizer validation used
`nightly-2026-06-02` with an instrumented standard library.

| Locked corrected-source gate | Result |
| --- | --- |
| default full Brimstone workspace tests | 44 passed: 33 core plus 11 integration/snapshot; 0 failed |
| `baseline_jit` `brimstone_core` library tests | 134 passed; 0 failed |
| unfiltered `baseline_jit,alloc_error,gc_stress_test,handle_stats` library tests | 144 passed; 0 failed |
| strict all-target `baseline_jit` Clippy | passed with `-D warnings` |
| release `brimstone_core` check with `baseline_jit` | passed |
| warning-denied `brimstone_core` rustdoc with `baseline_jit` | passed with only the four inherited warning classes explicitly allowed below |
| focused nightly ASan/LSan W8 tests with baseline feature, forced moving GC, allocation errors, handle statistics, and `no_jemalloc` | 10 passed; 0 failed; 134 filtered; no address or leak diagnostic |
| exact-path rustfmt and `git diff --check` | passed |

Representative exact commands, run from `js/brimstone/`, were:

```sh
CARGO_NET_OFFLINE=true CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w8-a2r/default \
  CARGO_INCREMENTAL=0 TMPDIR=/home/user/Documents/wildbuzzardbuilds/w8-a2r/default/tmp \
  cargo +1.95.0 test --locked --workspace

CARGO_NET_OFFLINE=true CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w8-a2r/baseline \
  CARGO_INCREMENTAL=0 TMPDIR=/home/user/Documents/wildbuzzardbuilds/w8-a2r/baseline/tmp \
  cargo +1.95.0 test --locked -p brimstone_core --features baseline_jit --lib

CARGO_NET_OFFLINE=true CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w8-a2r/stress \
  CARGO_INCREMENTAL=0 TMPDIR=/home/user/Documents/wildbuzzardbuilds/w8-a2r/stress/tmp \
  cargo +1.95.0 test --locked -p brimstone_core \
  --features baseline_jit,alloc_error,gc_stress_test,handle_stats --lib

CARGO_NET_OFFLINE=true CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w8-a2r/clippy \
  CARGO_INCREMENTAL=0 TMPDIR=/home/user/Documents/wildbuzzardbuilds/w8-a2r/clippy/tmp \
  cargo +1.95.0 clippy --locked -p brimstone_core --features baseline_jit \
  --all-targets -- -D warnings

CARGO_NET_OFFLINE=true CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w8-a2r/release \
  CARGO_INCREMENTAL=0 TMPDIR=/home/user/Documents/wildbuzzardbuilds/w8-a2r/release/tmp \
  cargo +1.95.0 check --locked --release -p brimstone_core --features baseline_jit

CARGO_NET_OFFLINE=true CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w8-a2r/rustdoc \
  CARGO_INCREMENTAL=0 TMPDIR=/home/user/Documents/wildbuzzardbuilds/w8-a2r/rustdoc/tmp \
  RUSTDOCFLAGS='-D warnings -A rustdoc::broken_intra_doc_links \
  -A rustdoc::private_intra_doc_links -A rustdoc::bare_urls \
  -A rustdoc::invalid_html_tags' cargo +1.95.0 doc --locked -p brimstone_core \
  --no-deps --features baseline_jit

CARGO_NET_OFFLINE=true ASAN_OPTIONS='detect_leaks=1:halt_on_error=1' \
  CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w8-a2r/asan \
  CARGO_INCREMENTAL=0 TMPDIR=/home/user/Documents/wildbuzzardbuilds/w8-a2r/asan/tmp \
  RUSTFLAGS='-Zsanitizer=address -Cdebuginfo=1' RUSTDOCFLAGS='-Zsanitizer=address' \
  cargo +nightly-2026-06-02 test -Zbuild-std --locked -p brimstone_core --lib \
  --target x86_64-unknown-linux-gnu --no-default-features \
  --features baseline_jit,gc_stress_test,alloc_error,handle_stats,no_jemalloc \
  runtime::browser_script::
```

The accepted rustdoc gate denies every warning except inherited Brimstone-wide broken/private
intra-doc links, bare URLs, and invalid HTML tags. Stable rustfmt reported that the repository's
nightly-only configuration keys were ignored; only the six W8 Rust files were formatted.

After the handle-scope, terminal-poison, and allocation-failure corrections, root repeated the full
default, baseline, stress, strict Clippy, release, rustdoc, rustfmt, and diff-check matrix outside
the repository under `/home/user/Documents/wildbuzzardbuilds/w8-a2r-root-review/`. The focused
corrected-source ASan/LSan run used
`/home/user/Documents/wildbuzzardbuilds/w8-a2r-root-review-asan/` and is the ten-test result recorded
above. Hostile review artifacts, including the cross-context HRTB compile rejection and original
64 MiB OOM reproducer, remain under
`/home/user/Documents/wildbuzzardbuilds/w8-a2r-independent-review/`.

The final independent corrective-delta review returned bounded GO with no critical, high, medium,
or low finding. It independently reran the focused 10-test correction set, the 44-test default
workspace, the 134-test baseline-JIT set, and the 144-test combined stress set. It verified that
permanent poison is checked before browser VM, GC, or task-queue admission; panic cleanup touches
only scalar admission state; VM allocation failure clears pending jobs; compiler allocation failure
maps to `EngineAllocation`; HRTB realm branding remains intact; and product JIT admission remains
disabled. It accepted the separately recorded 10-test ASan/LSan result rather than rerunning it.

## Explicit blockers and next contract

The following blockers prevent untrusted-page admission or any Firefox-parity claim:

1. Brimstone still exposes raw `Context`, lifetime-free heap handles, raw mutable aliases, and a
   compacting collector which upstream describes as very unsafe. W8 confines its public result but
   does not complete the internal lifetime/root migration.
2. Parser, analyzer, compiler, diagnostic internals, standard-library/builtin Rust loops, host
   vectors, permanent heap, executable mappings, and native libraries are not continuously charged
   or polled. A 16 MiB source cap and phase polls do not make them safe against hostile input.
3. Panic-based policy termination requires unwind semantics. Abort builds/hooks cannot produce a
   typed result. Unexpected panics permanently poison only this browser seam; broader
   `OwnedContext` entrypoints and destruction after arbitrary engine corruption are not covered,
   and prior JavaScript-visible effects are not rolled back. Product admission therefore requires
   process isolation and process retirement on `EnginePanic`.
4. Managed-byte totals are cumulative allocation requests, not live bytes or total process memory.
   GC growth/collection CPU and the owner context's pre-existing heap are not bounded here.
5. Only one initial realm is exposed. There is no browser global, principal/origin, site-isolated
   process binding, wrapper membrane, cross-realm policy, or DOM/WebIDL host object.
6. The base URL is opaque metadata. There is no URL resolution, module graph, dynamic import/eval
   provenance, CSP/SRI/referrer/charset policy, script loader, source-map handling, current-script
   tracking, or muted-error behavior.
7. Promise jobs are an engine queue, not the browser task/event-loop contract. There is no global
   error/rejection reporting, incumbent-realm preservation proof, `queueMicrotask`, mutation
   observer, timer, networking, rendering, or shutdown integration. A thrown job clears the queue
   instead of reporting and continuing to HTML semantics.
8. Asynchronous interruption can be delayed by long parser/compiler/builtin Rust work between
   audited polls. A real browser deadline must reach every potentially unbounded phase and host
   callback.
9. Baseline JIT remains deliberately suppressed for this API and product dispatch is impossible.
   Full Test262, WPT, differential, fuzz, Miri where applicable, broad sanitizer, debugger,
   profiling, rejection tracking, and performance gates remain open.

The next JavaScript integration slice should therefore be **rooted generated DOM bindings plus a
browser-owned task/event-loop boundary**. It should create and identify an exact page global/realm,
root host objects without leaking raw pointers, translate DOM calls and exceptions through typed
generated bindings, carry principal/origin and loader metadata, report script and Promise errors in
HTML order, and schedule bounded microtask checkpoints at browser task boundaries. It must retain
W8's owner-thread brand, pointer-free outward results, exact cleanup, and disabled product/JIT
admission. Adding more isolated limit-policy breadth before that integration would not advance a
normal website.

## Workspace integrity

The final W8-A2R source paths have these exact SHA-256 values:

```text
96be3c916daba89ae1b7391a798f167e3caff3ab35da9af3c90af20deb199919  js/brimstone/src/js/runtime/browser_script.rs
17e01a96732722e31b4f94d8d4df2df760ee36bd2293086195ea8d841f3f409c  js/brimstone/src/js/runtime/bytecode/vm.rs
9a2405af55ca47fe300b32d502f52b7bc28530acb43a2210fb665107b53d7e40  js/brimstone/src/js/runtime/context.rs
d608083145184adc4ccaee6988be706d3c0dd09666e3f51c54cfa70f52b778a3  js/brimstone/src/js/runtime/gc/heap.rs
7ff237df66ad9d6bc8ff9c5c486784ae32d10d79b267669e8d781daa03b98edc  js/brimstone/src/js/runtime/mod.rs
fe68b665d246524a2d4a10b972f54581a1cf62e7fa002085fe05c7b9544fc02c  js/brimstone/src/js/runtime/tasks.rs
```

The W7-A2Q frozen paths ended this wave with these exact SHA-256 values:

```text
e84ac94e449c384c24bd99a7da79f57c5a2bb7c0d3c4b906e45fe18bd2a2a612  bytecode/verifier.rs
11f3a26a8d2c79160e08d7e6251fe60a71968f08b0dc93b7fd974ad3897f401b  jit/compiler.rs
a19ae77aa1d1e8cc6dd806ad24252b0429406e7358abd944721ac4bddad160ab  jit/continuation.rs
4ed8fdd818319e36ac0fd250989532d4235dd5fbaf9761ed93eb2668dd9b1b33  jit/dispatch.rs
b46f867fb48eadda3b4372c32e3c9d74c8a61233be1118988a04a8d0b9ab919a  W7-A2Q handoff
```

The protected `js/README.md`, `js/src/**`, `js/tests/gc.rs`, `js/tests/symbols.rs`, and
`js/tests/control_flow.rs` hashes also match their W8 starting values. Other agents had concurrent
changes elsewhere in the shared worktree; W8 did not edit, format, stage, commit, or discard them.
