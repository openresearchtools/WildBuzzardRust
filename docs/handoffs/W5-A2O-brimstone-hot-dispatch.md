# W5-A2O disabled Brimstone hot dispatch and inline backedge polling

- Task: Replace per-backedge Rust polling with a stable inline fast path, connect the bounded
  baseline proof to Brimstone's ordinary Rust-call VM entry under test-only policy admission, and
  capture an exact receiver plus exact formal arguments.
- Owner: Agent 2, JavaScript and WebAssembly runtime.
- Status: Implemented and accepted by the locked external validation matrix below after hostile
  source review and correction. Product dispatch remains a compile-time false constant. This
  handoff must not be read as a browser baseline tier, DOM or untrusted-page admission, or Firefox
  parity evidence.
- Upstream baseline: Brimstone `bfb720f0afb8b2b28b27c22ee7091deb7d16b082`; Cranelift `0.134.3`
  from exact Wasmtime v47.0.3 source at
  `5554cc1a651da536af2cc46c7324bdc085b162e3`.
- Wild Buzzard paths changed:
  `js/brimstone/src/js/runtime/{context,bytecode/{function,vm},gc/handle}.rs`,
  `js/brimstone/src/js/runtime/jit/{abi,code_cache,compiler,continuation,dispatch,hotness,mod}.rs`,
  and this handoff. No manifest, lockfile, dependency, DOM, browser host, or product-admission
  constant changed.

## Hostile-review corrections

The post-implementation hostile review found five integration defects which the current source
closes before any new acceptance claim:

- Every bytecode `call_from_rust` now runs in one escaping child `HandleScope`. `HandleScope`
  itself is unwind-RAII, so ordinary return and JavaScript throw escape exactly one handle,
  allocation failure escapes none, and a terminal panic restores the child scope. The actual
  `VM::execute` handle-stat invariant is result-variant aware and uses checked addition.
- Every ordinary VM frame marked as having a Rust caller now has exact unwind ownership: direct
  bytecode calls, initial-realm frames, Rust-runtime callbacks, generator resume, direct bytecode
  construction, and recursive bytecode construction use `RustCallFrameGuard`. A guard may remove
  only well-formed ordinary VM descendants before its exact child and aborts on an intervening
  unowned Rust frame or any parent-state mismatch. The private JIT-resume frame retains its older
  equivalent catch-and-exact-pop protocol because that admitted continuation cannot call JS.
- The hot hook performs the same checked, nonmutating byte/slot/depth admission as ordinary frame
  push before compiling or entering generated code. A rejected near-capacity or maximum-depth call
  bypasses the hook and lets ordinary VM push construct the canonical stack-overflow error, with no
  native allocation or other effect.
- Native allocation cannot observe the wrong ambient realm before a callee frame exists. The hook
  reads the ambient realm through its existing unique VM borrow, reads the immutable initial realm
  through the VM owner's disjoint context field, and requires the exact caller/callee/initial triple
  before hotness or compilation. A moving-GC two-realm regression requires clean fallback and the
  callee realm's `ObjectPrototype`.
- Metadata/cache retirement now requires the mapping-removal boolean to equal
  `artifact_loaded` during LRU eviction, rejection, and shutdown. The stale-mapping regression runs
  corruption in a subprocess and requires `SIGABRT` during owner teardown instead of manually
  masking the mismatch.

The replacement unfiltered `baseline_jit,handle_stats` suite exercises actual `VM::execute` for
disabled policy, negative fallback, native return, JavaScript throw, nested
JS-to-JS-to-Rust-to-JS terminal interruption, constructor-callback poison, exact handle deltas,
exact SP/FP/depth restoration, and context reuse. A separate actual `Context::run_script`
terminal/reuse case and near-capacity/maximum-depth cases cover the product-shaped boundaries. The
locked matrix below validates this corrected source directly.

## Inline poll ABI

Generated-code ABI version 4 adds one stable `Arc`-owned `#[repr(C)] InlinePollState`. Its exact
32-byte Linux x86-64 layout contains an ABI version, structure size, immutable nonzero quantum,
atomic remaining-work count, atomic external-request bit, and zeroed reserved fields. A
`DeterministicInterruptBudget` and any cross-thread request handles share the allocation, so its
address remains stable for the complete synchronous generated activation. The owner-thread budget
is deliberately `!Send + !Sync`; a request handle can only atomically set the request bit.

The generated prologue validates the activation, poll pointer, exact poll header, nonzero bounded
remaining count, request representation, reserved fields, helper table, shadow frame, and exact
captured slot count before executing bytecode. Every taken nonpositive edge then performs this
sequence:

1. publish the exact verified target;
2. decrement the independent one-million-edge native residency cap inline;
3. atomically inspect the request and remaining-work fields;
4. on the ordinary path, atomically decrement remaining work, clear the publication, and jump
   without entering Rust;
5. enter the versioned nonallocating Rust slow helper only for an external request, quantum
   boundary, hard-cap boundary, or test-injected policy boundary.

The slow helper validates the exact activation/budget/poll identity again. It consumes the ordinary
budget first, preserving external-request or quantum-expiry priority when either coincides with the
hard cap or an injected policy failure. It then returns one distinct status for success,
interruption, rooted VM side exit, poison, or invalid activation. Panics are caught before the
generated-code boundary. A malformed zero remaining count or zero initial native cap is rejected in
the prologue before subtraction. Normal loop regressions require zero Rust poll-helper calls and
check exact inline decrements of both counters.

This is a correctness-first atomic load/store implementation. It is not yet a tuned thread-local
poll word, signal/watchdog integration, or proof of browser interruption latency.

## Actual hot-call hook

The ordinary `VM::call_from_rust` bytecode-function branch owns the sole hot-dispatch hook. With
`baseline_jit` compiled, every call follows that same hook; the dispatch policy defaults to
`PRODUCT_DISPATCH_ENABLED`, which remains const-asserted `false`. Only `cfg(test)` methods can
enable policy admission or change thresholds and fault injection. Tests do not call a detached
dispatcher to exercise successful hot execution: they configure policy and enter through the
ordinary VM hook.

One bounded entry records saturating function hotness. An exact-arity call which reaches its call
threshold receives a never-reused `VmBindingId`, verifies and compiles the exact rooted bytecode,
loads one inseparable RX artifact, performs whole-entry preflight, and enters generated code. Later
exact calls freshly root and revalidate the closure, function, scope, realm, constant table, cache
array, bytecode, slot counts, branch constants, and artifact identity before reusing that code.
Missing or extra arguments interpret without creating a hotness entry, because ordinary
JavaScript's arity adjustment remains VM-owned.

Only failure before generated entry may interpret. Compilation, binding, exact-slot conversion,
or whole-entry preflight failure becomes a rooted negative cache entry: it owns no RX mapping,
remains LRU eligible, and prevents another compile/admission attempt until eviction. Once native
execution is committed, return, throw, native/VM interruption, allocation failure, poison, invalid
activation, stale code, and continuation failure are terminal. The ordinary interpreter cannot
replay bytecode effects.

A native or continued return/throw is held in the private higher-ranked JIT handle scope. The hook
copies its raw `Value`, drops that scope, restores dispatcher ownership, and immediately re-roots
the value in the enclosing VM handle scope. No allocation or collection occurs between the copy and
re-root. A nested/reentrant call observes the dispatcher as temporarily unavailable and interprets;
the panic and nested-call tests use the same take/restore guard and verify exact VM cleanup.

## Ownership, roots, and cache coherence

`ContextCell` stores two deliberately separate fields:

- `Option<BaselineDispatchState>` owns pointer-free hotness metadata and RX mappings;
- `BaselineDispatchRoots` owns at most 32 moving-GC `BytecodeFunction` roots in fixed,
  generation-checked slots.

For one synchronous attempt, an unwind-RAII guard takes the pointer-free state out of
`ContextCell`, leaving `None` as the reentry sentinel. This avoids holding a mutable reference into
the context while compilation, helpers, moving collection, or actual VM continuation access the
same context. The sibling root registry stays in place and is the only dispatch-owned structure
visited by the collector. The guard restores the state exactly once on success or unwind;
unexpected replacement, missing state at teardown, missing/generation-mismatched roots, duplicate
never-reused cache keys, and registry/code disagreement fail closed.

The dispatcher bounds rooted entries to 32 and executable mappings to 32 entries and 8 MiB. Its
metadata and code caches retire synchronously: code eviction removes the exact entry and root before
another artifact is installed, including when later RW staging fails. Rejected functions retain
their exact root but no RX artifact. Context teardown explicitly removes every mapping and root
before ordinary field destruction and aborts on an orphan.

Tests cover take/restore after panic, actual nested-call interpreter fallback, threshold and code
reuse, exact-arity fallback, rooted negative caching, stale missing-code termination without
recompile or interpreter replay, moving collection before both first negative-cache creation and
negative-cache reuse, generation-checked LRU retirement, coherent shutdown, native moving GC,
external and quantum interruption, injected helper panic/poison, exact VM stack cleanup, and
same-artifact recovery.

## Captured call layout and conservative preflight

The exact generated/bridge layout is:

```text
[local 0 ... local N-1, receiver, argument 0 ... argument M-1]
```

Native destinations remain local-only. The receiver and exact formal arguments are immutable input
sources for moves, guarded arithmetic/unary/comparison operations, conditions, and return. The
compiler treats captured heap-pointer inputs as unproven at entry, so an operation which cannot
safely consume or return them natively side-exits before mutating its destination. Every receiver
and formal argument is conservatively included in every allocating `NewObject` stack map; forced
moving-GC tests verify the function root and captured pointer values are rewritten and the same RX
artifact remains reusable. The real VM continuation pushes the exact receiver and arguments using
ordinary frame layout, then restores the captured local values before publishing the exact resume
PC.

Whole-entry preflight proves all reachable paths before native execution. It may model a
zero-capacity `NewObject` only while execution is still guaranteed native. A conservative
VM-reachability bit propagates after every potentially dynamic native guard and branch (including a
backedge hard-cap exit). If a later path could execute `NewObject` in the ordinary resumed VM, entry
is rejected and negatively cached. Actual resume analysis always rejects `NewObject`. This
deliberately preserves W3-A2M's invariant that every admitted resumed-VM cycle is nonallocating;
the gate does not broaden the allocating continuation surface merely to make whole-entry preflight
convenient.

## Firefox reference evidence

Reference checkout: detached Firefox ESR153
`c19b7e89270787889495688244ec6ee8e79288a1`.

- `firefox/js/src/jit/BaselineCodeGen.cpp` around the interrupt-check lowering synchronizes the
  frame, checks runtime interrupt state inline, and calls the VM interrupt helper only on the
  nonzero slow path. Its loop-head lowering combines the interrupt check with warmup accounting.
- `firefox/js/src/jit/BaselineJIT.cpp` and the `MaybeEnterJit` call path record warmup before the
  admission checks, cleanly report a not-entered result before JIT execution, and keep a distinct
  committed JIT outcome after entry. Wild Buzzard preserves the observable no-replay boundary but
  uses its own simpler bounded Rust ownership and identity design.
- `firefox/js/src/jit/VMFunctions.cpp` routes the slow interrupt call through runtime interrupt
  handling; `JitFrames.cpp`/`JitFrames.h` remain references for frame tracing, return-PC identity,
  and future unwind/debugger work.
- History commit `db17034edd588bee0e9f43a491c4203975615533` folded `LOOPENTRY` into
  `LOOPHEAD` while preserving loop interrupt/warmup behavior. Commit
  `bc60f1d2e34af58b7f7e7536b78cab6983401216` assigned interrupt VM calls a unique return-address
  kind for debug-mode OSR.
- Applicable Firefox infinite-loop watchdog, interrupt-callback, GC, argument, and baseline
  warmup tests remain future differential inputs rather than copied fixtures.

## Evidence

All Cargo, rustc, rustdoc, Clippy, sanitizer, and test output stayed outside the repository under
`/home/user/Documents/wildbuzzardbuilds/w5-a2o/`, with `TMPDIR` fixed beneath that tree. Cargo
dependency resolution was offline for every acceptance build. The stable
toolchain was `rustc 1.95.0 (59807616e 2026-04-14)` and
`cargo 1.95.0 (f2d3ce0bd 2026-03-21)`. The sanitizer used
`rustc 1.98.0-nightly (6bdf43094 2026-06-01)` from `nightly-2026-06-02`, including its installed
`rust-src` component and a freshly instrumented standard library.

| Locked corrected-source gate | Result |
| --- | --- |
| default full workspace tests | 35 passed: 24 core plus 11 integration/snapshot; 0 failed |
| `baseline_jit` full workspace tests | 120 passed: 109 core/JIT plus 11 integration/snapshot; 0 failed |
| unfiltered `brimstone_core` with `baseline_jit,handle_stats` | 112 passed; 0 failed |
| JIT filter with `baseline_jit,gc_stress_test,alloc_error,handle_stats` | 89 passed; 0 failed; 25 filtered |
| default workspace, strict all-target Clippy | passed with `-D warnings` |
| `baseline_jit` workspace and combined-feature package, strict all-target Clippy | both passed with `-D warnings` |
| release workspace check with `baseline_jit` | passed with `-D warnings` |
| warning-denied `brimstone_core` rustdoc with `baseline_jit` | passed with only the four inherited classes explicitly allowed below |
| nightly AddressSanitizer plus LeakSanitizer, forced moving GC, allocation errors, handle statistics, and `no_jemalloc` | 89 passed; 0 failed; 25 filtered; no address or leak diagnostic |
| repository rustfmt, `git diff --check`, product-dispatch/feature, and in-repository artifact audits | passed |

Commands were run from the repository root. Each Cargo target was a distinct external directory:

```sh
cargo +1.95.0 fmt --manifest-path js/brimstone/Cargo.toml --all -- --check

CARGO_NET_OFFLINE=true \
  CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w5-a2o/default CARGO_INCREMENTAL=0 \
  TMPDIR=/home/user/Documents/wildbuzzardbuilds/w5-a2o/tmp cargo +1.95.0 test \
  --manifest-path js/brimstone/Cargo.toml --locked --workspace

CARGO_NET_OFFLINE=true \
  CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w5-a2o/baseline-corrected \
  CARGO_INCREMENTAL=0 \
  TMPDIR=/home/user/Documents/wildbuzzardbuilds/w5-a2o/tmp cargo +1.95.0 test \
  --manifest-path js/brimstone/Cargo.toml --locked --workspace --features baseline_jit

CARGO_NET_OFFLINE=true \
  CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w5-a2o/handle-stats \
  CARGO_INCREMENTAL=0 \
  TMPDIR=/home/user/Documents/wildbuzzardbuilds/w5-a2o/tmp cargo +1.95.0 test \
  --manifest-path js/brimstone/Cargo.toml --locked -p brimstone_core \
  --features baseline_jit,handle_stats

CARGO_NET_OFFLINE=true \
  CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w5-a2o/stress-corrected \
  CARGO_INCREMENTAL=0 \
  TMPDIR=/home/user/Documents/wildbuzzardbuilds/w5-a2o/tmp cargo +1.95.0 test \
  --manifest-path js/brimstone/Cargo.toml --locked -p brimstone_core \
  --features baseline_jit,gc_stress_test,alloc_error,handle_stats runtime::jit::

CARGO_NET_OFFLINE=true \
  CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w5-a2o/clippy-default-corrected \
  CARGO_INCREMENTAL=0 TMPDIR=/home/user/Documents/wildbuzzardbuilds/w5-a2o/tmp \
  cargo +1.95.0 clippy --manifest-path js/brimstone/Cargo.toml --locked --workspace \
  --all-targets -- -D warnings

CARGO_NET_OFFLINE=true \
  CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w5-a2o/clippy-baseline-corrected \
  CARGO_INCREMENTAL=0 TMPDIR=/home/user/Documents/wildbuzzardbuilds/w5-a2o/tmp \
  cargo +1.95.0 clippy --manifest-path js/brimstone/Cargo.toml --locked --workspace \
  --all-targets --features baseline_jit -- -D warnings

CARGO_NET_OFFLINE=true \
  CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w5-a2o/clippy-combined-corrected \
  CARGO_INCREMENTAL=0 TMPDIR=/home/user/Documents/wildbuzzardbuilds/w5-a2o/tmp \
  cargo +1.95.0 clippy --manifest-path js/brimstone/Cargo.toml --locked \
  -p brimstone_core --all-targets \
  --features baseline_jit,gc_stress_test,alloc_error,handle_stats -- -D warnings

CARGO_NET_OFFLINE=true \
  CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w5-a2o/release-corrected \
  CARGO_INCREMENTAL=0 \
  TMPDIR=/home/user/Documents/wildbuzzardbuilds/w5-a2o/tmp RUSTFLAGS=-Dwarnings \
  cargo +1.95.0 check --manifest-path js/brimstone/Cargo.toml --locked --workspace \
  --release --features baseline_jit

CARGO_NET_OFFLINE=true \
  CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w5-a2o/rustdoc-corrected \
  CARGO_INCREMENTAL=0 \
  TMPDIR=/home/user/Documents/wildbuzzardbuilds/w5-a2o/tmp \
  RUSTDOCFLAGS='-D warnings -A rustdoc::broken_intra_doc_links \
  -A rustdoc::private_intra_doc_links -A rustdoc::bare_urls \
  -A rustdoc::invalid_html_tags' cargo +1.95.0 doc \
  --manifest-path js/brimstone/Cargo.toml --locked -p brimstone_core --no-deps \
  --features baseline_jit

CARGO_NET_OFFLINE=true ASAN_OPTIONS='detect_leaks=1:halt_on_error=1' \
  CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w5-a2o/asan-corrected \
  CARGO_INCREMENTAL=0 \
  TMPDIR=/home/user/Documents/wildbuzzardbuilds/w5-a2o/tmp \
  RUSTFLAGS='-Zsanitizer=address -Cdebuginfo=1' RUSTDOCFLAGS='-Zsanitizer=address' \
  cargo +nightly-2026-06-02 test -Zbuild-std --manifest-path js/brimstone/Cargo.toml \
  --locked -p brimstone_core --lib --target x86_64-unknown-linux-gnu \
  --no-default-features \
  --features baseline_jit,gc_stress_test,alloc_error,handle_stats,no_jemalloc runtime::jit::
```

The rustfmt configuration contains nightly-only keys; stable rustfmt reported that those keys were
ignored and returned success. Rustdoc denied every warning except the four inherited imported-code
classes named in the command: broken and private intra-doc links, bare URLs, and invalid HTML tags.
This is not represented as a strict full-workspace documentation gate.

The hostile review identified the five ownership, realm, stack-admission, panic-cleanup, and cache
coherence defects described above; none of the earlier pre-review runs count as acceptance. The
first corrected-source compile then exposed a test-only helper whose configuration needed to match
`handle_stats`. One nested-call regression initially placed multi-argument `Call` inputs in the
parameter region rather than the bytecode format's required contiguous local temporaries; only the
fixture layout changed. A transient over-broad test constructor configuration was also corrected
before acceptance. Every applicable gate above ran after its relevant correction, and the final
default and `baseline_jit` workspace suites were refreshed after source freeze.

Frozen corrected-source SHA-256 values:

| Source | SHA-256 |
| --- | --- |
| `js/brimstone/src/js/runtime/bytecode/function.rs` | `356018ec7f297f1ceb303c1650ccba5e363baa88cae71e3d2608e93a07e402db` |
| `js/brimstone/src/js/runtime/bytecode/vm.rs` | `4e56811b3270619e33072a6d01e57e53659d3038d8791810e16e404fa39a42a9` |
| `js/brimstone/src/js/runtime/context.rs` | `d38b54566cb42160f04d16c09a247ea03ec27f42435d35c3e25d20a04c381933` |
| `js/brimstone/src/js/runtime/gc/handle.rs` | `2d527078e2ef78bf155f5b4c9fa68ba62f90dbcfcc608696ddb1139cd739a82f` |
| `js/brimstone/src/js/runtime/jit/abi.rs` | `7e8a8a63dbcef4f37e3a626abc3629d01e686ceb6ba23ea4a9abfb926d7cdbe5` |
| `js/brimstone/src/js/runtime/jit/code_cache.rs` | `e5f94ce760ce696071e503161105fce9161d8bb9d0a13b02903ba1bc14bebf33` |
| `js/brimstone/src/js/runtime/jit/compiler.rs` | `d35df5d488eb22a522ca637bd530db26ed53ff80d1466a98518597eb40a9cf6b` |
| `js/brimstone/src/js/runtime/jit/continuation.rs` | `ce5c43070dbb155ce1981182da2c578f9021f6fee08884bf56a9bc53ee384389` |
| `js/brimstone/src/js/runtime/jit/dispatch.rs` | `e5efff0afbb0eba6cf3f5aeca9ff25b5d3e5395e0e0ee9e7e6c33782c7dc0761` |
| `js/brimstone/src/js/runtime/jit/hotness.rs` | `3d53ebd285ed1afee41ae91edde89e7dced5492414dc1d55747a7bfee233ce7f` |
| `js/brimstone/src/js/runtime/jit/mod.rs` | `88ba1e5bc25b50a5af4d49f829bea717b1c4b6a4e7cf6ab879ae3b6f235239b4` |

## Boundaries and next work

`baseline_jit` remains off by default and `PRODUCT_DISPATCH_ENABLED` remains compile-time `false`.
No DOM binding, untrusted page, browser host, constructor path, or ordinary in-VM call opcode can
enter this tier. The only connected entry is an exact-arity bytecode function called through
`VM::call_from_rust` under the test-only enabled policy.

This gate has no properties, calls, cache-backed operations, handled exceptions, noninitial realms,
runtime functions, constructor/new-target support, OSR, deoptimization, invalidation,
debugger/unwind metadata, complete native stack maps, optimizing tier, browser interrupt wiring,
browser resource policy, Test262/WPT claim, or performance gate. Hotness is call-threshold only;
backedge warmup does not yet trigger compilation. Preflight is repeated for every admitted call and
its dequeue limit still does not charge every local-cell scan. The fixed root/code bounds are a
contained proof policy, not a browser memory manager. Remaining raw context/handle lifetime
migration, broader fuzzing and sanitizer coverage, generational/incremental GC work, and actual
product-tier admission all remain open.
