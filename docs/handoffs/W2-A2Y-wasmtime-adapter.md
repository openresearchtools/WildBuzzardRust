# W2-A2Y browser-owned Wasmtime core adapter

- Task: Add the first capability-free Wild Buzzard boundary around the exact imported Wasmtime
  core without exposing a JavaScript `WebAssembly` API or activating page content.
- Owner: Agent 2 — JavaScript/WebAssembly; owner gate and independent frozen-source review both
  returned GO.
- Status: Complete for the contained binary-only adapter. NO-GO for Brimstone/DOM connection,
  untrusted pages, browser WebAssembly conformance, sandbox/AppImage acceptance, or product
  activation.
- Upstream baseline: Wasmtime `v47.0.3`, revision
  `5554cc1a651da536af2cc46c7324bdc085b162e3`, with Cranelift `0.134.3`, from the exact admitted
  source under `js/wasmtime/`.
- Firefox reference: ESR153 `c19b7e89270787889495688244ec6ee8e79288a1`, especially
  `js/src/wasm/WasmJS.{h,cpp}`, `WasmModule.{h,cpp}`, `WasmInstance.{h,cpp}`, and
  `WasmMemory.{h,cpp}`, plus `js/src/jit-test/tests/wasm/` and
  `testing/web-platform/{tests,mozilla/tests}/wasm/`. SpiderMonkey remains behavioral reference
  only.
- Wild Buzzard paths added: the independent `js/wasm/` workspace: `Cargo.toml`, `Cargo.lock`,
  `README.md`, seven source modules, and two integration-test files. It is not a root-workspace or
  product dependency.

## Dependency and capability boundary

`wild_buzzard_wasm` is first-party MPL-2.0 Rust, edition 2024, with `rust-version = "1.94"` and
`unsafe_code = "forbid"`. It depends on the exact local `wasmtime = "=47.0.3"` path with default
features disabled and only `std,runtime,cranelift,gc,gc-drc,threads`. Cranelift, on-demand instance
allocation, and the DRC collector are selected explicitly. Runtime Wasm GC objects, threads/shared
memory, shared-everything threads, memory64, stack switching, custom page sizes, branch hints, wide
arithmetic, and legacy exceptions remain disabled.

One `WasmProcess` owns one Wasmtime `Engine` and its module, store, and instance registries. Public
IDs carry owner, slot, and never-wrapping generation; foreign, stale, and mismatched IDs fail
closed. Store removal invalidates descendant instances, reset tears down instances then stores then
modules, and external interrupt handles become unusable after shutdown.

This gate accepts bounded core binary modules only, rejects every import before admission,
instantiates with an empty import list, and calls only exported functions whose parameters and
results are all `i32`. It exposes no `Linker`, host function, WAT parser, WASI, filesystem, socket,
HTTP, environment, clock, randomness, CLI/server, component model, native deserialization,
automatic cache, async fiber, or ambient host capability.

Hard logical limits cover module bytes, module/store/instance identities, resident instances,
instances per store, memories and tables per store, memory bytes, table elements, call arity,
export-name bytes, Wasm stack, and fuel. Epoch interruption is externally triggerable without
async/fibers. Failed instantiation is conservatively charged as resident until store teardown
because Wasmtime does not report whether it allocated before failure.

## Frozen source and lock identity

The ordered 11-file source/manifests/README/tests aggregate, excluding `Cargo.lock`, is:

`d935b51df8c2214ff397e7e735b8d760f238b38b69f456e93e2054886dabf8a3`

The lockfile hash is:

`83a227c8d00fd79a047f50a2aa44684cb13344d9440e4586ca0e86c0c160efae`

The complete 12-file aggregate, using lexical path order with `Cargo.lock` first, is:

`7c40e5355a81a4aef8c2431d6b4640e0c0d7fdeb384ffc3eace5972a5ae4dc5f`

| Path | SHA-256 |
| --- | --- |
| `js/wasm/Cargo.lock` | `83a227c8d00fd79a047f50a2aa44684cb13344d9440e4586ca0e86c0c160efae` |
| `js/wasm/Cargo.toml` | `355e2ccca70fb6b83967682e49011d00c7cdc1ca14a1633073982fb8bdec4dc3` |
| `js/wasm/README.md` | `f5eae1ebbe8bafa7a01228589ecca4e8baa851c344c88f1a7c1beea570351e55` |
| `js/wasm/src/error.rs` | `0e847919a542afe3c16bfa30a72a6ec793c21f3d6845d03c801b358641cd1479` |
| `js/wasm/src/identity.rs` | `b58b1be302c3b4340559a89e8ddd5b56efb30e6e4fa7c122dd151ebe83ec7dbc` |
| `js/wasm/src/lib.rs` | `c6a32aa46f124935b2ecd33ae1e38c5f4221502ebeb1df841d80cbfc1725caa6` |
| `js/wasm/src/limits.rs` | `b45818fbf89677efe1d8e814c28ee8d91974386ed5e7cd7a1fa6eb6519654356` |
| `js/wasm/src/policy.rs` | `37af63d3860d2a58a94b8faebb3100e22741d20488888fe99df3eecc6d00778a` |
| `js/wasm/src/registry.rs` | `2343598b2c31726f75bbcca8e919878864141584b45ee57c7dd5b9e2e3d655f6` |
| `js/wasm/src/runtime.rs` | `5d3848d0b6bb871699b8dad0a2a90f43e53e534552554db6af522880be127fa3` |
| `js/wasm/tests/adapter.rs` | `d3b3e1ea8b3d6327f0558dfe55e9ba4b82e47a2704ce2620bf617447ef987d6c` |
| `js/wasm/tests/policy_audit.rs` | `ef559f5dbd75dfa980c2aaf11f68f7db915037886e5c5151d757347d06915ac9` |

The universal lock has 109 package records including the first-party adapter root and zero Git
sources. All 108 non-first-party records are byte-for-byte identical to the admitted reduced
W2-A2X seed; the only two textual lock diff lines replace that seed's root name/version with
`wild_buzzard_wasm 0.1.0`. A fresh latest-compatible resolution intentionally differs and is not
the admitted lock.

The selected executable feature graph is 83 unique packages: one first-party adapter, 23 packages
from the imported Wasmtime tree, and 59 registry packages, with zero Git dependencies and none of
the prohibited product packages. The universal lock contains 83 registry records and also retains
inactive optional records not selected by this graph. In particular, v47's internal-fiber and
JIT-debug records can occur in the lock without being active dependencies. The policy test's
historical `wasmtime-fiber` string check is stale for the renamed internal package; future audits
must continue checking the resolved package/feature graph, not infer executable inclusion or
exclusion from a lockfile name substring alone.

## Owner and independent evidence

The declared MSRV is Rust 1.94.0, but that toolchain was not installed locally and this gate does
not claim an MSRV test. Owner and independent commands used Cargo/rustc 1.96.0 on
`x86_64-unknown-linux-gnu`, locked and offline. Owner output used:

`/home/user/Documents/wildbuzzardbuilds/w2-a2y-wasm-adapter/target-owner`

Representative owner command:

```sh
CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w2-a2y-wasm-adapter/target-owner \
  cargo test --manifest-path js/wasm/Cargo.toml \
  --locked --offline --target x86_64-unknown-linux-gnu
```

The owner passed locked/offline metadata and selected-tree audits, check, strict all-target Clippy
with `-D warnings`, 27 tests with 0 failures, release build, and no-dependency rustdoc with all
warnings denied. The 27 tests comprise 2 unit, 18 adapter integration, and 7 policy-audit tests;
doc tests ran 0 tests. Format, diff, first-party-unsafe, capability, dependency-feature, product
connection, and repository-artifact scans passed.

Independent review used the fresh target:

`/home/user/Documents/wildbuzzardbuilds/w2-a2y-final-review/target`

It reproduced the exact imported lock seed byte-for-byte, confirmed both frozen aggregates, and
independently passed format; locked/offline metadata and check; strict all-target Clippy with
`-D warnings`; the same 27 tests with 0 failures and 0 doc tests; release build; warning-denied
no-dependency rustdoc; and artifact, unsafe, capability, dependency, and disconnected-product
scans. The reviewer confirmed the 109-record universal lock, the selected `1 + 23 + 59` graph,
zero Git sources, and the absence of prohibited products from the selected graph. Final verdict:
GO for this contained adapter, with explicit NO-GO for page or product activation.

## Limits and open browser work

The adapter's limits are logical resource limits, not a process RSS bound. They do not
comprehensively charge adapter bookkeeping, compiled code or engine caches, virtual-memory guards
and reservations, native host allocations, or per-store GC heaps. Public construction exists for
testing and future embedding, so content-process integration must enforce exactly one
`WasmProcess` owner rather than allowing several engines to evade process accounting.

Compilation is synchronous and bounded by input bytes but has no wall-clock deadline,
cancellation, or compiled-code-size accounting. The adapter sequence poisons before its own
interrupt counter can wrap, but Wasmtime's independent epoch counter at natural `u64` rollover is
not proved. `max_wasm_stack` does not establish browser acceptance of native-stack use, signal/
unwind interaction, executable mappings, or sandbox/AppImage behavior.

This is not the JavaScript `WebAssembly` API. It has no Brimstone value conversion, generated DOM
binding, ArrayBuffer/SharedArrayBuffer ownership, streaming compilation, CSP or cross-origin-
isolation policy, promises/jobs, browser exception mapping, debugger/profiler hooks, authenticated
cache, host imports, or WebAssembly specification/WPT evidence. Brimstone and Wasmtime still have
separate collectors and no admitted rooted cross-heap edge/cycle protocol; Wasm GC therefore stays
off for page content. Threads/shared memory are compiled into the upstream library feature closure
but disabled by runtime policy and remain separately gated.

First-party adapter code is MPL-2.0 and introduces no first-party unsafe or native boundary.
Wasmtime and Cranelift retain Apache-2.0 WITH LLVM exception and their existing audited unsafe/JIT/
signal/native dependencies. The 59 selected registry sources are not vendored by this gate. A
product gate still needs exact offline source/license closure, compile and RSS accounting, native
stack/signal/sandbox/AppImage validation, cross-heap rooting and cycles, JS API semantics, spec/WPT
conformance, fuzzing, and content-process integration before enabling any untrusted module.
