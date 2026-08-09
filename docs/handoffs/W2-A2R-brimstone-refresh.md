# W2-A2R Brimstone upstream refresh

- Task: Refresh the editable canonical Brimstone baseline to the newest upstream `master` observed
  at final integration, without importing nested repository metadata or losing the separately
  reviewable Wild Buzzard hardening/JIT patches.
- Owner: Agent 2 — JavaScript/WebAssembly; applied by the main orchestrator and independently
  reviewed for source identity, provenance, rooting/GC safety, and JIT compatibility.
- Status: Complete for the source-refresh boundary. This is not approval for product dispatch,
  DOM bindings, untrusted content, or JavaScript/WebAssembly parity.
- Upstream identity: `https://github.com/Hans-Halverson/brimstone.git`, branch `master`, revision
  `bfb720f0afb8b2b28b27c22ee7091deb7d16b082`, tree
  `6c1221d72cff1dcec5fdb6cd10d329abb935f126`, commit time
  `2026-08-08T19:56:40-07:00`, sole parent
  `b544eff181ef6a72639f26a89b6aca1f8d6e6b50`.
- Source delta: one upstream commit and four paths, with 529 insertions and 49 deletions. It updates
  the named-property cache and VM for primitive prototype access, extends the GC-stress ignore
  list, and adds `primitive_prototype.js`. The upstream inventory grows from 1,554 to 1,555 files.
- Contract added or changed: `bfb720f0` is now the single canonical Brimstone source baseline.
  Commit `4063038` remains the exact original `b544eff` import checkpoint; the refresh is a distinct
  exact upstream delta layered below the Wild Buzzard adaptations. The live tree contains no
  nested Git metadata and remains editable in Wild Buzzard's own history.
- Source verification: a clean, non-shallow, detached upstream checkout matched remote `master`,
  commit, tree, parent, time, and all four changed blobs. Zero upstream paths were missing. All
  remaining common-path differences and eight extra files classified exactly as the committed
  W2-A2H adaptations, current W2-A2J JIT work, and `WILDBUZZARD_UPSTREAM.md`. No nested `.git`,
  `target`, generated index, compiled artifact, submodule, symlink, or Git LFS omission was present.
- Tests run and results: from external copies/targets under
  `/home/user/Documents/wildbuzzardbuilds`, the refreshed adapted tree passed 185/0/0 normal
  integration tests and 179/0 with six configured skips under `gc_stress_test`. The new
  `primitive_prototype.js` test also passed directly with the combined `baseline_jit` and
  `gc_stress_test` features and the Test262 host exposed. W2-A2J records the Rust, release,
  sanitizer, Clippy, verifier, W^X, and generated-code gates separately.
- Safety review: the upstream prototype walk does not allocate. The prototype is converted to a
  handle before validity-guard allocation; the original primitive, coerced object, and key remain
  rooted; and every cached shape, guard, prototype, and polymorphic entry is traced. The patch adds
  no unsafe block, FFI, dependency, public raw-context escape, opcode, or bytecode-layout change.
  `GetNamedProperty` remains a conservative allocating/calling safepoint and the contained JIT
  compiler side-exits before it.
- Parity evidence: the results establish faithful source refresh and preservation of the selected
  upstream integration behavior only. They are not a complete Test262, browser-host, GC-safety,
  performance, or Firefox-parity result.
- Licenses and provenance: MIT, with the upstream `LICENSE` unchanged. Exact identities and local
  patches are recorded in `js/brimstone/WILDBUZZARD_UPSTREAM.md` and
  `docs/upstream-components.toml`.
- Provider or network implications: none at runtime. The freshness check and detached audit clone
  accessed GitHub; Cargo retains the recorded upstream Git build dependency and the test harness
  uses an externally staged exact Test262 checkout.
- Remaining blockers: every untrusted-content blocker in `AGENTS.md` remains. A future upstream
  advance is not accepted automatically; it requires another exact freshness, delta, safety,
  conformance, and provenance review.
- Recommended next action: accept the independently reviewed, disabled W2-A2J JIT infrastructure
  gate, then connect GC-traced native frames, allocating helpers, interruption, and exact side-exit
  resume in separately forced-collection-tested slices.
