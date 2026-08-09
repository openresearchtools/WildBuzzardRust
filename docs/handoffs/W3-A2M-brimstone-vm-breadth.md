# W3-A2M bounded Brimstone VM-continuation breadth

- Task: Broaden the disabled Brimstone baseline-JIT side-exit proof from one numeric `Neg`/`Ret`
  tail to a bounded, statically admitted local control-flow graph executed by Brimstone's actual VM.
- Owner: Agent 2, JavaScript and WebAssembly runtime.
- Status: Conditional GO for this contained, off-by-default proof only. Product dispatch remains a
  compile-time false constant. DOM, untrusted-page execution, a browser baseline tier, and parity
  remain NO-GO.
- Upstream baseline: Brimstone `bfb720f0afb8b2b28b27c22ee7091deb7d16b082`; Cranelift `0.134.3`
  from the exact Wasmtime v47.0.3 source at
  `5554cc1a651da536af2cc46c7324bdc085b162e3`.
- Firefox reference paths: `js/src/vm/Interpreter.cpp`, `js/src/vm/Stack.cpp`,
  `js/src/gc/RootMarking.cpp`, `js/src/jit/Baseline*`, `js/src/jit/JitFrames*`, and applicable
  `js/src/jit-test/tests/{basic,gc}` behavior remain reference and future differential-test inputs.
- Wild Buzzard paths changed:
  `js/brimstone/src/js/runtime/bytecode/vm.rs`,
  `js/brimstone/src/js/runtime/jit/continuation.rs`, and
  `js/brimstone/src/js/runtime/jit/mod.rs`.

## Contract added

One exact rooted function/artifact binding may admit its native side exit into a monotone abstract
type/CFG proof. The proof starts at the exact verified instruction boundary, requires an exact
local-slot count, analyzes both successors of every conditional, and joins states without
evaluating branch outcomes. Analysis allocation is fallible, its modeled storage is capped at
32 MiB, and its worklist is capped at 2,000,000 dequeues.

The admitted actual-VM subset is deliberately local and noncoercing:

- local moves and immediate `undefined`, `null`, boolean, number, and internal `Empty` loads;
- `LogNot` and `TypeOf` over values already proven to be valid JavaScript values;
- number-only arithmetic, immediate arithmetic, `Neg`, `Inc`, `Dec`, and comparisons;
- exact-boolean, `ToBoolean`, undefined, and nullish branches, including rooted constant-backed
  targets;
- loops, `Ret`, and an uncaught terminal `Throw`.

`Empty` and internal heap metadata may be moved only so a dead slot can later be overwritten; every
consumer rejects them. Number-only admission keeps arithmetic and comparisons on Brimstone's
nonallocating fast paths. A terminal `Throw` may allocate only after its exact PC is published;
handler tables are rejected, so it cannot return to another admitted edge.

Every taken nonpositive edge validates exact source and target instruction starts, publishes the
target PC, and polls the shared deterministic interrupt budget. Quantum expiry, an external
interrupt request, and interrupt-policy failure remain distinct terminal outcomes. This is not a
resumable scheduler: the admitted VM frame is popped and exact parent SP/FP/depth are restored on
interrupt. Straight-line tails do not poll between instructions.

The private resumed dispatch disables comparison/branch fusion only for this proof so no backedge
can bypass polling. Its handle scope now exits on an inner dispatch panic before VM-frame cleanup.
Return, uncaught throw, interruption, policy failure, allocation failure, and panic paths all
release-check the exact parent stack state. `PRODUCT_DISPATCH_ENABLED` remains `false` with a const
assertion and there is still no non-test product caller.

## Evidence

All outputs were kept under `/home/user/Documents/wildbuzzardbuilds/`.

- Focused continuation tests: 24 passed with `baseline_jit`; 25 passed with
  `baseline_jit,gc_stress_test,alloc_error,handle_stats`.
- Full imported workspace: 35 default tests and 88 `baseline_jit` tests passed.
- Strict all-target Clippy passed for the default workspace, the `baseline_jit` workspace, and the
  combined-feature `brimstone_core` package.
- Locked release workspace check with `baseline_jit`, exact formatting, diff checks, and
  warning-denied `brimstone_core` rustdoc passed. Rustdoc allowed only the four already recorded
  imported-baseline classes: broken/private intra-doc links, bare URLs, and invalid HTML tags.
- A fresh nightly-2026-06-02 AddressSanitizer build of `std` and `brimstone_core`, with LeakSanitizer
  enabled, `no_jemalloc`, forced moving GC, allocation-error injection, and handle statistics,
  passed all 25 continuation tests with no sanitizer diagnostic. The later source change was prose
  only, clarifying terminal `Throw`; executable code is identical to the sanitizer-tested tree.
- An independent frozen-source audit returned GO for this contained proof after checking the exact
  artifact/root binding, abstract interpretation, actual VM fast paths, constant and prefixed
  branches, backedge polling, GC roots, unwind cleanup, and product-dispatch isolation.

Final ordered file hashes after the prose correction:

```text
0a6be81b878cd25f9af5bc16829ff3d7fc6a4e277da6eb2813062709fa4eb81a  js/brimstone/src/js/runtime/bytecode/vm.rs
c83e5692500c3a58bbe40ec81b19affe6358c9996581f090425451e8c6009ac7  js/brimstone/src/js/runtime/jit/continuation.rs
03349099f89a266716d640050443ce76416d1c43a478af09b2884e6993cbe4bd  js/brimstone/src/js/runtime/jit/mod.rs
```

## Boundaries and next work

No new unsafe block, native FFI, dependency, provider, or network capability was introduced. This
slice reuses the already admitted Linux x86-64 W^X and Cranelift boundary.

Calls, properties, cache-backed operations, allocating helpers beyond the existing zero-argument
`NewObject`, parameters, noninitial realms, runtime functions, handled exceptions, generators,
async suspension, deoptimization, OSR, debugger/unwind metadata, optimizing compilation, and
normal hot-function dispatch remain rejected. The abstract-analysis work limit counts dequeues,
not total local-cell scans; stronger cell-work metering is required before untrusted input. Add
direct branch-family regressions, complete internal lifetime migration and browser resource
policy, broaden generated and continued semantics with forced collection at every safepoint, then
connect product tiering only after Test262, fuzz, sanitizer, and browser-host gates pass.
