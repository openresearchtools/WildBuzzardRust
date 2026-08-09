# JavaScript and WebAssembly runtime decision

Status: accepted direction; import and hardening in progress. This record does not claim that either
engine is browser-ready or integrated.

## JavaScript execution baseline

Wild Buzzard uses the exact Brimstone source tree at
`b544eff181ef6a72639f26a89b6aca1f8d6e6b50` as its canonical JavaScript execution baseline. The
source repository is `https://github.com/Hans-Halverson/brimstone.git`; `master` pointed to that
revision when selected on 2026-08-09. The source lives at `js/brimstone/`, without nested Git
metadata. `docs/upstream-components.toml` is the authoritative provenance record.

The selection preserves Brimstone's strong execution substrate: register bytecode, compact
NaN-boxed values, VM frames, shapes, prototype guards, and polymorphic property caches. It does not
accept the current ownership or collector safety as a browser boundary. Upstream expressly labels
the engine not production-ready and its collector very unsafe.

The existing `wild_buzzard_js` crate is transitional. Its safe rooted embedding contract and
focused semantic tests may be migrated or used as differential evidence, but its interpreter must
not remain as a second page engine. One content process owns one canonical Brimstone-backed runtime
and heap; that runtime may contain multiple same-site realms. Site isolation creates additional
content processes rather than one micro-VM for every tab.

## JIT program

The product target includes both a fast interpreter and native Linux x86-64 JIT tiers. The first
native tier is a bounded Cranelift baseline compiler which retains boxed values and canonical
GC-visible VM frames. Complex, throwing, suspending, or initially unsupported operations side-exit
to the interpreter.

No generated code may call arbitrary Rust ABI functions, retain moving heap addresses, or omit a GC
or interruption poll. Every potentially allocating helper must publish a bytecode location, spill
all live heap references to a traced frame, call through a stable helper ABI, and reload after a
possible collection. Generated code lives outside the moving heap in bounded W^X memory and is
referenced by stable IDs. Invalidated shape/prototype assumptions must deoptimize or side-exit.

The optimizing tier follows only after baseline correctness under forced collection. It adds typed
feedback, SSA, unboxing, inlining, OSR, deoptimization snapshots, precise stack maps, exception and
debug metadata, bounded code eviction, and reproducible performance gates. GC work proceeds in the
same program: browser-scale operation ultimately needs partitioned generational/incremental
collection and explicit memory-pressure behavior rather than one full semispace collection for all
live objects.

## WebAssembly core

Selected Wasmtime crates are the candidate WebAssembly compiler/runtime core. The reviewed stable
baseline is Wasmtime `v47.0.3`, commit `5554cc1a651da536af2cc46c7324bdc085b162e3`
(2026-07-31); no Wasmtime source is imported by this decision alone. Pinning happens in a separate
mechanically reviewable import after its dependency, native-code, feature, and license audit.

Use the core `wasmtime`, Cranelift/Winch, environment, runtime, and spec-test facilities that are
actually required. Do not expose or silently enable the Wasmtime CLI, WASI, WASI HTTP, filesystem,
socket, server, or component-host capability layers for ordinary web content. The web platform does
not grant WASI capabilities.

Wasmtime already supplies a strong Rust base for core Wasm validation and native compilation and
supports the major reference-types, function-references, GC, exception-handling, SIMD, tail-call,
threads, memory, and interruption mechanisms. It is nevertheless a standalone embedding, not the
JavaScript `WebAssembly` API. Its own current stability record calls threads incomplete in important
areas and stack switching a work in progress limited to x86-64 Linux; current source also rejects
the stack-switching-plus-GC combination. JavaScript Promise Integration must therefore remain a
separate gated deliverable.

## Browser-owned integration

The Wild Buzzard adapter owns:

- `WebAssembly.Module`, `Instance`, `Memory`, `Table`, `Global`, `Tag`, `Exception`, `CompileError`,
  `LinkError`, and `RuntimeError` objects and their exact ECMAScript conversions;
- `compile`, `instantiate`, streaming variants, module imports/exports/custom sections, promises,
  jobs, cancellation, CSP, MIME validation, and cross-origin-isolation decisions;
- ArrayBuffer and SharedArrayBuffer backing-store identity, detachment/growth rules, shared memory,
  atomics, workers, and explicit memory/resource limits;
- deterministic trap and exception conversion, stack/source mapping, debugger/profiler hooks,
  tiering policy, executable-memory limits, cache partitioning, and code invalidation; and
- host function/reference conversions and the complete Brimstone/Wasmtime rooting protocol.

Brimstone and Wasmtime currently have separate collectors. Cross-heap values use stable, validated
host IDs and explicit root/trace transitions; raw pointers never cross the boundary. Before exposing
`externref` or Wasm GC objects to pages, forced-collection tests must cover JS-to-Wasm-to-JS cycles,
weak references, exceptions, tables, globals, suspended work, realm teardown, process shutdown, and
out-of-memory recovery. A design which permanently roots each heap from the other and leaks cycles
does not pass.

## Acceptance gates

The engine becomes an active browser runtime only after all of the following are recorded:

1. Safe owned context/realm facade, audited handles, forced-GC/fuzz/sanitizer evidence, hard limits,
   deterministic interruption, and no safe API capable of double destruction or stale aliasing.
2. Pinned metadata-aware Test262 results, explicit skip accounting, SpiderMonkey differential
   regressions, and modern site workloads without hidden fallback to another JS engine.
3. Baseline-JIT correctness with collection at every safepoint, W^X/code-budget tests, exception and
   side-exit tests, then optimizing-tier/deoptimization and performance evidence.
4. Pinned Wasm specification tests for every enabled proposal plus JavaScript WebAssembly API WPT
   and cross-heap rooting/lifecycle tests.
5. Multi-realm and multi-process memory, CPU, cancellation, teardown, crash-isolation, and many-tab
   stress tests on `x86_64-unknown-linux-gnu`.

## Upstream references

- Brimstone pinned README and readiness statement:
  `https://github.com/Hans-Halverson/brimstone/blob/b544eff181ef6a72639f26a89b6aca1f8d6e6b50/README.md`
- Wasmtime v47.0.3 source:
  `https://github.com/bytecodealliance/wasmtime/tree/v47.0.3`
- Wasmtime proposal stability and known limitations:
  `https://github.com/bytecodealliance/wasmtime/blob/v47.0.3/docs/stability-wasm-proposals.md`
- Wasmtime Rust embedding API:
  `https://docs.wasmtime.dev/api/wasmtime/`
