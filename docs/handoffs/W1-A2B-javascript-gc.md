# W1-A2B JavaScript tracing-GC handoff

- Task: W1-A2B safe tracing collector, reusable generational heap slots, and realm-wide collection safe points
- Owner: Agent 2 — JavaScript/WebAssembly; integrated and independently reviewed by the main orchestrator
- Status: Complete for the implemented Wave 1 heap graph; this is not SpiderMonkey GC, ECMAScript, Wasm, or browser parity
- Firefox commit and source paths: ESR153 `c19b7e89270787889495688244ec6ee8e79288a1`; RootingAPI, HeapAPI, RootMarking, Sweeping, ArenaList, environment, interpreter, realm/context, and stack paths are recorded in `js/README.md`
- Firefox test paths: persistent-root and GC-marking jsapi tests plus twelve focused Test262/jit-test cases are listed in `js/README.md`; all cited paths and history revisions were independently verified
- Wild Buzzard paths changed: `js/src/heap.rs`, `js/src/runtime.rs`, `js/src/lib.rs`, `js/tests/gc.rs`, and `js/README.md`
- Contract added or changed: explicit non-moving stop-the-world `Context::collect_garbage`, typed generation-checked heap handles, tombstone free lists, permanently retired exhausted slots, root/arena diagnostics, and structured safe-point/trace failures
- Tests run and results: owner and orchestrator root-integrated gates passed on `x86_64-unknown-linux-gnu`; 54 total JavaScript tests passed, including 12 GC integration tests; formatting, check, strict Clippy, locked tests, release build, and rustdoc passed with external targets
- Parity evidence: complete strong-edge tracing for the currently implemented intrinsic/global environments, root registry, lexical chains/bindings, object/function properties, closures, rooted exceptions, and values retained through the public host/job API; unreachable cycles and stale generations are exercised
- Known behavioral differences: explicit idle-only collection; no automatic threshold, nursery, incremental/concurrent phases, stack maps, in-entry collection, barriers, weak edges, finalizers, compaction, arbitrary host trace hooks, heap byte limits, or OOM-safe allocation
- Unsafe or FFI introduced: None; the crate forbids unsafe code and exposes no heap pointer
- Licenses and provenance: MPL-2.0 first-party implementation informed by pinned ESR153 behavior, tests, and history; zero external dependencies
- Provider or network implications: None; collection performs no I/O and introduces no endpoint
- Blocked on: Nothing for continued engine work; stack maps and barriers are required before collection can safely become incremental or run during execution
- Recommended next action: build the standards-driven object/array/property model and Test262 evidence on this root contract, then add shared JS/Wasm edges before evolving the collector
