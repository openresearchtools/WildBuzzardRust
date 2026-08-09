# W2-A2W Wasmtime core admission handoff

- Task: Audit the newest stable Wasmtime release as Wild Buzzard's browser WebAssembly execution
  core and define the feature, collector, browser-host, security, and AppImage boundaries.
- Owner: Agent 2 — JavaScript/WebAssembly; independently reviewed by the main orchestrator.
- Status: Conditional GO for exact source import and development against Wasmtime/Cranelift core;
  NO-GO for treating upstream unchanged as a complete browser WebAssembly implementation.
- Firefox commit and source paths: ESR153
  `c19b7e89270787889495688244ec6ee8e79288a1`; SpiderMonkey `js/src/wasm`, JIT/GC/value
  conversion paths, and DOM WebAssembly bindings remain behavioral reference only.
- Firefox test paths: applicable Wasm specification tests, SpiderMonkey jit-tests, Web Platform
  Tests, and browser security/isolation tests remain required.
- Wild Buzzard paths changed: `AGENTS.md`, `docs/architecture/javascript-wasm-runtime.md`,
  `docs/import-status.md`, `docs/program-status.toml`, and this handoff. No Wasmtime source was
  imported by the audit.
- Contract added or changed: pin Wasmtime `v47.0.3`, commit
  `5554cc1a651da536af2cc46c7324bdc085b162e3`, tree
  `c48fdb3d3530ac038f149f17d9e35f0a554ec0ec`; initial features are
  `std,runtime,cranelift,gc,gc-drc,threads` with default features disabled. Use Cranelift only and
  keep the entire JavaScript `WebAssembly` API, policy, jobs, backing stores, cache, resource
  accounting, and cross-heap bridge browser-owned.
- Tests run and results: a clean external v47.0.3 clone passed locked minimal-feature `cargo check`
  for `x86_64-unknown-linux-gnu` in 27.16 seconds. The corresponding upstream library-test target
  failed to compile with 19 cache/pooling/component missing-feature guard errors. The selected
  product library is valid; Wild Buzzard must fix/report that test-matrix gap rather than enable
  unwanted defaults. The pinned spec-test submodule was not initialized or run in this audit.
- Parity evidence: upstream's stability matrix lists reference types, function references, GC,
  exception handling, SIMD, tail calls, and multi-memory as Tier 1 under Cranelift. This is upstream
  capability evidence only, not browser API or Firefox parity evidence.
- Known behavioral differences: DRC cannot collect cycles; the cycle-capable copying collector is
  documented as not yet functional. Threads/shared memories are Tier 2, unfuzzed, not fully covered
  by `ResourceLimiter`, and cannot use the pooling allocator. Memory64, stack switching, and JSPI
  remain gated. Cross-heap JS/Wasm cycles have no admitted collector protocol yet.
- Unsafe or FFI introduced: no live-tree code yet. The selected core uses substantial reviewed
  unsafe VM/JIT/signal/memory code and compiles one C helper for unwind-registration detection. It
  requires Linux executable mappings, signal chaining, unwind registration, and a shared W^X policy
  with Brimstone's future JIT. Wasmtime native-code deserialization is unsafe and may never accept
  web-supplied or unauthenticated cache artifacts.
- Licenses and provenance: Apache-2.0 WITH LLVM-exception, Rust edition 2024, MSRV 1.94.0. The
  audited closure contained 23 Wasmtime-tree and 59 registry packages with no Git dependency; its
  registry licenses were compatible MIT/Apache-family, LLVM-exception, Zlib, Unlicense/MIT, and
  Unicode-3.0 combinations. Exact license/checksum preservation remains an import gate.
- Provider or network implications: no WASI, filesystem, socket, HTTP, server, or other ambient
  capability is exposed to page code. Cargo/network access was audit-only; the admitted closure
  must support a locked offline build. A future compiled-code cache is local, partitioned,
  integrity-protected, bounded, and never populated from untrusted serialized native code.
- Blocked on: exact minimal source/dependency import, selected-feature test repair, Wasm spec tests,
  browser adapter, cross-heap cycle collection, shared-memory resource enforcement, fuzzing,
  signal/sandbox/AppImage validation, and browser conformance evidence.
- Recommended next action: import the exact minimal v47.0.3 closure separately, preserve licenses,
  create a locked Wild Buzzard core workspace, repair the disabled-default unit-test guards, and
  run the exact Cranelift/x64 Wasm spec configuration before connecting any Brimstone wrapper.
