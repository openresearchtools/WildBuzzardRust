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

Wasmtime is the selected WebAssembly compiler/runtime core. The audited stable baseline is
Wasmtime `v47.0.3`, commit `5554cc1a651da536af2cc46c7324bdc085b162e3`, tree
`c48fdb3d3530ac038f149f17d9e35f0a554ec0ec` (2026-07-31). It uses Rust edition 2024, has an MSRV
of Rust 1.94.0, and is licensed Apache-2.0 with LLVM exception. No Wasmtime source is imported by
this decision alone; the exact minimal source/dependency closure is a separate mechanically
reviewable import.

The initial configuration is the `wasmtime` crate with default features disabled and only
`std,runtime,cranelift,gc,gc-drc,threads`. This resolved to 23 Wasmtime-tree packages and 59
registry packages in the audit, with no Git dependency. Use Cranelift only: Winch's own v47.0.3
manifest says it should not be used in production, and its x86-64 proposal coverage is incomplete.
Do not enable the CLI, WAT, WASI, WASI HTTP, filesystem/socket/server hosts, component model,
automatic cache, async fibers, stack switching, profiling, coredumps, debug built-ins, address-to-
line support, pooling allocator, or ambient capabilities for ordinary web content. The web
platform does not grant WASI capabilities.

Wasmtime supplies a strong Rust base for core Wasm validation and native compilation and lists
reference types, function references, Wasm GC, exception handling, SIMD, relaxed SIMD, tail calls,
and multi-memory as Tier 1 under Cranelift. It is nevertheless a standalone embedding, not the
JavaScript `WebAssembly` API. Its functional DRC collector cannot reclaim cycles, and the public
v47.0.3 documentation calls its cycle-capable copying collector not yet functional. Threads are
Tier 2, unfuzzed, incomplete around shared-memory resource limiting, and incompatible with the
pooling allocator. Memory64 is still warned as unfinished/lightly exercised. Stack switching is
Tier 3, x86-64 Linux-only, and incomplete; JSPI is not implemented as a browser API. These features
remain separate gated deliverables.

The exact minimal product library passed `cargo check` on `x86_64-unknown-linux-gnu`. The matching
upstream `cargo test -p wasmtime --lib` configuration failed to compile with 19 missing-feature
guard errors in cache, pooling, and component-related test code. Do not enable unwanted defaults to
hide that gap. The admitted browser-core workspace must patch or upstream the guards, add a
selected-feature integration target, and run the pinned Wasm spec suite before activation.

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

Use one shared Wasmtime `Engine` per content process and a resource-accounted
`Store<BrowserWasmState>` per site/agent-cluster runtime. Wrappers carry generation-checked IDs;
Wasmtime `ExternRef` host data carries only a bridge ID, never a Brimstone pointer or thread-affine
handle. Wasmtime owns linear-memory allocation. Brimstone ArrayBuffer wrappers resolve a memory ID
and generation for every access rather than caching a relocatable base pointer. Shared-memory
maximums are enforced before creation and growth because upstream's `ResourceLimiter` does not
cover those paths.

Brimstone and Wasmtime currently have separate collectors. Cross-heap values use stable, validated
host IDs and explicit root/trace transitions; raw pointers never cross the boundary. Before exposing
`externref` or Wasm GC objects to pages, forced-collection tests must cover JS-to-Wasm-to-JS cycles,
weak references, exceptions, tables, globals, suspended work, realm teardown, process shutdown, and
out-of-memory recovery. A design which permanently roots each heap from the other and leaks cycles
does not pass.

Use epoch interruption for browser cancellation. Fuel is a deterministic test/quota tool, not the
default scheduler. Serialized native code is never accepted from web content: Wasmtime's deserialize
APIs are unsafe, so a future browser-owned cache must be locally generated, origin/config/version/
CPU keyed, integrity-protected, atomic, bounded, and evictable.

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
