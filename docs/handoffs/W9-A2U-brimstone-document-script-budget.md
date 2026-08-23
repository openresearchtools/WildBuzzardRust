# W9-A2V-C2: exact JS-owner provenance, TLS-safe retirement, and one-way host disposition

- Owner: Agent 2 — JavaScript/WebAssembly/GC/rooting
- Accepted task source base: `076242d20aaf554bc313a2ef4cf52d91d878b3bd`
- Verification worktree `HEAD`: `5739fa22359919b86a4bda4771fd6ac367592884`, a descendant
  advanced by other component lanes while this task was active
- JS-path delta between those two committed revisions: none (`git diff --quiet` returned 0 for
  all seven runtime paths owned by this task)
- Firefox ESR153 reference: `c19b7e89270787889495688244ec6ee8e79288a1`
- Product admission: **NO-GO for untrusted/general-web content**

## Decision

The three W9-A2V-C2 HIGH findings are closed for the lifetime-branded, disabled document-session
coordinator:

1. every context-owned live root creation, replacement, cast, and escape validates the exact live
   heap owner of pointer-bearing contents before allocating or writing a handle-stack slot;
2. heap-authority registration remains available during arbitrary caller TLS destruction order and
   retires each exact range identity once; and
3. installed-host completion and discard use a one-way disposition state, with the last fallible
   policy poll before the irreversible callback and no post-disposition relabel or second abort.

Recommendation: **GO for the later disabled parser coordinator to use the branded browser API**.
This remains **NO-GO for legacy raw-token embedding, product dispatch, DOM/untrusted entry,
Firefox parity, or a YouTube claim**. `baseline_jit` remains nondefault and
`PRODUCT_DISPATCH_ENABLED` remains the literal compile-time `false`.

The recommendation is deliberately narrow. Brimstone's legacy `RawContext`, `RawHandle`, and
lifetime-free `HeapPtr` surface is still an unsafe quarantine, not a generally sound public
embedding API. The browser coordinator neither exposes nor accepts those tokens.

## Exact live-root owner provenance

`Handle<T>` carries two words: its existing pointer to one handle-stack slot and an
`Option<Context>` naming the exact owner of that live root. The same owner identity is retained by
casts and checked against the exact active handle-stack slot before mutation.

For context-owned handles:

- `Handle::new` validates the owner and incoming encoded contents before allocating or writing a
  slot;
- `Handle::replace` first validates the destination owner/current slot, then validates the new
  `Value` or `HeapPtr` owner, and writes only after both checks succeed;
- `Value::to_handle(cx)` rejects a foreign, stale, unregistered, or uninitialized pointer-bearing
  value while immediates remain valid;
- `HeapPtr<T>::to_handle` recovers its exact live-range owner, while internal `to_handle_in(cx)`
  additionally requires identity equality;
- `Handle::cast` validates the current encoded contents before preserving the same owner;
- every `Escapable` implementation for `Value`, `HeapPtr`, typed/value handles, `Option`, `Result`,
  and tuples validates while the child scope is still active, then reroots only into the same
  owner; and
- moving-GC root scanning revalidates every active slot before following it.

These checks are ordinary release assertions, not `debug_assert!`. Rejection happens before root
allocation or slot mutation. The hostile regression covers owner-B `Value` into owner A, owner-B
`HeapPtr<Realm>` into an A root, direct replacement, same/cross-owner escape, cast of an injected
foreign slot, forged unregistered bits, stale pointers, collection of both owners, unchanged slots,
and subsequent owner-A recovery.

### Non-root and serializer paths

Pointer-shaped bytecode metadata is intentionally not a JavaScript heap pointer. Its existing
crate-private `from_fixed_non_heap_ptr` path now creates an ownerless, read-only view which is not
inserted into the moving root stack. Mutation through that view is rejected. This distinction is
required by constant-table and snapshot generation and is covered by the full workspace and a
focused regression.

Live-root admission is separate from detached serializer traversal:

- `HeapInfo::live_owner_for_root` accepts only an exact registered, initialized, live heap range;
- `HeapInfo::assert_pointer_access_authorized` is the internal access-only path used while copied
  serializer bytes are traversed; and
- no detached serializer pointer can pass `Value::to_handle`, `HeapPtr::to_handle`, handle
  replacement, escape, or root scanning.

The access-only path does not make arbitrary legacy `HeapPtr` construction sound. A caller which
forges raw pointer bits or mutates a legacy raw handle after obtaining unsafe authority remains
outside the branded browser guarantee. Product admission requires removing that legacy surface or
replacing it with scoped typed serializer authority.

## Teardown-safe exact heap authority

Each aligned heap allocation receives a monotone, never-reused `u64` registration identity. The
owner-thread registry stores exact start/end/registration/owner tuples. A range is registered before
objects exist, bound once to the exact `Context`, and unregistered by exact tuple before allocator
reuse. Resize to-space is bound to the same owner before collector traversal; metadata transfer
aborts on owner mismatch.

The registry TLS key is a destructor-free `Cell` pointing to a deliberately process-lifetime
`RefCell<Vec<HeapAuthorityRange>>`. This makes registry lookup and exact unregistration available
even when a caller's `thread_local! OwnedContext` initialized before Brimstone's key and therefore
drops after ordinary destructible TLS values. Missing TLS access, borrow reentry, overlap, duplicate
binding, duplicate/missing retirement, and registration exhaustion remain fail-closed aborts; a
missing registry is never interpreted as serializer permission.

The repeated regression creates 1, 2, 4, and 8 nested safe `OwnedContext` values in caller TLS on
fresh threads, exits the threads normally, and compares exact registration and retirement ID sets
after join. It passes with default and `baseline_jit` builds in debug/release and under ASan. The
original reviewer process-exit probe also exits normally in both debug and release.

## One-way host phase disposition

Every direct and already-installed classic/checkpoint host path now owns a local
`HostPhaseDisposition` with these states:

```text
Armed -> Finishing -> Disposed
                   -> FinishRejected -> Aborting -> Disposed
Armed --------------------------------> Aborting -> Disposed
```

Any impossible transition aborts. Panic while `finish_phase` or `abort_phase` is in flight aborts
the process because completion cannot be proved. A returned `finish_phase` success is permanently
`Disposed`; cleanup cannot relabel it or call abort. A returned finish error permits exactly one
documented abort fallback. A returned abort is permanently `Disposed`.

All cancellation, deadline, resource, task, VM, and host validation polls occur before beginning
the irreversible disposition. Cancellation or deadline requested from inside a successful finish
is observed only by the next document/session poll: the completed phase remains `Completed` and is
never aborted. Cancellation or deadline requested inside abort cannot replace the phase's already
selected failure/discard terminal and cannot trigger a second retirement.

Four regressions cover cancellation and deadline independently for classic finish, checkpoint
finish, classic abort, and checkpoint abort. Each proves one exact callback count, exact
`Completed` or `Discarded` phase outcome, no second retirement, stable document terminal
precedence, and subsequent safe recovery. Existing host-B substitution rejection, host panic
poison, and bounded `SIGABRT` abort-callback-panic evidence remain green.

## Preserved document-session contract

W9-A2V-C2 preserves the accepted W9-A2U/W9-A2V behavior:

- one owner thread, one `OwnedContext`, one initial realm, one document generation, and when hosted
  one exclusive host/navigation authority for the whole session;
- document-cumulative checked monotone candidate, source, opcode, managed-allocation, job,
  diagnostic, recursion, and wall-time accounting;
- recursion hard maximum 256, with 257 and 512 rejected before callback execution;
- exact pending-task cap installed before document execution, no allocation/push beyond the cap,
  monotone overflow, bounded retirement, and no cross-budget queued-job escape;
- callback/engine/host panic, OOM, poison, cancellation, deadline, `JobThrown`, and
  `PendingJobsAtDocumentExit` fail-closed behavior;
- first terminal reason stable across classic/checkpoint/close observation; and
- legacy direct-entry `RuntimeBusy` remains a nonterminal rejection, not a latched terminal.

The two fixed-heap OOM regressions now serialize only their deliberate saturation sections and use
an 8 MiB fixed heap with proportionally smaller arrays. This retains the same engine-allocation
failure/recovery proof while avoiding host-load-dependent deadline failures from concurrently
filling two 64 MiB heaps.

## ABI, allocation, and performance impact

The x86-64 size regression proves:

| Type | Size / alignment |
| --- | --- |
| `Context` | 8 / 8 bytes |
| `Option<Context>` | 8 / 8 bytes |
| `HeapPtr<ObjectValue>` | 8 / 8 bytes |
| `Handle<Value>` | 16 / 8 bytes |

`Handle` therefore remains two words after C1/C2, while each moving root-stack
`HandleContents` slot remains one 8-byte word. `HeapPtr`, VM/JIT slots, and generated-code ABI
layouts are unchanged. `Heap` gains one internal `u64` registration identity (8 bytes on x86-64);
`HeapInfo` and serialized heap headers do not gain that field, and the serialized-heap package plus
snapshot suites pass.

Runtime costs are correctness-first:

- one owner word per Rust handle wrapper/copy;
- owner/poison and active-slot checks on handle access/mutation/cast/reroot;
- root provenance validation during GC root scanning;
- a TLS `RefCell` borrow and linear range lookup for live pointer/root authority (normally one
  range, transiently two during resize); and
- one intentionally leaked small registry allocation per owner thread, retaining the range vector's
  high-water capacity after all live entries retire.

Registry allocation/lookup CPU and retained capacity are not charged to the document budget. This
must be benchmarked and replaced or optimized during the lifetime/root migration before product
admission. ASan ran with leak detection disabled only because this process-lifetime registry
allocation is intentional and documented; address and use-after-free detection remained fatal.

## Hostile regressions and reviewer probes

New C2 evidence includes:

- `foreign_stale_and_unregistered_heap_contents_never_enter_or_rewrite_live_roots`;
- `caller_tls_owner_drops_after_destructor_free_registry_and_retires_each_range_once`;
- `classic_finish_disposition_survives_callback_cancellation_and_deadline`;
- `checkpoint_finish_disposition_survives_callback_cancellation_and_deadline`;
- `classic_abort_disposition_survives_callback_cancellation_and_deadline`;
- `checkpoint_abort_disposition_survives_callback_cancellation_and_deadline`; and
- strengthened `fixed_legacy_handle_is_read_only` coverage for pointer-shaped non-root metadata.

The original independent probes were copied unchanged into this task's Data lane:

- cross-owner probe: debug exit 101 and release exit 101 at the first owner-B `Value` root attempt,
  with `heap-bearing root value belongs to a different Brimstone owner`;
- caller-TLS teardown probe: debug exit 0 and release exit 0, with normal process destruction.

Five external compile-fail probes still prove API shape: hosted checkpoint host replacement
`E0061`, poison reset absence `E0599`, unsafe raw-context acquisition `E0133`, rooted lifetime escape
`E0521`, and private browser-realm raw field `E0616`.

## Data-drive verification

Every container invocation used only:

```text
/run/media/user/Data/Repositories/wildbuzzardbuilds/podman/podman-wildbuzzard ...
```

Container `w9-a2v-c2-builder` was network-disabled. All Cargo homes, targets, temporary files,
probe sources/binaries, sanitizer artifacts, and logs are under:

```text
/run/media/user/Data/Repositories/wildbuzzardbuilds/w9-a2v-c2-js-owner-provenance/
```

Toolchain: Rust 1.95.0, target `x86_64-unknown-linux-gnu`, LLVM 22.1.2. Representative commands:

```sh
cargo test --locked --offline --workspace
cargo test --locked --offline -p brimstone_core --lib --features baseline_jit
cargo test --locked --offline -p brimstone_core --lib --features alloc_error
cargo test --locked --offline -p brimstone_core --lib --features gc_stress_test
cargo test --locked --offline -p brimstone_core --lib --features handle_stats
cargo test --locked --offline -p brimstone_core --lib \
  --features baseline_jit,alloc_error,gc_stress_test,handle_stats
cargo clippy --locked --offline -p brimstone_core \
  --features baseline_jit,alloc_error,gc_stress_test,handle_stats --all-targets -- -D warnings
cargo check --locked --offline --release -p brimstone_core \
  --features baseline_jit,alloc_error,gc_stress_test,handle_stats
RUSTDOCFLAGS='-D warnings ...' cargo doc --locked --offline -p brimstone_core --no-deps \
  --features baseline_jit,alloc_error,gc_stress_test,handle_stats
RUSTC_BOOTSTRAP=1 RUSTFLAGS='-Zsanitizer=address' \
ASAN_OPTIONS='detect_leaks=0:abort_on_error=1:halt_on_error=1' \
  cargo test --locked --offline -p brimstone_core --lib --no-default-features \
  --features no_jemalloc,gc_stress_test <focused-filter>
```

Final results:

| Gate | Result | Log |
| --- | --- | --- |
| reviewer cross-owner, debug/release | rejected, exits 101/101 | `reviewer-cross-owner-*-final.log` |
| reviewer TLS teardown, debug/release | normal exits 0/0 | `reviewer-tls-drop-*-final.log` |
| focused exact-owner release | 1 passed | `focused-owner-release-final.log` |
| focused TLS + JIT release | 1 passed | `focused-tls-jit-release-final.log` |
| focused installed-host release | 16 passed | `focused-host-release-final.log` |
| full Brimstone workspace | 92 passed: 71 core + 11 parser/snapshot + 1 bridge + 9 rooted-task | `full-workspace-default-final.log` |
| baseline-JIT core | 171 passed | `full-core-baseline-jit.log` |
| alloc-error core | 71 passed | `full-core-alloc-error.log` |
| forced-moving-GC core | 72 passed | `full-core-gc-stress.log` |
| handle-stat core | 68 passed | `full-core-handle-stats.log` |
| combined JIT/OOM/GC/handle-stat | 184 passed | `full-core-combined-final.log` |
| focused ASan | 20 passed: owner 1 + TLS 1 + hosted/GC 18 | `asan-*.log` |
| strict combined Clippy | passed with `-D warnings` | `clippy-combined-strict-final.log` |
| combined release check | passed | `release-check-combined-final.log` |
| warning-denied rustdoc | passed | `rustdoc-combined-strict-final.log` |
| compile-fail API probes | 5 expected errors observed | `compile-fail-status.txt` |
| exact-path rustfmt | passed; stable ignored 3 nightly-only repository keys | `rustfmt-authorized-check-final.log` |

The first workspace attempt and one intermediate rerun are retained as negative evidence. They
exposed, respectively, concurrent oversized OOM fixtures and pointer-shaped non-root bytecode
metadata. The final OOM fixture and fixed-read-only split corrected both; the final workspace and
combined logs above are green.

The four rustdoc lint allowances are inherited Brimstone-wide debt outside this task's writable
scope; all other rustdoc warnings were denied.

Final Rust source SHA-256 values:

```text
0f177733103f498a246eaaf797b65e6010a8d6d92293edc3c32e646e178504b3  browser_script.rs
9955064e25cc59ea385ad876a4598435fa0229fb9a9773a72a0a3f599068409d  browser_host.rs
9087acbd8c5a32c0118d953254bd4f2f9a7921a80c4b8858cda1fc194988baab  context.rs
372d233dfb7becb65da44f3604dc9cb227062caba1d310bc408cdf39df6aaf34  tasks.rs
98fb1762c7bd0d40b1311261ddf9deb35fbf8f1110fdd47a45864ea3859aa005  gc/handle.rs
db9dccd4b935181ccfe05e149f461309fd7e8d9da0194cf42b272da9f8900742  gc/pointer.rs
abc41a83cea9034787855352478c1a8a032744133894ffb8ccbecb6eeccb173c  gc/heap.rs
```

## Remaining security and resource limitations

1. Legacy raw `Context`, `RawHandle::DerefMut`, pointer extraction, and lifetime-free `HeapPtr`
   remain unsafe-quarantine debt. The unsafe raw-context contract forbids installing foreign,
   stale, or unregistered root contents. Named safe root/replace/cast/escape paths defend in depth,
   but this is not a completed internal lifetime migration.
2. Detached serializer traversal is separated from live-root admission, but its legacy access-only
   pointer path is not a final scoped serializer capability. Forged raw pointer dereference remains
   outside the branded embedding guarantee and blocks product admission.
3. Parser/analyzer/compiler work, diagnostic construction before bounded copying, builtin/native
   loops, host callback CPU, GC CPU, permanent heap, and native libraries are not fully charged or
   continuously polled. Cancellation/deadline can be delayed there.
4. Pending task count is capped, but allocator rounding and task-payload bytes are not fully
   accounted. Managed allocation accounting measures requests, not RSS/live heap. Process memory
   pressure and OOM isolation remain open.
5. A foreign legacy queue larger than 10,000 is not scanned and permanently poisons its owner.
   Eventual owner/process destruction reclaims it; a product event loop needs generation-tagged
   queues and process-level admission.
6. Panic during unprovable host disposition/abort intentionally aborts the process. Per-document
   process isolation and parent recovery are later requirements.
7. A job throw clears remaining contained work. HTML global error/unhandled-rejection reporting and
   continued checkpoint semantics are absent.
8. No principal/origin/process binding, CSP, sandbox, Trusted Types, WebIDL Window/Document,
   workers, modules/dynamic import, timers, observers, parser reentry, multiple globals, site
   isolation, or production loader is admitted by this gate.
9. `baseline_jit` remains off by default; browser admission suppresses native entry even when the
   feature is compiled. `PRODUCT_DISPATCH_ENABLED` remains literal `false`.
10. The process-lifetime TLS registry and linear owner lookup are a correctness quarantine with
    measurable retained-memory and hot-path CPU cost, not final product architecture.

No files were staged, committed, or pushed.
