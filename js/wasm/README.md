# Wild Buzzard WebAssembly core adapter

`wild_buzzard_wasm` is the first browser-owned boundary around the exact local Wasmtime v47.0.3
source. One `WasmProcess` owns one Wasmtime engine and all modules, stores, and instances for a
single browser content process. Callers receive only owner- and generation-checked IDs; Wasmtime
handles and pointers never cross the boundary.

This gate accepts core WebAssembly binaries only. It validates and compiles bounded byte slices,
rejects every import before a module is admitted, instantiates with an empty import list, and calls
only exports whose parameters and results are all `i32`. It exposes no host function, linker,
filesystem, socket, HTTP client, environment, clock, random source, WASI interface, component,
serialized-native-code loader, cache, CLI, WAT parser, async/fiber path, or ambient capability.

The dependency has default features disabled and enables exactly `std`, `runtime`, `cranelift`,
`gc`, `gc-drc`, and `threads`. Cranelift and the deferred-reference-counting collector are selected
explicitly. The compile-time `threads` feature is retained for later reviewed browser work, while
the runtime threads and shared-memory proposal is disabled. Winch is absent.

The initial proposal policy explicitly enables stable core features, reference types, function
references, deterministic relaxed SIMD, SIMD, bulk memory, multi-value, multi-memory,
extended-constant expressions, tail calls, and exception handling. It explicitly disables Wasm GC
objects, threads/shared memory, shared-everything threads, memory64, stack switching, custom page
sizes, branch hints, wide arithmetic, and legacy exceptions. Wasm GC stays off because this slice
does not yet have a hard cross-heap cycle/accounting contract. No GC or reference value can cross
the `i32`-only call API.

Hard policy limits cover input bytes, admitted modules/stores/instances, instances per store,
linear-memory bytes/count, table elements/count, call arity, export-name bytes, Wasm stack, and
fuel. Fuel bounds calls and start functions deterministically. Epoch interruption supplies an
externally triggerable synchronous terminal path without async or fibers. Store removal
cascade-invalidates its instances; module removal is rejected while an instance still depends on
it; reset tears down instances, stores, then modules and invalidates every prior ID. Because
Wasmtime owns allocations at store granularity and does not report whether a failing instantiation
allocated before failure, every instantiation attempt is conservatively charged against resident
process, store, and module counts until that store is removed. Invalidating an instance ID never
pretends to reclaim its store-owned allocation.

These limits bound logical Wasm resources through finite store counts, per-store resource counts,
and per-resource caps; they do not bound total resident memory. Internal adapter bookkeeping,
compiled code and engine caches, virtual-memory reservations and guards, other host allocations,
and per-store GC heaps are not comprehensively charged. Content-process integration must enforce
exactly one `WasmProcess` owner even though this standalone crate exposes public construction for
testing and later embedding. The adapter's interrupt sequence poisons instead of wrapping, but the
independent Wasmtime epoch counter's behavior at a natural `u64` rollover is not yet proven by this
gate.

Compilation is synchronous. The module byte cap and Wasmtime validation provide structural/input
bounds, but this gate has no compilation wall-clock deadline, compilation cancellation, compiled
code-size accounting, sandbox/AppImage acceptance, or authenticated compiled-code cache. Those are
required before untrusted product activation.

This crate is not the JavaScript `WebAssembly` API, a Brimstone bridge, a rooted cross-heap wrapper,
a WebAssembly specification-suite result, browser conformance evidence, a sandbox boundary, or a
product-activation claim. It must remain disabled from page content until those later gates are
reviewed and tested.
