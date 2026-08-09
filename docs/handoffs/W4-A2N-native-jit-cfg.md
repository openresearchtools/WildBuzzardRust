# W4-A2N bounded native Brimstone CFG breadth

- Task: Broaden the disabled Brimstone baseline proof from a tiny straight-line generated subset
  to a bounded native local-control-flow graph, while preserving an exact and rooted handoff to
  Brimstone's real VM on every unsupported or dynamically slow operation.
- Owner: Agent 2, JavaScript and WebAssembly runtime.
- Status: Conditional GO for this contained, off-by-default proof only. The frozen source passed
  the locked default, baseline-JIT, stress, strict-Clippy, release, rustdoc, and sanitizer gates
  recorded below. Product dispatch remains a compile-time false constant. DOM, untrusted-page
  execution, a browser baseline tier, and parity remain NO-GO.
- Upstream baseline: Brimstone `bfb720f0afb8b2b28b27c22ee7091deb7d16b082`; Cranelift `0.134.3`
  from exact Wasmtime v47.0.3 source at
  `5554cc1a651da536af2cc46c7324bdc085b162e3`.
- Wild Buzzard paths changed: `js/brimstone/src/js/runtime/jit/{abi,compiler,continuation,mod}.rs`.
  No manifest, dependency, product-dispatch, DOM, or browser-host path changed in this slice.

## Contract added

The private compiler now admits these exact local native families:

- immediate `undefined`, `null`, `Empty`, boolean, and signed integer loads, plus local `Mov`;
- SMI `Add`, `Sub`, `Mul`, bitwise operations, shifts, their immediate forms, `Neg`, `Inc`, `Dec`,
  and `BitNot`;
- SMI relational comparisons and strict equality, plus immediate-only `LogNot` fast paths;
- zero-capacity `NewObject`, preserving the earlier exact allocating safepoint contract;
- direct and rooted constant-backed jumps, exact-boolean branches, bounded `ToBoolean` branches,
  undefined branches, nullish branches, joins, loops, and `Ret`.

Admission checks the verifier's exact opcode/effect contract. A dynamically unsupported type,
integer overflow, negative zero, unsigned-right-shift result outside the SMI range, coercing case,
or unsupported opcode side-exits at the source instruction before mutating its destination. The
rooted real-VM continuation remains the authority for the general JavaScript semantics.

Entry slots are representation-checked but are not assumed to be ECMAScript values because
Brimstone carries internal heap metadata in the same pointer-shaped `Value`. A separate bounded
must-provenance analysis starts every entry slot as unknown, marks successful native JavaScript
producers, propagates `Mov`, and intersects at joins. An unproven `Ret` or undefined/nullish branch
accepts only a canonical non-`Empty` immediate natively; pointer-shaped and `Empty` values side-exit
to rooted VM admission, which distinguishes objects/strings/symbols/bigints from internal function,
realm, scope, and bytecode metadata. This prevents both native return and control-flow observation
of an internal pointer. The provenance analysis is fallible, shares the already charged liveness
matrix allocation, and caps worklist dequeues at 2,000,000.

## Native backedge and helper ABI

Generated-code ABI version 3 adds one private nonallocating backedge-poll helper beside the existing
allocating `NewObject` helper. Allocating safepoints and nonallocating poll calls use disjoint source
location ranges, and release validation requires every compiler-planned callsite to appear exactly
once in the emitted native callsite table.

Every taken nonpositive native edge performs this sequence:

1. publish its exact verified target in the activation;
2. call the versioned helper before entering another iteration;
3. consume one ordinary deterministic interrupt-budget unit;
4. either clear the publication and jump, or terminate distinctly as interrupt, policy side exit,
   or poison.

The helper validates the activation header, registered frame, quiescent safepoint publication,
helper table, context, and non-null private budget pointer; the activation owner revalidates the
exact owner identities after generated code returns. Rust panics are caught before they can cross
generated code. An independent hard limit of 1,000,000 taken native backedges applies to each
activation. The edge which consumes the final native unit is still polled first, then side-exits at
its already-published target; an ordinary interrupt on that edge has priority. Unsupported helper
status poisons and terminates rather than continuing.

Native reachability stops at the first mandatory side exit on each path. Bytecode after that exit is
owned by the rooted VM continuation and does not promise a native helper callsite or safepoint. This
matters because Cranelift may delete unreachable blocks: an unreachable `NewObject` or backedge may
not leave behind a native callsite which could satisfy metadata prepared from the bytecode alone.

The native/VM bridge tests cover exact semantic agreement for each admitted arithmetic, bitwise,
shift, unary, comparison, constant, move, and branch family. Slow-path differentials cover overflow,
negative zero, an unsigned shift requiring a double, non-immediate truthiness, and non-SMI strict
equality. A native loop allocates at a real safepoint on every iteration under forced moving GC,
retains a moved persistent string, and checks exact helper and backedge-poll counts. Separate tests
cover target-preserving policy side exit, quantum/external interruption, policy failure, helper
panic, exact parent-state cleanup, same-context/cache recovery, `Empty`/internal-pointer rejection,
and a compiler-proven moved object used by an undefined branch.

## Firefox reference evidence

Reference checkout: detached Firefox ESR153
`c19b7e89270787889495688244ec6ee8e79288a1`.

- `firefox/js/src/jit/BaselineCodeGen.cpp` around lines 1598-1618 synchronizes the frame, tests the
  runtime interrupt bits inline, and enters the VM interrupt helper only when the fast check is
  nonzero. Its callsite has a unique return address.
- The same file around lines 2537-2547 lowers `LoopHead` through a jump target, interrupt check, and
  warmup-counter update. Its ordinary boolean-branch lowering around line 2287 supplies the native
  label/control-flow comparison point.
- `firefox/js/src/jit/VMFunctions.cpp` around line 881 routes `InterruptCheck` through verifier and
  runtime interrupt handling; `JitFrames.cpp`/`JitFrames.h` remain the stack-map, frame tracing,
  and resume reference.
- History commit `db17034edd588bee0e9f43a491c4203975615533` (Bug 1598548 part 8) folded
  `LOOPENTRY` into `LOOPHEAD` while retaining loop interrupt/warmup behavior. Commit
  `bc60f1d2e34af58b7f7e7536b78cab6983401216` (part 7) assigned interrupt VM calls a unique
  return-address kind for debug-mode OSR.
- `firefox/js/src/jit-test/tests/ion/iloop.js` and `ion/timeout-iloop.js` assert that an otherwise
  infinite optimized loop remains watchdog-interruptible. Applicable shell interrupt-callback and
  GC regressions remain future differential inputs rather than copied tests.

## Evidence

All Cargo, rustc, rustdoc, Clippy, sanitizer, and test output stayed outside the repository under
`/home/user/Documents/wildbuzzardbuilds/w4-a2n/`. The stable toolchain was
`rustc 1.95.0 (59807616e 2026-04-14)` and `cargo 1.95.0 (f2d3ce0bd 2026-03-21)`. The sanitizer used
`rustc 1.98.0-nightly (6bdf43094 2026-06-01)` from `nightly-2026-06-02`, including its installed
`rust-src` component and a freshly instrumented standard library.

| Locked gate | Frozen-source result |
| --- | --- |
| default full workspace tests | 35 passed: 24 core plus 11 integration/snapshot; 0 failed |
| default workspace, strict all-target Clippy | passed with `-D warnings` |
| `baseline_jit` full workspace tests | 101 passed: 90 core/JIT plus 11 integration/snapshot; 0 failed |
| JIT filter with `baseline_jit,gc_stress_test,alloc_error,handle_stats` | 66 passed; 0 failed; 25 filtered |
| `baseline_jit` workspace and combined-feature package, strict all-target Clippy | both passed with `-D warnings` |
| release workspace check with `baseline_jit` | passed with `-D warnings` |
| warning-denied `brimstone_core` rustdoc with `baseline_jit` | passed with only the four inherited classes explicitly allowed below |
| nightly AddressSanitizer plus LeakSanitizer, forced moving GC, allocation errors, handle statistics, and `no_jemalloc` | 66 passed; 0 failed; no address or leak diagnostic |
| repository rustfmt, `git diff --check`, product-dispatch/feature, and in-repository artifact audits | passed |

Commands were run from the repository root. Each Cargo target was a distinct external directory:

```sh
cargo +1.95.0 fmt --manifest-path js/brimstone/Cargo.toml --all -- --check

CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w4-a2n/default CARGO_INCREMENTAL=0 \
  cargo +1.95.0 test --manifest-path js/brimstone/Cargo.toml --locked --workspace

CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w4-a2n/clippy-default CARGO_INCREMENTAL=0 \
  RUSTFLAGS=-Dwarnings cargo +1.95.0 clippy --manifest-path js/brimstone/Cargo.toml \
  --locked --workspace --all-targets

CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w4-a2n/baseline CARGO_INCREMENTAL=0 \
  cargo +1.95.0 test --manifest-path js/brimstone/Cargo.toml --locked --workspace \
  --features baseline_jit

CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w4-a2n/stress CARGO_INCREMENTAL=0 \
  cargo +1.95.0 test --manifest-path js/brimstone/Cargo.toml --locked -p brimstone_core \
  --features baseline_jit,gc_stress_test,alloc_error,handle_stats runtime::jit::

CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w4-a2n/clippy-baseline \
  CARGO_INCREMENTAL=0 RUSTFLAGS=-Dwarnings cargo +1.95.0 clippy \
  --manifest-path js/brimstone/Cargo.toml --locked --workspace --all-targets \
  --features baseline_jit

CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w4-a2n/clippy-baseline \
  CARGO_INCREMENTAL=0 RUSTFLAGS=-Dwarnings cargo +1.95.0 clippy \
  --manifest-path js/brimstone/Cargo.toml --locked -p brimstone_core --all-targets \
  --features baseline_jit,gc_stress_test,alloc_error,handle_stats

CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w4-a2n/release CARGO_INCREMENTAL=0 \
  RUSTFLAGS=-Dwarnings cargo +1.95.0 check --manifest-path js/brimstone/Cargo.toml \
  --locked --workspace --release --features baseline_jit

CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w4-a2n/rustdoc CARGO_INCREMENTAL=0 \
  RUSTDOCFLAGS='-D warnings -A rustdoc::broken_intra_doc_links \
  -A rustdoc::private_intra_doc_links -A rustdoc::bare_urls \
  -A rustdoc::invalid_html_tags' cargo +1.95.0 doc \
  --manifest-path js/brimstone/Cargo.toml --locked -p brimstone_core --no-deps \
  --features baseline_jit

ASAN_OPTIONS='detect_leaks=1:halt_on_error=1' \
  CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w4-a2n/asan \
  CARGO_INCREMENTAL=0 RUSTFLAGS='-Zsanitizer=address -Cdebuginfo=1' \
  RUSTDOCFLAGS='-Zsanitizer=address' cargo +nightly-2026-06-02 test -Zbuild-std \
  --manifest-path js/brimstone/Cargo.toml --locked -p brimstone_core --lib \
  --target x86_64-unknown-linux-gnu --no-default-features \
  --features baseline_jit,gc_stress_test,alloc_error,handle_stats,no_jemalloc runtime::jit::
```

The rustfmt configuration contains nightly-only keys; stable rustfmt reported that those keys were
ignored and returned success. Rustdoc denied every warning except the four inherited imported-code
classes named in the command: broken and private intra-doc links, bare URLs, and invalid HTML tags.
This is not represented as a strict full-workspace documentation gate. After evidence was recorded,
all completed W4-A2N output trees (`default`, `clippy-default`, `baseline`, `stress`,
`clippy-baseline`, `release`, `rustdoc`, and `asan`) were removed with exact-target `cargo clean`
commands to recover about 17 GiB; the recorded commands reproduce them without writing inside the
repository.

A pre-freeze baseline run exposed two metadata/reachability assumptions rather than being counted as
acceptance evidence. The first correction stops planning poll calls beyond a mandatory native side
exit. Static review then found the analogous unreachable-allocating-helper case; safepoint planning
now requires native reachability, and
`unreachable_allocating_helper_after_mandatory_side_exit_is_not_promised` locks that behavior down.
The complete matrix above was run after both corrections.

Final ordered source hashes:

```text
edd511d21476051e0a542450babe9de3f87eba6ae7efef46ce0d1a69e15d018e  js/brimstone/src/js/runtime/jit/abi.rs
89bf465848881c114c7512e315094f3c41fdf89a60567e8f1d02be773b778ea0  js/brimstone/src/js/runtime/jit/compiler.rs
1ded09571576fe9de9fd181a49bfd3d8c9369b7f495778ba6736441ac6dba218  js/brimstone/src/js/runtime/jit/continuation.rs
2e862a8ed29374b85b67606ca424caf358c1af2908f5065792307a474d46e2b5  js/brimstone/src/js/runtime/jit/mod.rs
```

An independent hostile read-only review reproduced all four frozen hashes and found no Critical,
High, Medium, or Low correctness defect in the admitted scope. It specifically rechecked native
and slow-exit semantics, conservative provenance through loops/joins, target-before-poll ordering,
interrupt precedence, exact allocating/poll callsite accounting, moving-GC reloads, panic/poison
termination, rooted VM cleanup, mandatory-side-exit reachability, and compile-time-disabled product
dispatch. The reviewer did not run another Cargo/rustc gate because the frozen owner matrix above
was already complete and the serialized build lease belonged to another workstream.

## Boundaries and next work

`baseline_jit` remains off by default and `PRODUCT_DISPATCH_ENABLED` remains compile-time `false`.
No DOM or untrusted content can enter this code. This is not normal hot-function dispatch, a
production baseline tier, or performance parity.

Calls, properties, caches, parameters, noninitial realms, runtime functions, handled exceptions,
OSR, deoptimization, debugger/unwind metadata, invalidation, complete native stack maps, an
optimizing tier, browser resource policy, the remaining lifetime migration, Test262, fuzzing, and
browser integration remain open. The analysis limits count dequeues, not every local-cell scan, so
they are not an untrusted-bytecode CPU bound.

The per-backedge Rust helper is an intentional correctness-gate debt. Firefox keeps the ordinary
zero-interrupt path inline and calls out only when an interrupt bit is set; Wild Buzzard currently
pays a Rust helper call on every taken native backedge. The next gate should define a stable C-layout
inline fast-poll state that preserves Rust ownership/atomic-lifetime safety, keeps exact per-edge
poll-count regressions, and enters Rust only for slow interrupt or policy handling. Do not enable
product dispatch on the strength of this contained implementation.
