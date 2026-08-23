# W9-A2W — Array native-loop interruption

## Verdict and bounded outcome

Self-review recommendation: **GO for the bounded `Array.prototype.fill` runtime correction**.
This is **not** a GO for untrusted-content admission, all native builtins, product dispatch,
Firefox parity, or YouTube parity.

The Rust-native implementation of `Array.prototype.fill` now checks the one existing Brimstone
browser-script policy authority:

- after `ToObject`, length, start, and end coercion and before the first `Set` effect; and
- before each subsequent chunk of at most 1,024 successful `Set` operations.

It does not create a second timer, deadline, cancellation flag, exception, or fast property path.
Every element still goes through the existing specification-facing `set(..., Throw = true)`
operation. An interruption therefore leaves exactly the successful prefix preceding the poll.
A synchronous accessor/proxy exception still wins at the exact `Set` which throws, without a
later poll replacing it.

Changed paths:

- `js/brimstone/src/js/runtime/intrinsics/array_prototype.rs`
- `docs/handoffs/W9-A2W-array-native-loop-interrupt.md`

No manifest, lock, parser, DOM, browser, host-admission, context, task, GC, handle, pointer, heap,
Firefox-reference, or product-dispatch path was changed.

## Reproduced defect and corrected behavior

W9-A3PQ-R1's fixed-heap case used a 64 MiB heap, three
`new Array(2_000_000).fill(...)` operations, a 256 MiB managed-allocation allowance, and a
30-second wall deadline. Before W9-A2W it returned `Interrupted(Deadline)` only after 88–89
seconds because the native fill loop had no policy poll.

The copied core-runtime reproducer now reports:

```text
HOSTILE_FIXED_HEAP_OUTCOME=Interrupted(Deadline)
HOSTILE_FIXED_HEAP_ELAPSED=30.041511901s
WATCHDOG_ELAPSED=30.54
WATCHDOG_EXIT=0
```

The subprocess had a 42-second external kill boundary. Evidence is in
`../wildbuzzardbuilds/w9-a2w-array-poll/logs/hostile-watchdog-final.log`.

The original host-integrated test also reaches `Interrupted(Deadline)` in 30.06 seconds and the
42-second watchdog does not fire. Its old assertion still requires
`ResourceLimit(EngineAllocation)`, so that test intentionally reports failure at
`dom/script_bridge/tests/rooted_task.rs`. W9-A2W was forbidden to edit DOM code/tests. The DOM
owner must update that stale expected terminal while retaining the existing host-discard,
successful-DOM-prefix, pending-job retirement, and stale-task assertions. Evidence is in
`logs/original-hostile-watchdog.log`.

## Ordering and adversarial coverage

Four focused tests run in both debug and release builds:

1. Dense and sparse arrays preserve ordinary fill behavior.
2. Length/start/end coercion, accessor invocation, Proxy `set` order, thrown-value identity, and
   the exact successful prefix before an abrupt completion are preserved.
3. A requester on another Rust thread publishes through `ScriptInterruptHandle` at the first
   fill poll. Length/start/end coercion effects remain; no `Set` occurs.
4. A requester at the third poll leaves exactly 2,048 successful elements, and a deadline at the
   second poll leaves exactly 1,024 successful elements. The selected poll blocks only in
   `cfg(test)` to make the external request/deadline race deterministic; production polling uses
   the unchanged browser-script authority directly.

No test hook, timing state, or requester is compiled into non-test builds.

## Firefox and specification reference

The pinned Firefox reference remains detached and clean at
`c19b7e89270787889495688244ec6ee8e79288a1`.

- `firefox/js/src/builtin/Array.js:483` implements ordinary `ArrayFill` as a JavaScript loop, so
  interpreter/JIT backedge interruption applies naturally.
- `firefox/js/src/vm/JSContext-inl.h:270` documents the inline `CheckForInterrupt` fast path for
  hot C++ builtin loops.
- `firefox/js/src/builtin/Array.cpp` contains explicit `CheckForInterrupt` calls throughout native
  Array loops.
- `firefox/js/src/jsapi-tests/testSlowScript.cpp` exercises externally requested interruption at
  immediate and delayed loop boundaries.

The Rust implementation remains architecture-independent at the algorithm layer and adds no
Firefox build or runtime dependency.

## Verification

All commands used the Data-drive Podman wrapper, `--network none`, read-only source for build/test
gates, a read-only Firefox overlay where the source mount was writable for the one-file rustfmt
operation, Rust 1.95.0, and Cargo/target/temp/log storage under
`../wildbuzzardbuilds/w9-a2w-array-poll/`.

| Gate | Result |
|---|---:|
| Focused debug | 4 passed |
| Core default | 72 passed |
| Core `alloc_error` | 75 passed, 1 explicitly ignored hostile test |
| Core forced moving GC | 76 passed |
| Core handle statistics | 72 passed |
| Combined baseline JIT + OOM + GC + handle statistics | 188 passed, 1 explicitly ignored hostile test |
| Explicit hostile fixed-heap watchdog | 1 passed in 30.06s; process 30.54s |
| Focused release | 4 passed |
| Strict combined Clippy (`-D warnings`, all targets) | passed |
| Combined release check (all targets) | passed |
| One-file rustfmt check and scoped `git diff --check` | passed |

The raw workspace rustdoc `-D warnings` gate is blocked by inherited documentation in untouched
files (`bare_urls`, broken intra-doc links, and invalid HTML tags). Rustdoc passes with
`-D warnings` after explicitly allowing only those three pre-existing workspace-wide lint
classes; W9-A2W introduces none of those diagnostics. Both the raw failure and bounded passing
gate are retained in `logs/rustdoc-combined-r2.log` and `logs/rustdoc-combined-r3.log`.

## Adjacent native-loop audit

W9-A2W fixes only ordinary `Array.prototype.fill`. The audit found substantial residual
unpolled-native-loop debt:

- In `array_prototype.rs`: spread `concat`; both `copyWithin` directions; sparse scans in
  `every`, `filter`, `forEach`, `some`, and `find*`; recursive `flat`/`flatMap`; `includes`,
  `indexOf`, `lastIndexOf`, `join`, `map`, `reduce*`, `reverse`, `shift`, `slice`, `sort`,
  `splice`, `toLocaleString`, `toReversed`, `toSorted`, `toSpliced`, `unshift`, `with`, and the
  indexed-property gather/merge-sort helpers.
- In `typed_array_prototype.rs`: `TypedArray.prototype.fill` and the copy/search/map/reduce/
  reverse/set/sort/toReversed/toSorted/with loops, including byte-copy loops.

Some callback paths re-enter bytecode and receive VM opcode polls, and some allocation paths poll
through managed-allocation admission. Neither is a proof for sparse scans, already-reserved dense
storage, native byte copies, or callback-free iterations. These loops need follow-up work with
operation-specific prefix/exception/detachment/shared-memory tests. They were not changed because
that would expand W9-A2W beyond the exact fill contract and test surface.

## Frozen source identity

`array_prototype.rs` SHA-256:

```text
112b53e9af6939dbb210df65573afbabfeb9915bfbf51a934c2605c87aedada0
```

The seven frozen W9-A2V-C2 paths retained their accepted hashes exactly; see
`../wildbuzzardbuilds/w9-a2w-array-poll/evidence/frozen-w9-a2v-c2-sha256.txt`.

## Residual product limits

`PRODUCT_DISPATCH_ENABLED` remains compile-time false and `baseline_jit` remains nondefault.
This correction does not enable DOM or untrusted-page entry and does not close the broader parser,
host-binding, native-loop, resource-accounting, Test262, fuzzing, sanitizer, browser-integration,
AppImage, or live-site gates.
