# W2-A2H Brimstone ownership and rooting handoff

- Task: Remove Brimstone's safe double-destruction/stale-root embedding surface, establish an
  exactly-once owner and lifetime-branded moving roots, and decide whether contained baseline-JIT
  infrastructure may begin.
- Owner: Agent 2 — JavaScript/WebAssembly; independently reviewed, corrected, and gated by the main
  orchestrator.
- Status: Conditional GO for disabled/internal JIT infrastructure. NO-GO for DOM bindings or
  untrusted browser content.
- Firefox commit and source paths: ESR153
  `c19b7e89270787889495688244ec6ee8e79288a1`; SpiderMonkey context/realm ownership, GC roots,
  native stack maps, JIT safepoints, interrupts, and tests remain behavioral reference only.
- Firefox test paths: relevant SpiderMonkey GC/JIT tests, Test262, fuzzers, and DOM binding/WPT paths
  remain required; this slice uses Brimstone's own tests.
- Wild Buzzard paths changed: Brimstone runtime context and GC modules; CLI, serializer, benchmark,
  snapshot, fuzz, and integration harness callers; provenance/status/architecture records; and this
  handoff.
- Contract added or changed: `ContextBuilder::build` returns a non-copyable, non-cloneable,
  non-`Send`, non-`Sync` `OwnedContext` which automatically destroys its allocation exactly once.
  Higher-ranked `with_root_scope` creates non-escaping, non-copyable roots updated by the moving
  collector. Safe raw construction/manual destruction is gone. Legacy raw context/handle/heap
  types and pre-generated script/module entry points are named, hidden, and unsafe.
- Tests run and results: final external gates passed formatting, `git diff --check`, complete nested
  workspace tests (4 ownership plus 11 upstream Rust/snapshot tests), strict product-feature Clippy
  with warnings denied, and full release workspace build. The release harness passed 184/0
  Brimstone integration tests and 44,867/0 selected Test262 cases with 8,419 configured skips.
  Collect-on-every-allocation passed five ownership tests and 182/0 integration cases with two
  configured stress skips. Nightly AddressSanitizer plus LeakSanitizer passed all five ownership,
  unwind, relocation, resize, and forced-GC tests with no final address or leak diagnostic.
- Parity evidence: these results establish the bounded ownership/rooting adaptation and preserve the
  upstream selected-green behavior. The Test262 run uses `--ignore-unimplemented` and is not a full
  conformance percentage or browser-parity claim.
- Known behavioral differences: eight non-core tool/test call sites still use the explicit unsafe
  raw-context escape. Upstream internals still contain pervasive copyable contexts, lifetime-free
  handles/heap pointers, and unrestricted mutable dereferencing after unsafe acquisition. No safe
  general host-object/value facade, hard execution/allocation/recursion limits, interrupt polling,
  browser module loader, or browser-scale collector exists. Miri was unavailable.
- Unsafe or FFI introduced: no new native FFI. The safe surface narrows, but the implementation
  deliberately retains hidden unsafe raw aliases while upstream internals migrate. AddressSanitizer
  first exposed invalid `HeapInfo` initialization, leaked handle blocks, and bitwise-copied resize
  ownership; those defects were fixed. LeakSanitizer then exposed a bump-arena `Rc<Options>` leak;
  `ScopeTree` now retains only the copyable Annex B flag.
- Licenses and provenance: the adapted source remains MIT and based on exact Brimstone revision
  `b544eff181ef6a72639f26a89b6aca1f8d6e6b50`. Commit `4063038` preserves the byte-identical import;
  local patches are recorded in `WILDBUZZARD_UPSTREAM.md` and `docs/upstream-components.toml`.
- Provider or network implications: none at runtime. Cargo still resolves the recorded lock-pinned
  upstream Git dependency; the Test262 checkout is external development input only.
- Blocked on: untrusted exposure remains blocked on internal lifetime/root migration, safe host
  bindings, limits/interrupts, fuzz/Miri evidence where applicable, malformed-input/OOM tests,
  JIT safepoint correctness, full conformance, and process-scale memory behavior.
- Recommended next action: implement the first JIT infrastructure gate behind an off-by-default
  feature: exact Cranelift 0.134.3, complete opcode metadata/verifier, hot counters, stable unsafe
  helper ABI, canonical GC-visible shadow frames, bounded RW-to-RX code memory, interruptible
  backedges, and side exits. No generated path which can allocate may activate until forced-GC at
  every native safepoint proves all live references are spilled, traced, and reloaded.
