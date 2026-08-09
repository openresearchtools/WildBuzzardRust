# W2-A2K contained Brimstone GC-linked helper and continuation proof

- Task: Extend the disabled Brimstone/Cranelift infrastructure gate with the first allocating
  generated operation, a moving-GC-visible native frame, and an exact contained continuation
  without creating a product dispatch path.
- Owner: Agent 2 — JavaScript/WebAssembly; independently reviewed and integrated by the main
  orchestrator.
- Status: Complete for the contained, off-by-default proof. NO-GO for product VM dispatch, normal
  interpreter integration, DOM or untrusted content, or baseline/optimizing-tier parity.
- Upstream baselines: Brimstone
  `bfb720f0afb8b2b28b27c22ee7091deb7d16b082`; exact Cranelift `0.134.3` source from imported
  Wasmtime v47.0.3 revision `5554cc1a651da536af2cc46c7324bdc085b162e3`.
- Firefox reference: ESR153 `c19b7e89270787889495688244ec6ee8e79288a1`. SpiderMonkey's
  baseline frames, exit frames, safepoints, stack maps, rooting, helper calls, invalidation, and
  jit-tests remain behavioral and architectural reference only; no SpiderMonkey code was copied.
- Wild Buzzard paths changed: Brimstone runtime context/root traversal, the feature-gated JIT ABI,
  executable cache, baseline compiler, contained continuation, JIT/runtime module exports, and the
  Brimstone provenance note. No manifest, lockfile, dependency, root-workspace, or product-dispatch
  path changed.

## Artifact and execution contract

`compile_prototype` creates one `PreparedPrototype` containing relocation-free machine bytes,
checked safepoint metadata, and an owned `PreparedProgram`. The program copies both the raw bytecode
and the verifier's exact decoded instructions, including widths, effects, operands, instruction
boundaries, and resolved constant-backed branch targets which cannot be recovered from bytes alone.
The executable cache consumes that whole value and privately constructs a `LoadedPrototype` which
owns its RX mapping and prepared data together. The safe contained runner accepts only a borrowed
loaded prototype; it cannot accept raw executable bytes, independent maps, or another decoded
program. That cache borrow also prevents replacement, eviction, or unmapping during execution.

The executable cache remains owner-thread-only, hard bounded by mapped bytes and entry count, and
transitions each mapping from RW to RX without an RWX phase. The production mapper is module-private
and reachable by safe execution only through compiler-prepared cache insertion; the arbitrary-byte
convenience constructor and its callers are test-only. Direct generated entry remains an unsafe
internal ABI probe whose contract requires the activation to use that loaded artifact's metadata;
the safe runner establishes this automatically.

## Native roots and value validity

A higher-ranked `JitContextScope` grants non-escaping authority over one owned context. A
thread-affine `ActivationOwner` validates its frame and every slot before publishing an intrusive
frame-head link, bounds nesting to 64 activations, and restores the exact previous head in LIFO
order on ordinary return and unwind. Context destruction aborts if a native frame remains linked.

Native slots are opaque `#[repr(transparent)] JitSlot(Value)` cells. Their field is private; there
is no safe raw-word constructor, setter, mutable raw accessor, `Default`, or `From<u64>`. General
construction validates canonical immediates and requires every pointer to equal an actual item
start in that exact context's permanent heap or currently used moving heap. Activation creation
repeats the validation before linkage, closing the interval between slot construction and entry.
The checks reject forged address 1, noncanonical tagged values, aligned interior pointers,
foreign-context pointers, and pointers left stale by an intervening moving collection. Native and
continued return values receive the same representation/context validation before their bits are
accepted.

This exact-start scan is a contained proof mechanism, not the scalable product design. Brimstone's
NaN-boxed pointer carries no allocation generation, so a theoretical stale address reused for a
later allocation cannot be distinguished by pointer bits alone. The remaining lifetime-free raw
internal values and handles must be migrated before untrusted exposure; product code will need
stable rooted activation inputs rather than a full-heap scan at every entry.

## Safepoint, helper, and continuation

The compiler performs bounded CFG liveness and admits exactly one allocating opcode shape:
zero-argument `NewObject`. Before its indirect System V helper call, generated code publishes the
verified bytecode offset and immutable safepoint index. The safepoint record contains the checked
Cranelift call-return PC and a strictly sorted slice of exact live slots; the destination is not
live until the helper finishes. Brimstone's existing root visitor walks only those published live
slots and rewrites them after moving collection. Generated code then reloads slot values rather
than retaining moving pointers in native temporaries.

The versioned helper validates the activation, exact context head, frame schema, safepoint record,
and result index; polls deterministic interruption before allocating; establishes the initial realm
used by the interpreter's ordinary-object construction; roots the result; and stores it only after
forced collection is complete. Allocation failure and interruption are terminal. A caught helper
panic poisons the activation, and no Rust unwind crosses generated code.

Ordinary side exits may enter only a tiny checked continuation which borrows the loaded artifact's
captured decoded program. It accepts an exact verified instruction boundary and implements only
numeric `Neg` followed by `Ret`. It does not allocate in Brimstone's moving JavaScript heap and
cannot replay the preceding allocating instruction. Unsupported operations and backedges fail
closed. This is not a Brimstone VM frame, does not carry rooted function/bytecode identity, and is
not general interpreter resume.

`PRODUCT_DISPATCH_ENABLED` remains a compile-time false constant. No VM, browser engine, DOM
binding, or untrusted input can reach this gate.

## Verification evidence

All builds and tests ran outside the repository under
`/home/user/Documents/wildbuzzardbuilds/w2-a2k-soundness-*`:

- frozen-source debug `baseline_jit` library tests: 54 passed, 0 failed;
- frozen-source `baseline_jit,gc_stress_test` library tests: 55 passed, 0 failed;
- frozen-source release `baseline_jit` library tests with default features disabled: 54 passed,
  0 failed;
- nightly `2026-06-02` AddressSanitizer plus LeakSanitizer with `no_jemalloc`: 54 passed, 0 failed,
  with no final address or leak diagnostic;
- default workspace tests: 23 core plus 11 integration/snapshot tests passed;
- strict default and `baseline_jit` workspace/all-target Clippy with warnings denied: passed;
- explicit changed-file rustfmt on the dated nightly, `git diff --check`, feature-off dependency,
  exact-local-Cranelift, prohibited-pattern, and repository-artifact audits: passed.

The selected integration manifest used the external Test262 revision
`227d905513f790dec90858d04ddf8cf81326706f` and passed 185/0/0 on the frozen source. An earlier
invocation without the required external Test262 assets ran zero cases and reported missing
prerequisites; it is recorded here and is not counted as evidence. The refreshed baseline's prior
collect-on-every-allocation harness passed 179/0 with six explicitly configured stress skips. These
selected harnesses preserve known-green behavior; they are not a full Test262 conformance result
and cannot exercise product JIT dispatch while that dispatch is disabled.

Strict `RUSTDOCFLAGS="-D warnings"` over the imported workspace remains blocked by extensive
pre-existing broken-link, bare-URL, invalid-HTML, and private-link warnings. With exactly those four
imported-baseline lint classes allowed, `brimstone_core` documentation with `baseline_jit` passed
with every other warning denied. This is recorded as baseline documentation debt, not reported as a
strict workspace rustdoc pass.

Independent frozen-diff review returned GO for this contained proof only, with no high- or
medium-severity finding, and retained NO-GO for product VM/browser/DOM dispatch and untrusted
content. The reviewer independently repeated the 54-test debug run, 55-test forced-GC run,
54-test release run, 54-test sanitizer run, strict all-target baseline-JIT Clippy, exact-file
rustfmt, and diff checks under `/home/user/Documents/wildbuzzardbuilds/w2-a2k-final-review`.
The review covered artifact inseparability, exact program capture, slot/context validity,
activation/root lifetimes, CFG liveness and native-PC maps, forced collection, helper failure
containment, continuation offsets, W^X/cache lifetime, and the disabled product boundary.

Two low-severity wording findings were corrected: continuation does not allocate in Brimstone's
moving heap but final return validation may fallibly reserve host `Vec` bookkeeping, and the
production raw-byte mapper is module-private rather than test-only while its arbitrary-byte
convenience constructor/callers are test-only. Remaining non-blocking regression gaps are a
two-`NewObject` native-PC/map test, forced GC while overwriting an existing moving-pointer
destination, and wide/extra-wide `Neg` continuation-resume coverage.

## Scope, provenance, and next work

The proof establishes one GC-linked generated helper and one exact contained continuation. It does
not establish site performance, broad ECMAScript behavior, normal VM tiering, rooted continuation
identity, exceptions, backedge execution, calls, property caches, deoptimization, OSR,
debugger/unwind support, invalidation, asynchronous interruption, precise production native stack
maps, browser resource limits, full conformance, Firefox parity, or untrusted-content safety.

No dependency or license changed. Brimstone remains MIT; Cranelift remains Apache-2.0 WITH LLVM
exception. No runtime endpoint, provider integration, WASI capability, branding, or telemetry was
introduced. Cargo and the external conformance harness retain their separately recorded development
network/source requirements.

Next, give side exits rooted function/bytecode identity and integrate them with the actual
interpreter frame/exception model. Expand allocating helpers one semantic family at a time under
forced collection, then add backedge polling, invalidation, debugger/unwind metadata, property
guards, and product-owned dispatch only after separate review. The internal lifetime migration,
limits, fuzz/Miri where applicable, full Test262, browser DOM bindings, and Wasmtime cross-heap
rooting remain parallel blockers.
