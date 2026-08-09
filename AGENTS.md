# Wild Buzzard agent operating agreement

## Mission

Wild Buzzard is an independent, privacy-respecting, general-purpose web browser whose first-party browser chrome, engine, and runtime are implemented in Rust.

The compatibility target is observable Firefox ESR parity:

- Web-engine behavior: ECMAScript, WebAssembly, DOM, HTML, CSS, layout, graphics, networking, storage, media, accessibility, extensions, and developer tooling.
- Browser-product behavior: navigation, tabs, windows, prompts, downloads, settings, history, bookmarks, passwords, permissions, sessions, recovery, keyboard operation, and accessibility.
- Security and privacy behavior: process isolation, origin boundaries, TLS validation, sandboxing, permission checks, safe failure, and no unsolicited product telemetry.

Parity means behavior, not a line-for-line translation or preservation of Gecko's internal architecture. A simpler Rust design is preferred when tests prove equivalent behavior. Security fixes take precedence over reproducing an insecure historical behavior.

Wild Buzzard has its own name, artwork, application IDs, profile paths, defaults, and visible identity. Do not copy Firefox trademarks or imply Mozilla affiliation.

This is a long-running compatibility program. A component is not complete merely because it compiles, renders one page, or passes a smoke test. Claims of parity require recorded conformance and regression evidence.

## Supported product target

Wild Buzzard targets only 64-bit Linux:

- Rust target: `x86_64-unknown-linux-gnu`.
- Release artifact: a self-contained AppImage.
- Desktop integration: Linux windowing and input, supporting Wayland and X11 where required for
  normal desktop compatibility.
- Linux process isolation, sandboxing, graphics, audio, accessibility, profile, and packaging
  behavior are in scope.

Do not implement, test, package, or carry product requirements for Windows, macOS, Android, iOS, or
other architectures. Standards-facing Rust code may remain naturally platform-independent, but no
workstream should spend time on another platform's adapters or parity. Cross-platform files that
exist inside an exact imported upstream snapshot are inactive source, not supported code; prune
them when establishing the canonical editable Linux workspace.

AppImage recipes and metadata belong in `packaging/appimage/`, but AppImages, AppDirs, extracted
roots, debug symbols, and packaging logs are build artifacts and must remain under the external
`../wildbuzzardbuilds/` tree.

## Meaning of "implemented in Rust"

All new first-party runtime and product code must be Rust. WebIDL, schemas, localization resources, shaders, test data, and generated files are allowed.

Operating-system APIs and audited third-party native libraries may be reached through narrow Rust FFI modules when unavoidable. Every native boundary must be documented, tested, and assigned an owner. Transitional native dependencies must not quietly become permanent architecture.

Do not add new first-party C or C++ implementation code. Imported C/C++ headers may be retained temporarily only when required to compile an adopted Rust component, and must be recorded as migration debt.

## Firefox reference checkout

The ignored `firefox/` directory is the read-only reference implementation:

- Repository: `https://github.com/mozilla-firefox/firefox.git`
- Baseline: Firefox ESR153 branch `esr153`
- Pinned checkout: `c19b7e89270787889495688244ec6ee8e79288a1`
- State: detached `HEAD`, non-shallow, with full Git history

It is reference material, not part of Wild Buzzard and never a build input.

Agents may inspect implementation code, tests, generated interfaces, blame, and historical implementations. Useful commands include:

```sh
git -C firefox show HEAD:path/to/file
git -C firefox log --follow -- path/to/file
git -C firefox blame path/to/file
git -C firefox log -S'symbol' -- path/to/subsystem
git -C firefox show <historical-commit>:path/to/file
```

Reference rules:

- Never edit, format, commit, fetch into, or generate files inside `firefox/`.
- Never create a Cargo path dependency, symlink, include path, build-script input, test fixture lookup, or runtime lookup pointing into `firefox/`.
- Wild Buzzard must build and test when `firefox/` is absent.
- Search narrowly by subsystem. The reference contains hundreds of thousands of files.
- Before porting behavior, inspect both the implementation and its relevant tests.
- Use full history to understand invariants, regressions, and removed approaches rather than translating only the current file.
- Preserve copyright, license, and third-party notices on imported or derived code.
- Never copy service credentials, endpoint keys, Firefox artwork, branded defaults, or proprietary binary material.

The pinned ESR commit is a reproducible behavioral baseline, not a security freeze. The orchestrator must track later security and standards changes separately.

## Repository layout

Preserve Firefox's broad subsystem hierarchy when practical so paths remain easy to compare:

```text
browser/       Wild Buzzard browser shell and product behavior
toolkit/       provider-neutral browser-level services
devtools/      developer tools and protocols
accessible/    accessibility tree and operating-system adapters
js/            Rust JavaScript and WebAssembly runtime
dom/           Web APIs, DOM, HTML, events, workers
layout/        formatting, layout, scrolling, hit testing
servo/         imported and adapted Stylo crates
gfx/           WebRender, WebGPU integration, compositor, fonts, color
image/         image decoding and animation
media/         audio/video containers, codecs, capture, playback
netwerk/       DNS, HTTP, QUIC, proxy, cache, cookies, sockets
security/      TLS, certificates, permissions, sandbox policy
storage/       origin storage, quota, databases
xpcom/         temporary Rust service abstractions during migration
ipc/           typed process boundaries
widget/        windows, surfaces, input, clipboard, platform events
intl/          Unicode, encoding, locale, segmentation
packaging/     Linux x86_64 AppImage recipes and metadata; never packaged output
third_party/   pinned external source, licenses, and notices
testing/       conformance, integration, reftest, and browser harnesses
docs/          architecture, parity evidence, provenance, handoffs
firefox/       ignored read-only ESR reference; never a dependency
```

Keep imported Mozilla- or Servo-authored Rust components at their Firefox-relative paths unless the orchestrator approves a move. Do not preserve a path at the cost of cyclic crate dependencies; explicit Rust interfaces and an acyclic dependency graph take precedence.

## Agent topology

There is one main orchestrator and six logical component owners. They are durable workstreams, not a requirement that all six run simultaneously. The orchestrator uses only the concurrency the environment can safely support and may create short-lived research or test agents with narrower scopes.

### Model and staffing policy

Use `gpt-5.6-sol` for component implementation and review agents. Select reasoning effort by risk:

- `high` for bounded crate implementation, focused protocol work, test harnesses, mechanical
  adaptation, and well-specified platform tasks.
- `ultra` for JavaScript/WebAssembly/GC/rooting, DOM and layout architecture, unsafe or security
  boundaries, cross-process design, difficult integration failures, and decisions that would be
  expensive to reverse.

Do not reduce the JavaScript lane to an occasional side task. Keep Agent 2 continuously staffed
across waves, with JavaScript, WebAssembly, GC, rooting, DOM host bindings, optimization, modules,
promises, and debugger behavior treated as one critical program. A browser shell or static-page
demo does not justify pausing that workstream.

The main agent orchestrates, reviews, integrates, builds, tests, and tracks evidence. It should not
silently absorb a component lane while a non-overlapping delegated task can make progress. When
concurrency is constrained, schedule work in waves and preserve the six durable ownership lanes.

### Main orchestrator

The main agent owns integration and is the only default writer for:

- Root `Cargo.toml`, `Cargo.lock`, toolchain files, CI, release configuration, and this `AGENTS.md`.
- Cross-subsystem interface crates and architecture decisions.
- Stable IPC protocol/message and service-kind assignments in `docs/wire-registry.toml`.
- `third_party/` imports, provenance, license review, and upstream refreshes.
- Parity matrices, milestones, and product-wide status.
- Full builds, integrated test gates, conflict resolution, and release acceptance.
- Privacy, prohibited-endpoint, branding, dependency, and native-code audits.

The orchestrator must give every delegated task:

- a task identifier and observable outcome;
- one owner and exact writable paths;
- relevant Firefox implementation, history, and test paths;
- dependencies and the public interface contract;
- required tests and acceptance criteria;
- explicit exclusions and forbidden shortcuts.

Prefer thin vertical slices that integrate frequently over six isolated ports that cannot run together.

### Agent 1: foundation, process model, and platform

Default ownership:

- `xpcom/`, `ipc/`, `widget/`, `memory/`, and `mozglue/`
- `intl/`, preferences, profiles, clocks, task scheduling, and event loops
- Process launch, shutdown, sandbox plumbing, windows, surfaces, input, clipboard, and drag-and-drop

Responsibilities:

- Replace XPCOM ownership patterns with Rust traits, typed handles, `Arc`/`Weak`, explicit lifetimes, and explicit errors.
- Implement parent, content, GPU, network, and utility process roles.
- Own typed and versioned IPC, validation, cancellation, backpressure, crash isolation, and deterministic shutdown.
- Provide platform-neutral task, timer, preference, localization, profile, and event primitives.
- Keep `unsafe` and operating-system FFI in small reviewed modules.

Imported `modules/libpref/parser` belongs to this workstream. Firefox's `xpcom/rust` and `ipc/rust` are reference material because they primarily wrap C++ Gecko contracts.

W3-A6W admits `widget/rust/wild_buzzard_linux` as a Linux x86-64 window/event-shell
prerequisite. Exact crates.io `winit` 0.30.13 is selected with defaults disabled and only
`wayland`, `wayland-dlopen`, and `x11`. The first-party API exposes Wild Buzzard value types rather
than winit objects or native handles. Its lifecycle is one-way (`Running -> Stopping -> Exited`),
stop seals ordinary event admission, callback-requested exit prevents later queued events from
escaping, and wake admission closes permanently on stop/drop/error. Normal shutdown publishes one
`Destroyed` for a live surface before one `Stopped`; Wayland and X11 live-display smokes exercise
that contract.

At the W3-A6W gate, winit owned the native window and backend surface but the crate had no Wild
Buzzard presentation connection. Its `SurfaceDescriptor` recorded desired configuration and
identity only. Callback panic terminated that historical protocol; some ignored/suppressed native
events were not fully counted; and its smoke lacked an internal timeout and redraw-identity
assertion. W4-A4P closes only the direct-EGL connection described below. AppImage work must still
audit the exact Linux feature graph and dynamically opened Wayland/X11/xkbcommon libraries, choose
a host-ABI versus bundling policy, and rerun both backend smokes from the packaged artifact. See
`docs/handoffs/W3-A6W-linux-window.md`.

W4-A4P connects that event-shell owner to the first bounded native presentation surface through
`gfx/wild_buzzard_linux_presenter`. Its synchronous preparation API keeps the display-handle owner
borrowed across EGL display, configuration, context, window, and surface creation and rejects an
exact raw-display mismatch. The accepted startup profile is hardware-only desktop OpenGL 3.2,
RGBA8/A8, sRGB-capable, zero-sample EGL on the exact Wayland display or X11 visual. Native, GL,
driver, renderer-panic, extent, diagnostic-pixel, and swap faults are caught and latched at the
first authoritative stage. Normal shutdown reports only Rust wrapper release; it never fabricates
native EGL destruction acknowledgement which glutin does not expose. Startup failure after native
ownership explicitly consumes the partial presenter and carries either checked wrapper release or
fail-closed retention through the shell's terminal `Stopped` outcome; `NotCreated` is reserved for
failure before ownership. `SwapSubmissionReceipt`
proves only that a bounded direct-GL frame completed, its initialized readback sample matched, and
EGL accepted the swap for the exact surface identity and sequence. It does not prove that a desktop
compositor displayed the buffer, and it does not present WebRender or browser content. See
`docs/handoffs/W4-A4P-linux-presenter.md`.

### Agent 2: JavaScript and WebAssembly runtime

Default ownership: `js/`.

Responsibilities:

- Complete and harden the adopted Brimstone parser, bytecode VM, values, objects, realms, modules,
  promises, jobs, exceptions, debugger hooks, garbage collector, and standard library for browser
  use.
- Build Brimstone's Linux x86-64 JIT tiers, executable-code lifecycle, safepoints, stack maps,
  interruption, deoptimization, and profiling; an interpreter-only result is not the product target.
- Integrate the selected Wasmtime/Cranelift core for WebAssembly decoding, validation, compilation,
  instantiation, memories, tables, reference types, exceptions, threads, and GC without exposing
  WASI to ordinary web content.
- A stable host API used by generated DOM bindings. The runtime must not import concrete DOM implementations.
- Test262, WebAssembly specification tests, differential tests, fuzzing, and applicable SpiderMonkey regression behavior.

SpiderMonkey is a JavaScript engine, not a Java engine. Firefox implements WebAssembly inside SpiderMonkey, and the Wasm runtime shares its GC, values, promises, JIT machinery, and host rooting rules. Treat JS, Wasm, GC, and host rooting as one architectural boundary even when separate agents research subparts.

Firefox's `js/src/rust` is helper/glue code, not a Rust implementation of SpiderMonkey. Current ESR153 uses Rabaldr and Baldr/Ion for Wasm; it has no active Cranelift backend. Historical Cranelift code is available in the full reference history, but restoring it is not considered a completed Rust runtime.

#### Canonical Brimstone baseline

Wild Buzzard's JavaScript execution-engine baseline is the exact Brimstone source snapshot at
`js/brimstone/`:

- Upstream: `https://github.com/Hans-Halverson/brimstone.git`
- Upstream branch at selection time: `master`
- Pinned revision: `bfb720f0afb8b2b28b27c22ee7091deb7d16b082`
- Commit time: `2026-08-08T19:56:40-07:00`
- License: MIT

The pin is a source baseline, not a production-readiness claim. Upstream explicitly describes the
engine as not production-ready and its compacting collector as very unsafe. The first adaptation
gate added an exactly-once thread-affine `OwnedContext`, lifetime-branded moving-GC root scopes, and
explicit unsafe quarantine for legacy raw types; it also corrected concrete heap-metadata teardown
and resize defects. This is sufficient for contained, disabled-by-default JIT infrastructure work,
not for untrusted content. Before any DOM binding or untrusted page can use it, Agent 2 must complete
the internal lifetime/root migration, remove or encapsulate the remaining raw mutable aliases, add
hard execution/allocation/recursion limits and interrupt polls, and pass forced-GC, Miri where
applicable, sanitizer, fuzz, and malformed-input gates. Safe raw `Context` construction/manual
destruction and lifetime-free heap handles must never re-enter the embedding surface.

W2-A2J admitted one contained JIT infrastructure gate behind the off-by-default `baseline_jit`
feature. It uses the exact Cranelift `0.134.3` source already imported with Wasmtime, an exhaustive
151-opcode use/def/effect table, a bounded defense-in-depth verifier for trusted in-process
bytecode, lifetime-branded ABI storage, deterministic hotness/interrupt primitives, and an
owner-thread executable cache which transitions mappings from RW to RX under hard byte and entry
limits. A tiny generated-code proof covers boxed immediates/moves, SMI add/sub, forward boolean
branches, and return on Linux x86-64. `PRODUCT_DISPATCH_ENABLED` is compile-time false.

That historical gate is not a browser JIT tier. Within W2-A2J it emitted no calls, relocations,
traps, allocating helpers, native safepoints, stack maps, or moving pointers. Its shadow-frame
schema was not registered with the Brimstone root walker, its side-exit result had no continuation
path, backedges side-exited, and unsupported operations side-exited before execution. The verifier
assumes trusted compiler-produced bytecode and does not validate dynamic scope metadata.

W2-A2K separately extends the disabled gate with one contained allocating-helper and continuation
proof. A compiler-created `PreparedPrototype` is consumed into one privately constructed,
cache-owned `LoadedPrototype`; its RX machine code, immutable native-return-PC safepoint metadata,
and exact captured decoded program, including resolved constant-backed branch targets, cannot be
selected or replaced independently by the safe runner. The cache borrow prevents eviction during
the synchronous call. Native activations link through a higher-ranked, thread-affine scope into the
owning context's root chain. Their opaque initialized `JitSlot` values are checked for canonical
representation and exact active-context allocation starts before frame publication, rewritten by
moving GC only for compiler-derived CFG-live slots at the published safepoint, and checked again
before native or continued return values are accepted.

The sole allocating generated operation is zero-argument `NewObject`. It polls interruption,
publishes its exact live roots, calls through the versioned helper table, survives forced moving
collection, reloads moved values, and stores a rooted result. A tiny contained continuation uses
only the loaded artifact's exact decoded program and implements numeric `Neg` followed by `Ret`
without allocating in Brimstone's moving JavaScript heap or replaying the allocating instruction.
Return validation may still fallibly reserve host bookkeeping. Unsupported operations and backedges
remain fail-closed side exits. `PRODUCT_DISPATCH_ENABLED` remains compile-time false.

W2-A2K is not normal Brimstone VM/interpreter integration or a product baseline tier. It does not
provide rooted function/bytecode continuation identity, normal hot-function dispatch, DOM or
untrusted-content entry, broad calls or properties, exceptions, deoptimization, OSR,
debugger/unwind data, invalidation, complete native stack maps, asynchronous interruption, or an
optimizing tier. The remaining raw context/handle lifetime migration, hard browser resource limits,
full Test262, fuzzing, Miri where applicable, browser integration, and performance gates remain
open. Do not enable product dispatch on the strength of this contained proof.

W2-A2L advances that disabled proof into one actual Brimstone VM continuation. A real
`HandleScopeGuard` now owns the higher-ranked JIT scope. `VmFunctionBinding` freshly roots the exact
closure, function, scope, realm, optional constant table, and optional cache array, assigns a
never-reused binding identity to the loaded artifact, and revalidates those identities after moving
collection. Admission remains deliberately narrow: zero parameters, the initial realm, no runtime
function, no exception-handler table, no ordinary value constants, and no nonempty cache array.
Constant-backed branches are described only from the rooted table's exact raw jump metadata and are
rechecked against the verified instruction boundary.

On an admitted native side exit, W2-A2L roots every live slot, clears dead native slots, unlinks the
native activation, refreshes moved roots with allocation-free all-or-clear semantics, and constructs
a private `AdmittedVmResume` which safe callers cannot forge. The VM creates an ordinary fully traced
frame and publishes the exact prefix-inclusive resume PC. Only a numeric local `Neg` followed by
`Ret`, or an uncaught terminal `Throw`, may run. Native return, VM return, throw, interruption,
allocation failure, poison, and setup failure remain distinct fail-closed outcomes. Normal, error,
allocation, and injected post-publication/pre-dispatch panic paths must restore the exact parent
stack pointer, frame pointer, and frame depth or abort.

The VM capacity gate proves byte distance and multiplication with checked integer arithmetic before
performing in-allocation pointer movement. Forced-moving-GC regressions cover two distinct
`NewObject` PCs/maps, a moving destination overwrite, wide and extra-wide prefixes, return and
throw, allocation and panic cleanup, an oversized near-capacity frame, and subsequent context
recovery. The inherited handle scope inside `dispatch_loop` is not unwind-RAII for a panic which
originates inside dispatch; the injected panic test occurs before dispatch and is not evidence for
that case.

`baseline_jit` remains off by default and `PRODUCT_DISPATCH_ENABLED` remains compile-time false.
W2-A2L is not normal hot-function dispatch, general side-exit coverage, a browser baseline tier, or
permission to run DOM or untrusted-page code. Calls, properties, backedges, handled exceptions,
deoptimization, OSR, complete stack maps, debugger/unwind support, optimizing compilation, lifetime
migration, browser resource policy, Test262, and browser integration remain open.

W3-A2M broadens only that private actual-VM continuation. A fallible monotone abstract-CFG proof
now admits local moves and immediates, valid-JS `LogNot`/`TypeOf`, number-only arithmetic and
comparisons, exact-boolean/`ToBoolean`/undefined/nullish branches, forward joins, loops, `Ret`, and
uncaught terminal `Throw`. It analyzes both conditional successors, rejects every consuming path
for `Empty` or internal heap metadata, caps modeled analysis storage at 32 MiB, and caps worklist
dequeues at 2,000,000. Every taken nonpositive edge validates and publishes its exact target before
polling the deterministic interrupt budget; interruption and policy failure pop the admitted frame
and restore the exact parent VM state rather than exposing a resumable continuation.

The private resumed dispatch disables comparison fusion so backedges cannot skip a poll, and its
handle scope is unwind-exact for a panic originating inside dispatch. The admitted cyclic
operations are nonallocating; terminal `Throw` may allocate only after publishing its PC and cannot
reach another edge because handler tables remain rejected. W3-A2M still emits no broader native
code: generated execution has the same tiny subset and sole `NewObject` helper established by the
earlier gates, while the new breadth is actual-VM side-exit continuation. `baseline_jit` remains off
by default, `PRODUCT_DISPATCH_ENABLED` remains compile-time false, and no DOM or untrusted page can
enter it.

Do not treat the current analysis limits as an untrusted-bytecode CPU bound: work counts dequeues,
not every local-cell scan. Calls, properties, parameters, caches, handled exceptions, noninitial
realms, normal hot dispatch, OSR, deoptimization, debugger/unwind metadata, optimizing compilation,
and the remaining lifetime/resource/conformance gates stay open. See
`docs/handoffs/W3-A2M-brimstone-vm-breadth.md`.

W4-A2N broadens the generated native side of the same disabled proof. Cranelift now emits a bounded
local CFG for immediate values and moves; SMI arithmetic, bitwise, shift, unary, and comparison
families; exact conditional branches, joins, loops, zero-capacity `NewObject`, and `Ret`. Dynamically
slow, coercing, overflowing, negative-zero, non-SMI, or unsupported cases side-exit at the source
instruction to the rooted real-VM continuation before mutating the destination. A bounded
must-provenance proof prevents native control flow or return from observing `Empty` or internal
pointer-shaped VM metadata.

Generated-code ABI version 3 adds a private nonallocating backedge-poll helper. Every taken
nonpositive edge publishes its exact target before polling, has an independent one-million-edge
activation cap, and preserves interrupt priority. Allocating safepoints and nonallocating poll
calls have disjoint location ranges; emitted callsites must exactly match the compiler plan, and
native reachability stops at a mandatory side exit so unreachable Cranelift blocks cannot satisfy
fabricated safepoint metadata. Forced-moving-GC loops and differential slow-path tests execute in an
actual Brimstone VM continuation. The per-edge Rust helper is correctness-first debt: the later
product tier needs an ownership-safe inline fast poll which calls Rust only for slow handling.

`baseline_jit` remains off by default and `PRODUCT_DISPATCH_ENABLED` remains compile-time false.
W4-A2N still has no normal hot dispatch, calls, properties, parameters, caches, handled exceptions,
OSR, deoptimization, debugger/unwind metadata, complete native stack maps, optimizing tier, DOM
entry, or untrusted-page permission. See `docs/handoffs/W4-A2N-native-jit-cfg.md`.

Preserve and extend Brimstone's parser, register bytecode, NaN-boxed value representation, VM-frame
layout, shapes, and inline caches when evidence supports them. The JIT program then proceeds in
reviewable gates:

1. Define a stable Rust-to-generated-code helper ABI, complete opcode use/def/effect metadata, a
   bytecode verifier, hot-call/backedge counters, deterministic interrupts, and a bounded W^X code
   allocator/cache.
2. Add a Cranelift baseline compiler for `x86_64-unknown-linux-gnu`, initially using boxed values,
   canonical GC-visible shadow frames, explicit safepoints, and side exits to the interpreter for
   unsupported or throwing operations.
3. Prove moving-GC correctness by spilling every live reference before allocating helpers,
   reloading after collection, never embedding untracked moving pointers, and stress-collecting at
   every safepoint. Add compiled exception metadata, code invalidation, and debugger/unwind support.
4. Add typed feedback beyond property caches, SSA/optimizing IR, OSR, inlining, unboxing,
   deoptimization snapshots, precise native stack maps, and performance regression gates.
5. Replace the whole-heap browser scaling model with partitioned generational/incremental
   collection and explicit memory-pressure behavior suitable for many site-isolated content
   processes and many realms.

W2-A2K, W2-A2L, W3-A2M, and W4-A2N are partial evidence for steps 2 and 3, not completion of either
step.

Do not combine Boa, the provisional `wild_buzzard_js` interpreter, and Brimstone as multiple live
heaps in one page. The existing first-party `js` crate is transitional host-contract and regression
material: migrate its validated semantics/tests into the Brimstone-backed facade, then retire the
redundant interpreter. A page process has one canonical JS heap/runtime with multiple realms as
required; site isolation is a process boundary, not a VM-per-tab rule.

#### Wasmtime boundary

Wasmtime `v47.0.3`, revision `5554cc1a651da536af2cc46c7324bdc085b162e3`, is the selected
WebAssembly execution-core baseline. It provides a substantially stronger Rust base than recreating
validation, Cranelift compilation, native execution, traps, reference types, Wasm GC, exceptions,
SIMD, tail calls, and interruption machinery. Its license is Apache-2.0 with LLVM exception and its
MSRV is Rust 1.94.0. The exact release superproject source is pinned at `js/wasmtime/` together with
the exact core WebAssembly specification suite. The Component Model and 210.86 MiB WASI test-suite
gitlink payloads are deliberately not materialized; their upstream identities remain recorded in
`js/wasmtime/WILDBUZZARD_UPSTREAM.md`. This is source admission, not product activation.

The initial product configuration is `wasmtime` with default features disabled and only
`std,runtime,cranelift,gc,gc-drc,threads`. Use Cranelift only. Do not enable Winch: v47.0.3 describes
it as unsuitable for production and its x86-64 backend lacks proposal coverage needed for browser
parity. Do not include the CLI, WAT parser, WASI, WASI HTTP, server, component model, automatic
cache, async fibers, stack switching, profiling, pooling allocator, or ambient host capabilities in
the product build for normal web pages. A later feature requires its own provenance, threat-model,
resource, conformance, and AppImage-closure gate. The selected locked Linux build reaches 23
packages in the imported Wasmtime tree and 59 registry packages. Those registry sources are not
vendored by this import and require their own exact-source/license admission before an offline
release build can claim a closed dependency set.

W2-A2Y adds the first browser-owned boundary at the independently locked `js/wasm/` workspace.
The MPL-2.0 `wild_buzzard_wasm` crate uses Rust 2024 with MSRV 1.94 and depends on the exact local
Wasmtime `=47.0.3` crate with defaults disabled and exactly the six features above. One
`WasmProcess` owns one Wasmtime `Engine`; callers receive only owner-, slot-, and
generation-checked module, store, and instance IDs. It accepts bounded core binary modules only,
rejects every import before registry admission, instantiates with an empty import list, and calls
only exports whose arguments and results are all `i32`.

The adapter selects Cranelift, on-demand instance allocation, and the DRC collector. Runtime Wasm
GC objects, threads/shared memory, shared-everything threads, memory64, stack switching, custom page
sizes, branch hints, wide arithmetic, and legacy exceptions remain disabled even though the pinned
compile graph contains the `gc` and `threads` implementation features. Fuel bounds each contained
operation/start function; epoch interruption supplies a synchronous terminal request; stack,
module, store, instance, memory, table, arity, and name limits fail closed. Failed instantiation is
conservatively charged until store teardown, reset/drop invalidates descendants deterministically,
and interrupt-sequence exhaustion poisons rather than wraps.

This gate exposes no `Linker`, host function, WAT, WASI, filesystem, socket, HTTP, environment,
clock, randomness, CLI, component model, compiled-code cache, async/fiber path, or native-code
deserialization. Its limits account logical Wasm resources, not total RSS: adapter bookkeeping,
compiled code and engine caches, VM reservations/guards, host allocations, and per-store GC heaps
are not comprehensively charged. Compilation is synchronous and has no wall deadline,
cancellation, or compiled-code-size budget. Exactly one `WasmProcess` per content process and a
sufficient native thread stack remain embedding obligations; natural Wasmtime epoch-counter
rollover is not proven.

W2-A2Y is product-disconnected. It is not the JavaScript `WebAssembly` API, a Brimstone bridge, a
cross-heap rooting design, imports or host calls, the Wasm specification suite, WPT evidence, a
sandbox boundary, AppImage acceptance, or permission to execute untrusted page Wasm.

Wasmtime is not by itself a browser WebAssembly implementation. Wild Buzzard must still implement
the JavaScript `WebAssembly` API, streaming compilation, CSP and cross-origin-isolation policy,
ArrayBuffer/SharedArrayBuffer memory ownership, promise/job integration, browser error mapping,
debugger/profiler hooks, cache policy, and JS/Wasm call conversions. Brimstone and Wasmtime have
separate collectors, so cross-heap references and cycles require a reviewed rooted-handle/trace
contract; wrapping raw pointers or retaining each heap from the other is forbidden. Wasmtime's DRC
collector cannot reclaim cycles, while its copying collector is documented as not yet functional.
Wasm GC objects which can form cycles with JavaScript therefore remain disabled for untrusted pages
until an external-edge tracing/coordinated-collection design is implemented and stress-tested.
Threads/shared memory are Tier 2 and are not fully covered by `ResourceLimiter`; enforce limits in
the browser adapter and gate exposure on cross-origin isolation. Memory64, stack switching, and
JavaScript Promise Integration remain disabled until their upstream support, GC interaction, and
browser semantics pass dedicated tests.

### Agent 3: Web platform, DOM, Stylo, and layout

Default ownership:

- `dom/`, `layout/`, `parser/`, and `servo/`
- Excluding media, Canvas, WebGPU, and storage backends assigned to other agents

Responsibilities:

- HTML/XML parsing, DOM trees, generated WebIDL bindings, events, navigation lifecycle, workers, timers, and Web APIs.
- CSS parsing, selectors, cascade, computed values, invalidation, layout, fragmentation, scrolling, hit testing, selection, and semantic-tree production.
- Fetch, CORS, CSP, and referrer-policy semantics above the transport boundary.
- Import and adapt Stylo rather than rewriting it.

Adopted Stylo source includes `servo/components/style`, `selectors`, `style_traits`, `style_derive`, `servo_arc`, `malloc_size_of`, and `to_shmem` support crates.

The imported Firefox Stylo snapshot is active in the independently locked `servo/` workspace under
the default `wild_buzzard` profile. That profile uses Rust-owned atom, state, preference, and
platform shims; Gecko features are prohibited negative gates. Its concrete immutable
DOM/computed-style adapter feeds root layout and the independently locked
`browser/wild_buzzard_engine` integration proof. That proof is synchronous, loopback-only, and
uses incomplete device/font/UA and computed-value contracts; it is not live invalidation, a
product navigation pipeline, or CSS parity. Do not reintroduce `servo/ports/geckolib` as the final
boundary.

W3-A3S adds a bounded, engine-neutral `ScriptMutationBatch` over one exact `DocumentVersion`.
Existing nodes are document-checked and batch-created nodes use dense local tokens. Eight command
forms cover initial HTML element/text creation, tree insertion/removal, null-namespace HTML
attributes, and character data. Commands execute on a private same-identity arena and either
publish one validated snapshot plus one externally visible revision increment or leave the
original arena and node-slot allocation unchanged. Fixed command, creation, per-string, and total
string caps cannot be enlarged by callers. `NodeId` remains a lookup identity rather than a GC
root, and the existing root-provider/trace traits remain the future engine boundary.

This transaction is a correctness seam, not a scalable live DOM. It currently copies the complete
arena and has no Brimstone wrapper, event loop, MutationObserver delivery, style invalidation, or
frame scheduling. W3-A6D now connects direct synchronous engine calls to full
snapshot/Stylo/layout/text/scene/headless recomputation, and W4-A6E publishes that operation through
the bounded exact-navigation worker. It is still not a rooted script task, DOM event loop, or live
invalidation path. Before normal-page use, preserve the atomic/version contract while replacing
whole-arena copying with journaled or otherwise incremental mutation and connecting successful
commits to rooted script tasks, observer/event delivery, invalidation, and frame scheduling.

### Agent 4: graphics, GPU, images, and media

Default ownership:

- `gfx/`, `image/`, and `media/`
- `dom/canvas/`, `dom/webgpu/`, and `dom/media/`

Responsibilities:

- WebRender integration, display lists, compositor, surfaces, color, fonts, rasterization, clips, hit testing, and screenshots.
- Canvas 2D, WebGL, WebGPU, GPU-process protocols, resource lifetimes, and device-loss recovery.
- Image decoding, animation, audio/video demuxing, decoding, playback, capture, and output.
- Deterministic reftests, screenshot comparison, GPU validation, frame pacing, and media corpus tests.

Reuse before rewrite:

- `gfx/wr` for WebRender.
- `gfx/qcms` for color management.
- Imported `third_party/rust/wgpu-*` and `naga` source as the pinned WebGPU baseline.
- Imported `mp4parse`, `audioipc2`, and Cubeb Rust layers where their native dependencies are acceptable as tracked transition boundaries.

`gfx/webrender_bindings`, `gfx/wgpu_bindings`, and `media/mp4parse-rust` are Gecko adapters, not desired final interfaces. Replace their C++/XPCOM contracts with native Rust APIs.

Only the Rust core packages listed in `docs/upstream-components.toml` are admitted from the
`gfx/wr` snapshot. SWGL, Wrench, shader-to-C++ tooling, examples, and example compositor paths are
excluded from that workspace because they contain native or Gecko-dependent implementation. Do
not enable them as a shortcut; provide Rust-native Wild Buzzard equivalents or record an approved
third-party native boundary.

W4-A4P admits `gfx/wild_buzzard_linux_presenter` as the first direct Wayland/X11 EGL window-surface
proof. It owns the current context and surface, requires a hardware configuration and exact extent,
draws only through a private frame callback, verifies an initialized native-back-buffer diagnostic
sample before swap, and poisons at the first authoritative native fault even if a renderer remaps,
swallows, or panics after that fault. Its result is swap submission, not desktop-compositor
acknowledgement. It does not yet consume WebRender display lists, `CompiledScene`, shaped text,
worker frame leases, browser chrome, Canvas, WebGL/WebGPU, media, or AppImage output. The next gate
must add a renderer-owned WebRender window adapter without exporting GL authority and prove resize,
device loss, frame pacing, and teardown on both Linux backends.

### Agent 5: networking, security, and persistent storage

Default ownership:

- `netwerk/`, `security/`, and `storage/`
- Origin-storage and quota backends
- Selected provider-neutral application-services code

Responsibilities:

- DNS, sockets, proxies, HTTP/1.1, HTTP/2, HTTP/3, QUIC, cache, cookies, authentication, downloads, streams, backpressure, and cancellation.
- TLS, certificate verification, trust policy, content-security enforcement, process boundaries, and permission support.
- IndexedDB and local storage, quota, history, bookmarks, passwords, autofill, WebExtension storage, and profile migration.
- Loopback protocol servers, malformed-input testing, isolation tests, and secure failure behavior.

Reuse before rewrite:

- Imported Neqo crates for QUIC and HTTP/3.
- Imported WHATWG URL crates.
- `third_party/skv` and selected provider-neutral application-services components after their Mozilla Sync coupling is feature-gated away.

`netwerk/socket/neqo_glue` and URL/XPCOM glue are reference adapters, not final APIs.

NSS is C/C++, and `nss-rs` is only a binding. If NSS is used temporarily, isolate it behind a narrow tracked crypto interface. The target is an audited Rust-facing TLS/crypto implementation. Never weaken certificate validation, TLS, same-origin policy, CORS, CSP, sandboxing, or storage partitioning to unblock a milestone.

### Agent 6: browser product, UI, accessibility adapters, extensions, DevTools, and automation

Default ownership:

- `browser/` and product-facing `toolkit/`
- `devtools/`, `accessible/`, `extensions/`, and `remote/`
- `testing/geckodriver`, `testing/webdriver`, and browser-level integration harnesses

Responsibilities:

- Rust-native windows, tabs, address bar, navigation, menus, prompts, settings, downloads, history, bookmarks, passwords, permissions, sessions, and recovery.
- A stable browser-engine facade. Browser chrome must not access private DOM, layout, network, or renderer internals.
- Operating-system accessibility adapters over the semantic tree produced by DOM/layout.
- WebExtensions, DevTools protocol/UI, remote debugging, and WebDriver.
- Wild Buzzard branding and provider-neutral defaults.

UI parity means equivalent capability, interaction, keyboard behavior, accessibility, persistence, and comparable layout. It does not mean copying Firefox artwork or trademarks.

W3-A6D adds one bounded direct-engine live-document recomposition seam. A successful synchronous
load retains exactly one opaque mutable document. `L` is its exact live `DocumentVersion`; `F` is
the revision represented by the last frame successfully returned to the caller. A rejected
mutation batch leaves both unchanged. Once a batch commits, `L` advances exactly once and cannot be
rolled back by later style, layout, text, scene, cancellation, deadline, or renderer failure; `F`
advances only when the complete owned frame is returned. Successful renderer submission is
commit-wins: no fallible checkpoint follows it.

`rerender_live` requires exactly `L` and performs no fetch, parse, mutation, created-node mapping,
or revision increment. `F` is not proof of a backend surface's state after a post-send failure.
`renderer_is_usable() == false` is terminal for the engine and requires teardown/recreation;
`true` merely permits another attempt and predicts no success. Renderer epochs are monotone attempt
identifiers and may have gaps. Every update still performs complete immutable-snapshot Stylo,
layout, shaping, scene, and headless recomputation.

W3-A6D itself was not exposed through `NavigationEngine`. W4-A6E adds that bounded connection.
The worker owns independent opaque live pages per admitted `TopLevelContextId` on the executor
thread and accepts mutation, rerender, and close only for the exact current `NavigationId` and
`DocumentVersion`. A replacement load holds old and new pages in a private transaction until
generation-checked publication; failure, cancellation, supersession, or resource rejection cannot
silently install the hidden page. Live revision `L`, published-frame revision `F`, frame leases,
created-node result leases, and retained/pending node charges are updated as one checked publication
transaction.

Pending node reservations count across every context before navigation publication and dynamic
executor entry, then convert atomically to retained charge after an irreversible commit. A custom
executor cannot forge created allocations from raw `NodeId` values: success carries an opaque proof
derived from the DOM layer's private `ScriptMutationCommit`, and the worker rechecks topology,
document/version, cardinality, and uniqueness. Navigation-only replacement explicitly retires the
old typed document, node charge, frame, and mutation-result leases. Close names the exact current
navigation and permanently retires its numeric context identity behind a bounded monotone high
watermark so delayed controls cannot cross an ABA reuse boundary.

Navigation cancellation remains navigation-only. Every admitted mutation or rerender receives a
distinct `DocumentOperationId` containing a process-global never-reused engine incarnation and a
never-reused per-engine sequence. The receipt and every dynamic outcome carry that operation ID;
only the exact active navigation/operation pair may be cancelled. A completed or foreign operation,
a prior sequential operation under the same document, a restored-page replacement generation, a
superseding navigation, and a closed context must all reject stale cancellation. Engine-owner or
operation-sequence exhaustion fails closed rather than wrapping.

W4-A6E still has no Brimstone binding, DOM event/task/microtask loop, MutationObserver delivery,
incremental Stylo invalidation, exact arena/string/vector/RSS accounting, origin/process policy,
untrusted-script permission, window presentation, browser UI, or parity claim. See
`docs/handoffs/W3-A6D-dynamic-document.md` and
`docs/handoffs/W4-A6E-dynamic-navigation.md`.

## Dependency direction and required contracts

The intended high-level dependency direction is:

```text
browser product
      |
engine facade
      |
DOM/layout -------- JS/Wasm
   |                   |
network/storage        |
   |                   |
graphics/media --------+
      |
foundation + typed IPC + platform
```

No component may reach through another component's private implementation. Cross-owner dependencies require an orchestrator-approved public contract.

Required boundaries:

- DOM to JS: rooted handles and generated bindings; never expose unrooted GC pointers.
- JS to host: callbacks and capabilities; the runtime must not depend on concrete DOM types.
- DOM/layout to Stylo: snapshots, invalidation, computed values, and explicit ownership.
- Layout to graphics: immutable display lists, clips, hit-test data, and accessibility metadata; the renderer never owns DOM nodes.
- DOM Fetch to networking: asynchronous request/response streams with cancellation and backpressure. Networking owns transport; Web Platform owns CORS and browser semantics.
- Media, Canvas, and WebGPU DOM APIs to graphics/media: typed commands and resource handles.
- UI to engine: typed navigation, session, prompt, permission, download, history, diagnostics, and lifecycle events.
- Engine to platform: typed window, input, surface, clipboard, and accessibility events.
- Storage: origin, top-level site, private mode, and partition keys are explicit at every public boundary.
- IPC: messages are versioned and validated; never transmit process-local pointers or unchecked lengths.
- Wire IDs: reserve zero, scope message kinds to one registered protocol, and never reuse retired IDs.
- Accessibility: DOM/layout owns semantics and geometry; Agent 6 owns operating-system adapters.

Shared interface crates contain contracts and types, not component business logic. Avoid cyclic crate dependencies.

## Existing Rust import policy

Do not equate "contains `.rs` files" with "reusable Rust component." Classify every import:

1. Native reusable component: import its complete crate/workspace, tests, assets, build scripts, and licenses.
2. Reusable core plus Gecko adapter: import the core and replace the adapter at a Wild Buzzard contract.
3. Gecko/XPCOM glue written in Rust: use as reference or explicitly quarantined migration code; it is not a completed port.
4. Mechanically vendored dependency: keep pinned and unmodified or replace it with a canonical upstream workspace before development.
5. Provider-specific component: exclude unless a provider-neutral core is separated first.

For every adopted component:

- Import an exact source snapshot before adaptation.
- Keep the import mechanically reviewable; do not bulk-format it in the same change.
- Preserve licenses, notices, tests, assets, build scripts, schemas, and required generated data.
- Record source repository, commit or version, source and destination paths, license, local patches, and owning agent in `docs/upstream-components.toml`.
- Audit default features, telemetry, service endpoints, native dependencies, unsafe code, and generated Gecko bindings.
- Add a smoke test before calling the import usable.
- Do not edit `.cargo-checksum.json` or normalized Cargo vendor manifests as if they were canonical source. Move active development to a reviewed Wild Buzzard manifest or exact canonical upstream snapshot.

Current classifications:

- Independently buildable: the admitted WebRender Rust core workspace, `gfx/qcms`, `modules/libpref/parser`, and `third_party/skv`.
- Independently buildable nested workspaces: imported/adapted Stylo crates under `servo/`, the
  first-party `browser/wild_buzzard_engine` bounded static plus worker-exposed live-document recomposition
  seam, and the capability-free
  first-party Wasmtime adapter under `js/wasm`. The browser seam proves one synchronous loopback
  URL-to-WebRender path, has a generation-aware bounded worker/event facade for static and exact-document navigation,
  and can fully recompute an exact-version DOM batch without refetching. W2-A6C publishes one
  zero-pending composed page-and-text frame through the facade; W4-A6E gives its bounded contexts
  exact-navigation mutation/rerender/close commands and checked live-state leases/accounting. It is
  still not a browser product, page-content activation, or script event loop.
- Pinned component source awaiting canonical workspace integration: Neqo, wgpu/Naga, URL, mp4parse, audioipc/Cubeb, and authenticator imports under `third_party/rust`.
- Quarantined until provider coupling is removed: selected application-services Places, logins, autofill, and WebExtension storage code.
- Reference-only adapters: `servo/ports/geckolib`, `gfx/webrender_bindings`, `gfx/wgpu_bindings`, `netwerk/socket/neqo_glue`, most `xpcom/rust`, and `toolkit/library/rust`.
- Adopted engine baseline requiring hardening and browser adaptation: Brimstone under
  `js/brimstone`; it is neither production-ready nor an accepted parity implementation yet.
- Selected exact reusable compiler/runtime source requiring a browser-owned integration layer:
  imported Wasmtime v47.0.3/Cranelift under `js/wasmtime` for WebAssembly and Brimstone JIT work,
  plus the independently locked, product-disconnected first adapter at `js/wasm`. The complete
  JavaScript/browser/cross-heap integration layer remains unfinished. SpiderMonkey remains
  behavioral reference only.
- Rewrite track rather than reusable Rust engine: NSS/TLS.

Do not copy all of Firefox's `third_party/rust`. It contains hundreds of mechanically vendored versions, many unused by Wild Buzzard. Import only adopted component source and its reviewed dependency closure, or use Cargo with a locked dependency policy.

## Product scope, privacy, and branding

Do not port or introduce:

- Firefox Accounts or Mozilla-operated Sync integration.
- Pocket, Mozilla VPN, Relay, Monitor, sponsored content, advertising, or affiliate defaults.
- Glean, FOG, Firefox Telemetry, pings, studies, Normandy, or Nimbus experiments.
- Mozilla recommendations, messaging, remote campaigns, or automatic data submission.
- Firefox crash-upload, update, blocklist, or remote-settings endpoints.
- Mozilla API keys, credentials, distribution IDs, branded search deals, or provider defaults.
- Firefox icons, wordmarks, application IDs, profile names, or claims of Mozilla affiliation.

General browser functions remain in scope:

- Local history, bookmarks, passwords, autofill, downloads, permissions, and session recovery.
- User-configurable search and proxy support.
- Local diagnostics and crash dumps without automatic upload.
- Provider-neutral update, synchronization, blocklist, and threat-protection interfaces.
- Extensions, accessibility, developer tools, and WebDriver.

Removing a provider must not remove the associated security property. Certificate, revocation, safe-browsing, extension-blocklist, and application-update behavior need provider-neutral interfaces and explicit secure defaults.

No runtime network request may occur without a user action or a documented general-browser function. Automated tests default to loopback-only networking.

Use visible Wild Buzzard branding and `wild_buzzard_*` names for new first-party crates. Do not globally replace `Mozilla` or `Firefox` inside licenses, copyrights, imported compatibility tests, historical provenance, or standards data. User-agent compatibility tokens require an explicit architecture decision.

Proprietary DRM/CDM support and patented codec distribution require separate legal and product decisions and are not implied by the parity mission.

## Orchestration workflow

1. The orchestrator selects a thin end-to-end parity slice and allocates non-overlapping write scopes.
2. The component owner identifies Firefox implementation, tests, relevant history, existing Rust, security invariants, and provider dependencies.
3. If an interface is missing, the owner proposes a contract before privately implementing both sides.
4. Import reusable Rust separately from adaptation work.
5. Port behavior in reviewable increments with tests.
6. Run component gates and report exact commands, results, and skipped coverage.
7. Hand off cross-owner work instead of editing outside the assigned scope.
8. The orchestrator integrates, updates parity evidence, runs workspace gates, and accepts or rejects the slice.

The first bounded integration proof is:

```text
URL -> loopback HTTP -> HTML parse -> DOM -> Stylo -> layout -> display list -> WebRender
```

`browser/wild_buzzard_engine` executes that chain synchronously with explicit resource limits and
returns one real RGBA8 frame containing the admitted page primitives and every finalized shaped
text run. W2-A6C retains the exact canonical shaped inventory, projects `first_baseline`, calls
W2-A4D's checked one-display-list/one-transaction graphics path, requires zero pending text, and
publishes the result through W2-A6N's generation-aware lease boundary. This completes only the
bounded headless static-page milestone; it is not browser, UI, CSS, or rendering parity. W3-A6D
separately retains one direct live document and fully recomputes an exact bounded DOM transaction;
W3-A6W owns the reviewed native Wayland/X11 event shell, and W4-A4P connects that shell to a
bounded direct-GL EGL swap-submission proof. The next graphics integration gate is the typed
WebRender-to-window adapter and input routing; other open gates include rooted JS/DOM
bindings and document tasks, product browsing-context/navigation semantics, storage, normal
networking, and broader standards support.

## Shared-workspace rules

- Inspect `git status` before editing and preserve all user and agent changes.
- The orchestrator assigns one writer per path. Workers may read any live path but write only assigned paths.
- Workers must not edit root manifests, `Cargo.lock`, CI, architecture policy, provenance, or another agent's paths without a handoff.
- Do not run concurrent dependency-resolution, workspace formatting, or lockfile-generation operations.
- Component owners must never run a mutating `cargo fmt`, including with a component
  `--manifest-path`: Cargo can still traverse the root workspace and local path dependencies. Format
  only explicitly named owned Rust files with `rustfmt`; the orchestrator alone runs the
  workspace-wide non-mutating `cargo fmt --all -- --check` gate after writers are idle.
- Do not switch branches, commit, push, rebase, reset, clean, or rewrite history unless the user or orchestrator explicitly requests it.
- Never discard another agent's changes.
- A nested `AGENTS.md` may narrow local conventions but may not weaken reference-tree, privacy, security, licensing, parity, or ownership rules.
- Put cross-component handoffs under `docs/handoffs/` using its template.
- After a coherent wave passes component review and integrated external-build gates, the
  orchestrator stages only its exact reviewed paths, commits it, pushes the requested branch, and
  verifies the remote revision. Component agents do not commit or push independently.

## Build and test gates

### External build artifacts

All compilation, test, benchmark, coverage, profiling, generated corpus, and packaging artifacts
must be written outside this repository. The default Cargo configuration writes to the sibling
directory `../wildbuzzardbuilds/cargo`.

Concurrent agents must override `CARGO_TARGET_DIR` with a unique task-owned subdirectory so they do
not contend for or invalidate another agent's outputs. For example:

```sh
CARGO_TARGET_DIR=../wildbuzzardbuilds/agent-3-static-layout cargo test --workspace --locked
CARGO_TARGET_DIR=../wildbuzzardbuilds/agent-4-webrender \
  cargo test --manifest-path gfx/wr/Cargo.toml --workspace --all-features --locked
```

Non-Cargo tools must likewise receive an explicit output path below
`../wildbuzzardbuilds/<agent-or-task>/`. Do not put build directories, logs, screenshots, coverage
data, crash dumps, generated test corpora, or packaged applications in the live source tree.

Treat the shared build root as concurrent external state:

- An agent owns only the task-specific subdirectory assigned to it.
- Never delete or clean the whole `../wildbuzzardbuilds/` tree.
- Before deleting a task output, resolve and verify its exact path.
- Build outputs are disposable, must never be committed, and may not be used as source inputs.
- The repository must still build from a clean external target directory.

Until project wrappers exist, independently enabled workspaces use:

```sh
CARGO_TARGET_DIR=../wildbuzzardbuilds/check cargo fmt --all -- --check
CARGO_TARGET_DIR=../wildbuzzardbuilds/check \
  cargo check --workspace --all-targets --locked --target x86_64-unknown-linux-gnu
CARGO_TARGET_DIR=../wildbuzzardbuilds/check \
  cargo clippy --workspace --all-targets --locked --target x86_64-unknown-linux-gnu -- -D warnings
CARGO_TARGET_DIR=../wildbuzzardbuilds/check \
  cargo test --workspace --locked --target x86_64-unknown-linux-gnu
CARGO_TARGET_DIR=../wildbuzzardbuilds/check \
  cargo build --workspace --release --locked --target x86_64-unknown-linux-gnu
```

The static integration seam additionally needs the exact Python packages pinned by
`servo/style-build-requirements.txt`, installed only in an external task environment. Run its
locked gates with both `PYTHON3` and `CARGO_TARGET_DIR` pointing below
`../wildbuzzardbuilds/<task>/`; never create a virtual environment or target directory in the
repository.

The integrated root gate tests the exact Linux product feature set; it must not blindly add
`--all-features`. Imported manifests can retain comparison-only Gecko features, registry crates can
expose Windows debugger metadata, and future graphics crates can retain upstream DX12/Metal feature
names even though those paths are prohibited product code. Activating every feature would test a
different, unsupported product and can require excluded platform inputs. Component owners should
still use `--all-features` when it means all legitimate features of a platform-neutral first-party
crate (for example `cargo clippy -p wild_buzzard_js --all-targets --all-features`). Prohibited or
comparison-only features require an explicit negative compile gate proving that they fail closed;
Linux components with multiple supported backends require explicit positive feature combinations.
Record the precise product features and every negative gate in the component handoff.

Snapshot imports may have a documented temporary lint exemption. New Wild Buzzard code may not. Never claim a command passed if the workspace is not bootstrapped or the command was not run.

Required test layers:

- Unit, property, and contract tests for Rust crates and public interfaces.
- Test262 for ECMAScript.
- WebAssembly specification tests.
- Web Platform Tests for DOM and Web APIs.
- CSS tests, deterministic layout tests, and reftests.
- Network protocol, cancellation, isolation, and malformed-input tests.
- Media corpus and corruption tests.
- WebDriver product interaction tests.
- Accessibility-tree and keyboard-navigation tests.
- Privacy tests rejecting prohibited endpoints and unsolicited requests.
- Fuzzing for parsers, IPC, JS/Wasm, images, media, and network protocols.
- Linux x86_64 smoke, integration, and release tests under both supported window-system paths where applicable.

Firefox harnesses do not have to be ported literally. Preserve each test's observable assertion and upstream path in parity evidence.

Suggested CI tiers:

- Per change: formatting, check, clippy, component tests, contract tests, and focused conformance shards.
- Nightly: full workspace, WPT, Test262, Wasm suites, reftests, WebDriver, sanitizers, Miri where applicable, and fuzz jobs.
- Release: locked Linux x86_64 build, AppImage launch/relocation tests, UI/accessibility tests, privacy/network audit, dependency and license audit, and a published parity report.

## Definition of done

A component or parity slice is done only when:

- It is implemented in live-tree Rust and has no dependency on `firefox/`.
- Adopted code has provenance, intact licenses, and a dependency/native-code review.
- Public contracts are documented and exercised by integration tests.
- Focused tests and required enabled-workspace gates pass.
- Relevant Firefox, WPT, Test262, Wasm, reftest, or WebDriver behavior has recorded parity evidence.
- Unsupported behavior fails explicitly and safely rather than hiding behind an unrecorded skip.
- Security boundaries, cancellation, shutdown, and error paths are tested.
- New `unsafe` or native FFI is isolated and accompanied by documented `SAFETY` invariants.
- No telemetry, provider endpoint, credential, or Firefox-branded product asset was introduced.
- The parity matrix and handoffs state all remaining differences.

## Initial milestones

1. Establish the root workspace, core types, typed IPC contracts, provenance registry, test harnesses, and a headless executable.
2. Validate WebRender and qcms; adapt Stylo; establish canonical editable Neqo and wgpu workspaces.
3. Preserve the completed bounded static-page vertical slice: composed positioned text, typed
   navigation/event publication, and deterministic headless fixtures are established; expand its
   standards and malformed-input evidence without promoting it to parity.
4. Connect the reviewed W3-A6W Wayland/X11 event shell to a Wild Buzzard renderer/compositor
   presentation surface and input routing, then add tabs, address bar, and browser UI behavior.
5. Harden and integrate the Brimstone-backed JS runtime, its Linux x86-64 baseline/optimizing JIT,
   the Wasmtime-backed browser Wasm runtime, and generated DOM bindings; grow against Test262,
   Wasm specification tests, WPT, and SpiderMonkey regression behavior.
6. Add persistent storage, workers, Canvas/WebGPU, media, accessibility, extensions, DevTools, and multi-process hardening.
7. Close conformance, security, performance, Linux UI, and AppImage release gaps before claiming Firefox parity.
