# W2-A2L rooted Brimstone VM continuation

- Task: Replace W2-A2K's detached checked continuation with one narrowly admitted side exit into
  Brimstone's actual bytecode VM while keeping generated execution disconnected from product
  dispatch.
- Owner: Agent 2 — JavaScript/WebAssembly; frozen-source implementation and an independent fresh
  review both returned GO for this contained gate.
- Status: Complete for the off-by-default rooted native-to-VM proof. NO-GO for normal tiering,
  browser or DOM entry, untrusted content, or JavaScript-engine parity.
- Upstream baselines: Brimstone
  `bfb720f0afb8b2b28b27c22ee7091deb7d16b082`; exact Cranelift `0.134.3` source from imported
  Wasmtime v47.0.3 revision `5554cc1a651da536af2cc46c7324bdc085b162e3`.
- Firefox reference: ESR153 `c19b7e89270787889495688244ec6ee8e79288a1`, especially
  `js/src/jit/BaselineBailouts.cpp`, `js/src/jit/JitFrames.{h,cpp}`,
  `js/src/vm/Interpreter.{h,cpp}`, `js/src/gc/RootMarking.cpp`, and the `baseline`, `ion`, and `gc`
  directories under `js/src/jit-test/tests/`. SpiderMonkey remains behavioral and architecture
  reference only; no SpiderMonkey implementation was copied.
- Wild Buzzard paths changed: eleven Brimstone files in the bytecode constant/function/frame/
  verifier/VM, context, and feature-gated JIT ABI/cache/compiler/continuation modules. No manifest,
  lockfile, dependency, root workspace, or product entry point changed.

## Rooted identity and admission contract

`OwnedContext::with_jit_context` now owns a real `HandleScopeGuard` and uses a higher-ranked
lifetime. `VmFunctionBinding` creates distinct private handles for the exact closure, bytecode
function, scope, realm, optional constant table, and optional cache array. A fresh, never-reused
binding ID is inseparably recorded in the prepared/loaded code artifact; identical bytes from a
different function cannot rebind the artifact.

Compilation and every entry/handoff revalidate the rooted identities, exact bytecode, register and
argument counts, constant/cache counts, safepoint frame shape, and constant-backed branch metadata.
Branch descriptors are derived from the rooted raw constant table rather than caller-supplied
descriptors. This gate rejects runtime functions, exception handlers, parameters, non-initial
realms, every value constant, and nonempty cache arrays. The VM accepts only the module-private
`AdmittedVmResume` proof; safe code cannot pass an arbitrary closure/program/offset tuple.

The admitted VM tails remain intentionally tiny:

- already-numeric, local-register `Neg` immediately followed by terminal `Ret`; and
- terminal `Throw`, known to be uncaught because handler-bearing functions are rejected.

Resume starts at the verifier's exact instruction boundary, including the prefix start for Wide
and ExtraWide instructions. Unsupported instructions, coercing `Neg`, nonlocal operands, handlers,
and backedges remain fail-closed.

## Native-to-VM root and frame transition

On an ordinary side exit, the linked native activation validates every slot and copies the complete
snapshot into lifetime-branded Brimstone handles. The native frame is then unlinked, identity and
artifact metadata are checked again, moved values are copied back without allocation, and the VM
constructs a normal traced Rust-caller frame at the exact resume PC. Fixed optional constant/cache
frame slots use an explicit raw zero for `None`; GC visits `Option<HeapPtr<_>>` locally and writes
the updated encoding back, avoiding an invalid `Option` layout cast.

`RootedSlotSet::sync_to_slots` is allocation-free and all-or-clear: a length mismatch clears the
entire destination instead of exposing a refreshed prefix followed by stale moving pointers. The
outer catch synchronizes slots before returning or resuming a panic. Generated helper activation
clears every slot not live in the exact safepoint map before interruption polling, allocation, or a
caught helper panic. Late native validation failures also clear all caller slots.

The real VM bridge distinguishes native return, VM return, VM throw, interruption, native or VM
allocation failure, poison, invalid activation, and unsupported side exit. Allocation-error and
panic cleanup verify and pop the exact resumed frame. Normal `Ret`, uncaught `Throw`, setup failure,
and allocation-error cleanup all have a release-effective exact parent SP/FP/depth postcondition;
corruption aborts rather than returning a partially unwound VM to safe Rust. The VM's stack
capacity gate now proves byte distances with checked integer arithmetic before any pointer
subtraction, so an oversized admitted frame returns stack overflow without out-of-allocation
pointer arithmetic or partial publication.

Forced-moving-GC regressions cover both sides of VM-frame publication, two allocating native PCs
with distinct maps, overwriting a moving-pointer destination, dead-slot terminal outcomes, raw
constant-backed branches, Wide/ExtraWide resume, VM return/throw, allocation failure, injected
panic cleanup, near-capacity stack rejection, and reuse of the same context after failure.
`PRODUCT_DISPATCH_ENABLED` remains compile-time false.

## Frozen source identity

The final aggregate is SHA-256 over the following `sha256sum` records in the listed order, with
paths relative to `js/brimstone/`:

`979fe0829ac920e586e16aae812793bed615b8b3e3f9f239959de63830a0067c`

| Relative path | SHA-256 |
| --- | --- |
| `src/js/runtime/bytecode/constant_table.rs` | `a155ebb04b3f86f954a02e58cd34b71f774bc2184a2354ed4fbbf15966ef12c6` |
| `src/js/runtime/bytecode/function.rs` | `432f9faefb76f138660d2ac89d68e839c74fed3361ca38c098706dd3444d91d4` |
| `src/js/runtime/bytecode/stack_frame.rs` | `ebad98d743d1799c4b70b025cdc9baf5a72a750d5147082962c5fe5bc05e60ab` |
| `src/js/runtime/bytecode/verifier.rs` | `e9273872f93c1333b42dd122b80db934447295c746a66d732850434e148ce827` |
| `src/js/runtime/bytecode/vm.rs` | `4e160f0e10ab63a048ba443c85cd5da89c0a82977852421ec36ec5a0c5f4b7cb` |
| `src/js/runtime/context.rs` | `4fdc8b7792937d60006c451f05c54f937cc2b9d47862fa5974c079fde66cecae` |
| `src/js/runtime/jit/abi.rs` | `a999cd08e0a4d9372dc9470496f17d794e81dbbf405bbf02ff5bcd0bc0127d3a` |
| `src/js/runtime/jit/code_cache.rs` | `1d2cf4ee16d4d43f93cb85b1c43395c8a0302e332c4ec80e4d7bec4c55c47f3b` |
| `src/js/runtime/jit/compiler.rs` | `54acbe7678573fdd115fcd613a23a8722522703c3c9f2011ceb5b9dad3f6d3ff` |
| `src/js/runtime/jit/continuation.rs` | `bb7fae5e5e54d9862e52fbf5dd950e720aacdc1e2b865a66d1dad33a109ff235` |
| `src/js/runtime/jit/mod.rs` | `5049c38810458c0ebfda56a5438e012a6438535efb19bad9d2cbc4b80f9f4295` |

## Build and test evidence

All Cargo output remained outside the repository. The owner used Rust/Cargo 1.95.0 and
`CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w2-a2l-vm-continuation`; the independent
review used the fresh target
`/home/user/Documents/wildbuzzardbuilds/w2-a2l-final-review`. Representative full-gate invocation:

```sh
CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w2-a2l-vm-continuation \
  cargo +1.95.0 test --manifest-path js/brimstone/Cargo.toml --workspace \
  --locked --features baseline_jit
```

The owner command matrix passed:

| Configuration or filter | Result |
| --- | --- |
| default full workspace | 35 passed: 24 core plus 11 integration/snapshot; 0 failed |
| `baseline_jit` full workspace | 80 passed: 69 core/JIT plus 11 integration; 0 failed |
| `brimstone_core` continuation filter with `baseline_jit` | 16 passed; 0 failed |
| `brimstone_core` ABI filter with `baseline_jit` | 8 passed; 0 failed |
| optional fixed stack-frame filter | 1 passed; 0 failed |
| allocation-error VM recovery filter | 1 passed; 0 failed |
| continuation filter with `baseline_jit,gc_stress_test` | 16 passed; 0 failed |
| release check and release build | passed |
| strict default, `baseline_jit`, and combined-feature all-target Clippy | passed with `-D warnings` |

The independent reviewer repeated the frozen source under its fresh target:

| Independent configuration | Result |
| --- | --- |
| `baseline_jit` full workspace | 80 passed; 0 failed |
| continuation with `baseline_jit,gc_stress_test,alloc_error` | 17 passed; 0 failed |
| release `brimstone_core` with `baseline_jit` | 69 passed; 0 failed |
| nightly-2026-06-02 AddressSanitizer with leak detection and `no_jemalloc` | 17 passed; no address or leak diagnostic |
| strict default/baseline/combined all-target Clippy, format, diff, dependency, feature, and artifact audits | passed |

Focused package commands used `-p brimstone_core --lib` plus the named feature set and test filter;
release added `--release`. Every Cargo gate used `--locked`, an external `CARGO_TARGET_DIR`, and the
Linux x86-64 host. The sanitizer gate used the same focused 17-test configuration through
`cargo +nightly-2026-06-02` with explicit `x86_64-unknown-linux-gnu`, Rust's address sanitizer, and
LeakSanitizer enabled.

Strict workspace `RUSTDOCFLAGS="-D warnings"` remained an inherited failure, producing 10,468
lines of pre-existing imported documentation diagnostics. A warning-denied `brimstone_core`
`baseline_jit` documentation build passed with exactly these four inherited classes allowed:
`rustdoc::broken_intra_doc_links`, `rustdoc::bare_urls`, `rustdoc::invalid_html_tags`, and
`rustdoc::private_intra_doc_links`. No other warning was allowed, and this is not reported as a
strict workspace rustdoc pass.

Independent frozen-diff review returned final GO for this contained gate and retained explicit
NO-GO for product dispatch, DOM/untrusted execution, or parity claims.

## Residuals, provenance, and next work

The injected VM panic regression fires after frame publication and optional forced collection but
before entering `dispatch_loop`. It proves the new bridge's frame/slot cleanup, but it does not
exercise a panic originating inside dispatch. The inherited `dispatch_loop` wrapper uses
`HandleScope::new`, whose inner scope is not an unwind RAII guard; on an internal panic those
logical roots are not restored until the outer JIT `HandleScopeGuard` unwinds. Product dispatch is
false and the admitted tails are bounded, but broader VM continuation must replace or wrap that
inner scope and add an internal-dispatch panic regression before product use.

This gate still lacks normal hot-function dispatch, broad opcode continuation, calls, properties,
handlers/catch/finally, backedge execution, deoptimization, OSR, invalidation, debugger and native
unwind metadata, asynchronous interruption, production stack maps, hard browser resource policy,
the remaining raw-context/handle lifetime migration, fuzz/Miri where applicable, full Test262,
DOM bindings, and untrusted-content gates.

No dependency or license changed. Brimstone remains MIT and Cranelift remains Apache-2.0 WITH LLVM
exception. No endpoint, provider integration, WASI capability, branding, or telemetry was added.
The next gate should preserve rooted artifact identity while widening VM tails one semantic family
at a time, harden dispatch unwind scopes, then add normal tier selection only after separate
forced-GC, exception, invalidation, resource, conformance, and browser-boundary reviews.
