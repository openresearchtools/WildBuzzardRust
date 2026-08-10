# W7-A2Q: rooted constants and synchronous closure creation

## Status and scope

W7-A2Q is complete as a contained, test-only extension of the disabled Brimstone baseline-JIT
proof. Work started from Wild Buzzard commit
`1c7f4aa2d78fafc43172bfbb0ad50580f703afa6`. The adopted Brimstone source baseline remains upstream
revision `bfb720f0afb8b2b28b27c22ee7091deb7d16b082`; this wave does not refresh that import.

The exact W7-A2Q write set is:

- `js/brimstone/src/js/runtime/bytecode/verifier.rs`
- `js/brimstone/src/js/runtime/jit/compiler.rs`
- `js/brimstone/src/js/runtime/jit/continuation.rs`
- `js/brimstone/src/js/runtime/jit/dispatch.rs`
- `docs/handoffs/W7-A2Q-brimstone-rooted-constants.md`

No root manifest, lockfile, toolchain, CI, status, product-admission, imported upstream, or
first-party transitional-JS file is part of this gate.

## Observable result

The private tier now preserves exact typed constants across verification, compilation, moving GC,
cache reuse, and an actual Brimstone VM continuation:

- `undefined`, `null`, booleans, SMIs, and canonical non-SMI doubles have exact pointer-free
  descriptors and may be emitted as relocation-free boxed immediates.
- Strings and BigInts have exact moving-GC roots and take a mandatory side exit before
  `LoadConstant` changes its destination. The ordinary VM loads the rooted table entry.
- Bytecode-function constants have exact moving-GC roots and are admitted only where the opcode
  contract expects function metadata. `NewClosure` always exits before its allocating effect and
  is executed synchronously by the ordinary VM.
- Raw jump offsets remain non-value descriptors. They are never put through the value root walker.
- Replacing a rooted heap constant with a different allocation is rejected before native entry,
  even when the replacement is the same type and has equal content.
- A function can contribute at most 1,024 constant-table entries. The cap is exact and is checked
  before a binding registers any new root.

Two nested `NewClosure` levels now run through ordinary hot dispatch in the test policy. Each call
creates a fresh closure, a forced moving collection between cached invocations preserves the exact
function/constant identities, and the leaf function executes normally. Throw, interrupt, and
cleanup paths use the existing ordinary VM outcomes; no generated closure effect is replayed.

This is not product admission. The `baseline_jit` feature remains nondefault and
`PRODUCT_DISPATCH_ENABLED` remains a literal compile-time `false` guarded by a const assertion.

## Constant representation and rooting contract

### Pointer-free compilation metadata

`ConstantKind` now describes the VM-bound ordinary values needed by this gate:

| Descriptor | Meaning | Contains a heap address? |
| --- | --- | --- |
| `Undefined`, `Null` | exact primitive singleton | no |
| `Boolean(bool)` | exact boolean | no |
| `SmallInteger(i32)` | exact Brimstone SMI | no |
| `DoubleBits(u64)` | exact canonical boxed non-SMI number bits | no |
| `String`, `BigInt` | heap-value kind only | no |
| `BytecodeFunction` | internal function-metadata kind only | no |
| `JumpOffset(isize)` | exact raw branch displacement | no |

The verifier fallibly copies the complete descriptor slice into `VerifiedBytecode`.
`PreparedProgram` then fallibly copies it into immutable boxed storage inseparable from the
prepared and loaded artifact. Neither object stores an untraced heap address. The generated code
embeds only primitive boxed bits which cannot move; it has no constant-table relocation or raw
heap-pointer literal.

The generic `AnyValue` descriptor remains useful for structural verifier tests, but it cannot
admit `BytecodeFunction` or other engine metadata as an ECMAScript value. A VM-bound program never
uses `AnyValue`: it derives an exact descriptor from every validated table entry.

### Binding preflight and the 1,024-entry cap

`VmFunctionBinding` owns both the rooted constant table and one independent handle cell per value
entry. A raw jump entry has a descriptor and `None` instead of a value handle. Construction follows
this order:

1. Validate the incoming closure value and read the exact constant count.
2. Reject a count above 1,024 before registering a new binding root.
3. Fallibly reserve both host vectors and validate every raw entry into a pointer-free descriptor.
4. Root the constant table.
5. Re-read each value through that rooted table immediately before registering its independent
   handle.
6. Root the closure, function, scope, realm, and optional cache-array identities, then revalidate
   the complete binding.

The re-read rule avoids depending on a table pointer obtained before an earlier root registration.
The currently audited `Value::to_handle` path writes only to Brimstone's host-side handle blocks;
growing a block uses `Box` and does not allocate in the managed JavaScript heap or invoke GC. Both
facts are documented in the binding code. Any later change which makes handle registration a
managed-heap safepoint must preserve the rooted-table re-read contract.

Validation compares all of the following before native entry and before publishing a VM frame:

- the closure, function, scope, realm, constant-table, and cache-array identities;
- execution flags, register and argument counts, exact bytecode, and exact artifact binding ID;
- constant count and every immutable descriptor;
- raw jump kind and exact displacement;
- every value entry's exact rooted identity.

During legitimate moving collection, the constant table and each independent value handle are
both rewritten, so they continue to compare equal. Same-kind or equal-content substitution updates
only the table and therefore fails closed with `ConstantIdentityChanged` before generated code can
run.

### Persistent cache roots and failure cleanup

An ephemeral call binding cannot keep constants alive after it leaves `try_invoke`, so each loaded
dispatch artifact now has a sibling constant-root snapshot in `BaselineDispatchRoots`. The fixed
registry still has at most 32 function entries; together with the per-function cap it can retain at
most 32,768 value slots. Raw jumps occupy `None` slots and are checked through artifact metadata.

The snapshot is installed after successful verification/compilation and before cache insertion.
An insertion error clears the just-installed constants while retaining the function's negative
cache entry. Normal rejection, same-artifact deferred rejection, LRU eviction, and shutdown remove
the exact constant roots with the corresponding RX artifact. Registry coherence asserts that an
entry has a constant snapshot if and only if it owns a loaded artifact. Generation checks prevent
a stale ID from clearing or reading a reused slot.

## Lowering and continuation boundary

| Bytecode case | Generated behavior | Ordinary-VM behavior |
| --- | --- | --- |
| `LoadConstant` primitive or non-SMI double | write exact boxed bits, continue natively | not needed unless a later guard exits |
| `LoadConstant` string or BigInt | mandatory side exit before destination write | load the exact rooted entry and continue |
| `NewClosure` with `BytecodeFunction` | mandatory side exit before allocation or destination write | load exact function metadata, inherit the current scope/realm, allocate once, store result |
| constant-backed branch | use exact validated raw displacement | revalidate the same displacement before resume |
| unsupported value/metadata kind | reject compilation or continuation | ordinary untiered call remains available only on a clean pre-entry rejection |

`NewClosure` is deliberately not a native helper in this wave. The native activation reports the
exact source PC before any effect, spills and roots every live slot, unlinks, and materializes an
ordinary Brimstone VM frame. The VM therefore owns allocation, collection, current-scope capture,
throw propagation, interruption at exact backedges, and stack unwinding. A post-entry failure is
terminal to this contained dispatch attempt and never falls back by replaying the whole function.

The abstract-CFG proof models exact constant result types. It admits a `BytecodeFunction` only as
the metadata operand of `NewClosure`, models the resulting local as an object, and continues to
reject internal heap metadata consumed as an ECMAScript value. Pointer-backed `LoadConstant` and
`NewClosure` make all successors potential VM states; allocating loops remain subject to the
ordinary exact-PC backedge interrupt budget.

## Firefox ESR behavioral reference

The ignored Firefox ESR153 checkout was read only as behavioral and regression reference:

- `firefox/js/src/vm/Opcodes.h` specifies exact constant categories and describes `Lambda` as a
  fresh function object inheriting the current environment chain.
- `firefox/js/src/vm/Interpreter.cpp` loads doubles, strings, and BigInts from exact script data;
  its `Lambda` case roots the function, uses the current frame environment, and propagates
  allocation failure.
- `firefox/js/src/jit/BaselineCodeGen.cpp` distinguishes inline doubles from GC-backed script
  things and routes `Lambda` through an IC before pushing its result.
- `firefox/js/src/jit/BaselineIC.cpp` roots the function and environment in
  `DoLambdaFallback`, clones exactly once, and returns failure rather than replaying execution.
- Full-history search identifies commit `530d88a5cdd1` (Bug 1937570 part 2, 2024-12-19) as the
  introduction of the Baseline `Lambda` IC. Older interpreter history remains available from the
  pinned full checkout.

Wild Buzzard uses its own bounded Rust root registry and side-exit design. These references support
observable constant and closure semantics; they are not an architectural port of SpiderMonkey.

## Validation evidence

All Cargo, rustc, rustdoc, Clippy, sanitizer, and test output stayed outside the repository under
`/home/user/Documents/wildbuzzardbuilds/w7-a2q/`. Incremental compilation was disabled, `TMPDIR`
was beneath that tree, and Cargo dependency resolution was offline. Stable validation used Rust
1.95.0. Sanitizer validation used `nightly-2026-06-02` with an instrumented standard library.

| Locked corrected-source gate | Result |
| --- | --- |
| default full Brimstone workspace tests | 35 passed: 24 core plus 11 integration/snapshot; 0 failed |
| `baseline_jit` `brimstone_core` library tests | 127 passed; 0 failed |
| unfiltered `baseline_jit,handle_stats` library tests | 131 passed; 0 failed |
| unfiltered `baseline_jit,alloc_error,gc_stress_test,handle_stats` library tests | 134 passed; 0 failed |
| strict all-target `baseline_jit` Clippy | passed with `-D warnings` |
| release all-target `brimstone_core` check with `baseline_jit` | passed |
| warning-denied `brimstone_core` rustdoc with `baseline_jit` | passed with only the four inherited warning classes explicitly allowed below |
| nightly ASan/LSan JIT tests with forced moving GC, allocation errors, handle statistics, and `no_jemalloc` | 109 passed; 0 failed; 25 filtered; no address or leak diagnostic |
| exact-path rustfmt and `git diff --check` | passed |

Representative exact commands, run from `js/brimstone/`, were:

```sh
CARGO_NET_OFFLINE=true CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w7-a2q/default \
  CARGO_INCREMENTAL=0 TMPDIR=/home/user/Documents/wildbuzzardbuilds/w7-a2q/default/tmp \
  cargo +1.95.0 test --locked --workspace

CARGO_NET_OFFLINE=true CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w7-a2q/baseline \
  CARGO_INCREMENTAL=0 TMPDIR=/home/user/Documents/wildbuzzardbuilds/w7-a2q/baseline/tmp \
  cargo +1.95.0 test --locked -p brimstone_core --features baseline_jit --lib

CARGO_NET_OFFLINE=true CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w7-a2q/handle-stats \
  CARGO_INCREMENTAL=0 TMPDIR=/home/user/Documents/wildbuzzardbuilds/w7-a2q/handle-stats/tmp \
  cargo +1.95.0 test --locked -p brimstone_core --features baseline_jit,handle_stats --lib

CARGO_NET_OFFLINE=true CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w7-a2q/stress \
  CARGO_INCREMENTAL=0 TMPDIR=/home/user/Documents/wildbuzzardbuilds/w7-a2q/stress/tmp \
  cargo +1.95.0 test --locked -p brimstone_core \
  --features baseline_jit,alloc_error,gc_stress_test,handle_stats --lib

CARGO_NET_OFFLINE=true CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w7-a2q/clippy \
  CARGO_INCREMENTAL=0 TMPDIR=/home/user/Documents/wildbuzzardbuilds/w7-a2q/clippy/tmp \
  cargo +1.95.0 clippy --locked -p brimstone_core --features baseline_jit \
  --all-targets -- -D warnings

CARGO_NET_OFFLINE=true CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w7-a2q/release \
  CARGO_INCREMENTAL=0 TMPDIR=/home/user/Documents/wildbuzzardbuilds/w7-a2q/release/tmp \
  cargo +1.95.0 check --release --locked -p brimstone_core --features baseline_jit --all-targets

CARGO_NET_OFFLINE=true CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w7-a2q/rustdoc \
  CARGO_INCREMENTAL=0 TMPDIR=/home/user/Documents/wildbuzzardbuilds/w7-a2q/rustdoc/tmp \
  RUSTDOCFLAGS='-D warnings -A rustdoc::broken_intra_doc_links \
  -A rustdoc::private_intra_doc_links -A rustdoc::bare_urls \
  -A rustdoc::invalid_html_tags' cargo +1.95.0 doc --locked -p brimstone_core \
  --no-deps --features baseline_jit

CARGO_NET_OFFLINE=true ASAN_OPTIONS='detect_leaks=1:halt_on_error=1' \
  CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w7-a2q/asan \
  CARGO_INCREMENTAL=0 TMPDIR=/home/user/Documents/wildbuzzardbuilds/w7-a2q/asan/tmp \
  RUSTFLAGS='-Zsanitizer=address -Cdebuginfo=1' RUSTDOCFLAGS='-Zsanitizer=address' \
  cargo +nightly-2026-06-02 test -Zbuild-std --locked -p brimstone_core --lib \
  --target x86_64-unknown-linux-gnu --no-default-features \
  --features baseline_jit,gc_stress_test,alloc_error,handle_stats,no_jemalloc runtime::jit::
```

The accepted rustdoc gate denies every warning except the four inherited Brimstone-wide classes
shown above: broken private/public intra-doc links, bare URLs, and invalid HTML tags. Stable rustfmt
reported that the repository's nightly-only configuration keys were ignored; it formatted only the
four owned Rust files and the subsequent exact-path check passed.

## Focused regression evidence

New or extended tests prove:

- exact descriptor capture by the structural verifier;
- native exact-bit `-0.0` return without a heap relocation;
- a mixed double/string/BigInt program across two forced moving-GC handoffs;
- exact same-content string substitution rejection before native entry;
- admission of exactly 1,024 entries and pre-root rejection of 1,025 entries, including unchanged
  handle statistics;
- synchronous `NewClosure` followed by an uncaught throw, with forced collection before and after
  VM-frame publication;
- `NewClosure` in a polled loop, exact interruption, empty native-root registry, exact VM-stack
  cleanup, and context recovery;
- two nested ordinary hot-dispatch closure factories, fresh identity per invocation, forced moving
  collection before cached rebind, and normal leaf execution;
- persistent cache-root cleanup after exact-identity rejection and coherent negative caching.

The full stress matrix also re-exercises existing allocation-error injection, moving-GC safepoints,
ordinary and nested calls/constructors, recursive pinned artifacts, throw/unwind, panic recovery,
near-capacity frames, wide-prefix bytecode, and deterministic interruption.

## Workspace and source audit

The shared worktree contains concurrent changes owned by other browser, graphics, and transitional
JavaScript lanes. W7-A2Q did not edit, format, stage, or otherwise absorb those paths. In
particular, the pre-existing protected changes under `js/README.md`, `js/src/**`, and `js/tests/**`
retain their recorded SHA-256 identities from task start. No repository-local `target/`, AppImage,
AppDir, sanitizer log, generated documentation, debug symbol, or other build artifact was created.

`git diff --check` and exact-path rustfmt pass for the W7-A2Q source. No commit, push, staging,
status-file update, or `AGENTS.md` change is part of this component handoff; integration remains the
main orchestrator's responsibility.

## Boundaries and next work

W7-A2Q is not a browser JIT tier and grants no DOM or untrusted-content entry. It does not provide
native heap-constant loads, native closure allocation, ordinary property/cached operations,
general closure/function forms, handled-exception metadata, OSR, deoptimization, invalidation,
debugger or unwind metadata, complete native stack maps, an optimizing tier, browser watchdog and
resource policy, Test262 completion, or a performance gate.

The fixed 32-entry dispatch registry, 1,024-entry per-function constant cap, test-only hot policy,
and interpreter side exits are bounded proof mechanisms. Product work still requires the remaining
raw context/handle lifetime migration, full moving-GC and unsafe audit, broader fuzz/Miri/sanitizer
coverage, generational/incremental collection, browser host bindings, standards conformance, and a
measured tiering design. Do not enable product dispatch on the strength of this wave.
