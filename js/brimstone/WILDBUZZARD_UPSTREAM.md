# Brimstone upstream snapshot

This directory was imported as an exact source snapshot of Brimstone and is now the editable
Wild Buzzard adaptation tree. Git commit `4063038` preserves the original byte-identical import at
`b544eff`; a later freshness refresh applies the exact one-commit upstream delta to the current pin,
and other commits carry the separately reviewable local patches listed below. It is the pinned
baseline for the JavaScript execution-engine workstream, but it is not yet wired into the root
workspace or accepted as a production-safe browser runtime.

- Upstream repository: <https://github.com/Hans-Halverson/brimstone>
- Upstream default branch at import: `master`
- Commit: `bfb720f0afb8b2b28b27c22ee7091deb7d16b082`
- Commit date: `2026-08-09T02:56:40+00:00`
- Git tree: `6c1221d72cff1dcec5fdb6cd10d329abb935f126`
- Upstream tracked files: 1,555
- License: MIT (`LICENSE`)
- Import method: `git archive` of the original pin, followed by the exact reviewed upstream delta
- Original exact-import Wild Buzzard commit: `4063038` (`b544eff`)
- Current upstream refresh: exact `b544eff..bfb720f` one-commit/four-path delta, independently
  byte-checked against a clean non-shallow upstream checkout which is now detached at the pin
- Local adaptation patches: see below and `docs/upstream-components.toml`

The original exact-import commit contains 1,554 byte-identical upstream files plus this note. The
current source baseline has 1,555 upstream files; its refresh adds primitive-receiver prototype
cache handling and the matching integration test. Build outputs, the upstream Git repository,
generated test indices, and the separately cloned Test262 suite remain excluded. Use
`git diff 4063038 -- js/brimstone` together with this note to audit the upstream refresh and all
local adaptation.

Current local adaptation:

- exactly-once, thread-affine `OwnedContext` plus lifetime-branded moving-GC root scopes;
- hidden explicit unsafe aliases for legacy raw context, handle, and heap-pointer APIs;
- corrected `HeapInfo` initialization, destruction, and ownership transfer during heap resize; and
- a drop-free scope-tree option bit which avoids leaking an `Rc<Options>` into its bump arena;
- exhaustive opcode metadata and a bounded defense-in-depth verifier for trusted compiler output;
- versioned, lifetime-branded generated-code ABI schemas plus deterministic hotness/interrupt
  primitives; and
- an off-by-default Linux x86-64 W^X cache and boxed Cranelift proof using exact local Cranelift
  `0.134.3` source from Wasmtime v47.0.3; and
- a context-registered, lifetime-branded native root frame, bounded immutable safepoint maps, one
  zero-capacity `NewObject` helper through a versioned C ABI, and a tiny checked continuation proof;
- a compiler-created `PreparedPrototype` consumed into one cache-owned `LoadedPrototype`, which
  keeps RX bytes, maps, and the exact captured decoded program (including resolved constant-branch
  targets) inseparable for its complete synchronous call; and
- opaque initialized JIT slots whose representations and exact current-context heap-item starts are
  checked before frame-head publication, updated only by audited generated/helper/collector paths,
  and checked again before a native return is accepted; and
- a private actual-VM continuation admitted by a bounded monotone CFG/type proof for local
  moves/immediates, valid-JS `LogNot`/`TypeOf`, number-only arithmetic/comparisons, exact boolean,
  `ToBoolean`, undefined and nullish branches, joins, loops, `Ret`, and uncaught terminal `Throw`;
- bounded native Cranelift generation for immediate/move and SMI arithmetic, bitwise, shift, unary,
  comparison, exact branch, join, loop, `NewObject`, and return families with slow operations
  rooted-side-exiting before destination mutation; and
- generated-code ABI version 3 with an exact nonallocating backedge-poll helper, distinct allocating
  safepoint/poll callsite ranges, native-reachability accounting, a one-million-edge activation cap,
  and conservative must-provenance analysis which keeps `Empty` and internal pointer-shaped VM
  metadata out of native JavaScript return and control-flow decisions.

The `baseline_jit` feature remains outside product dispatch. Its sole allocating helper is forced-GC
tested with exact compiler-derived live slots and a rooted result; unsupported operations still
side-exit before execution. The safe runner cannot independently select executable bytes, maps, or
decoded semantics. W3-A2M broadens only the private actual-VM side-exit continuation: analysis is
fallible, models at most 32 MiB, caps worklist dequeues at 2,000,000, follows both conditional
successors, rejects consumers of `Empty`/internal values, and verifies/publishes every taken
nonpositive edge before an interrupt poll. The cyclic subset is nonallocating; an uncaught terminal
`Throw` may allocate only after publishing its exact PC and cannot reach another edge because
handler tables remain rejected. The private dispatch disables comparison fusion so a backedge
cannot skip its poll and restores exact parent VM state on return, throw, interruption, policy
failure, allocation failure, and panic. W4-A2N advances the generated side of the same disabled
proof. Every native nonpositive edge publishes its exact target and polls before another
iteration; slow or coercing semantics return to the rooted actual-VM continuation. Forced-moving-GC
native loops and the complete frozen source passed strict, release, warning-denied core-rustdoc,
AddressSanitizer, and LeakSanitizer gates.

This breadth is contained native-generation evidence, not a browser JIT. Calls, properties,
parameters, caches, handled exceptions, noninitial realms, normal hot dispatch, OSR,
deoptimization, debugger/unwind integration, complete stack maps, optimizing compilation, and an
untrusted-bytecode contract remain absent. The work caps count CFG dequeues or taken backedges, not
all interpreter work. Product dispatch stays compile-time false. Shared dependency resolution
aligns to the selected Wasmtime lock and advances compatible Brimstone packages to bumpalo 3.20.2,
libc 0.2.185, log 0.4.28, and smallvec 1.15.1. See
`docs/handoffs/W4-A2N-native-jit-cfg.md` for exact tests and hashes.

Upstream explicitly labels Brimstone as a work in progress and not ready for production. In
particular, Wild Buzzard must not expose the remaining raw context, handle, or heap-pointer APIs
across a browser boundary. W2-A2H is a conditional GO for contained JIT infrastructure work only;
untrusted content remains blocked on the deeper internal lifetime migration, limits, interruption,
host bindings, fuzzing/Miri where applicable, and all gates in `AGENTS.md`.
