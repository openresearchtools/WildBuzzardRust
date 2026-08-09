# W5-A4Q WebRender native-window adapter handoff

## Scope and decision

- Task: nest one internally owned WebRender renderer in the exact W4-A4P Linux EGL presenter and
  submit one immutable Wild Buzzard scene through WebRender into a real Wayland and X11/Xwayland
  native window without a CPU frame-copy path.
- Owner: Agent 4 — graphics/GPU; root integration and independent acceptance remain with the main
  orchestrator.
- Status: GO for this bounded same-process presentation prerequisite after the final static gates
  and live-backend evidence below. It remains NO-GO for a browser/UI, GPU-process, compositor
  acknowledgement, recovery, frame-pacing, or AppImage claim.
- Writable scope: `gfx/wild_buzzard_linux_presenter/**`, this handoff, and the one read-only
  `CompiledScene::pipeline` accessor/test under `gfx/wild_buzzard_renderer/src/**`. No widget,
  root manifest, toolchain, CI, or Firefox-reference source was changed by this component owner.
- Root integration: the orchestrator admitted the five existing runtime dependency paths and
  refreshed the root lockfile. The example's `wild_buzzard_layout` dev dependency is already an
  admitted workspace package and introduced no new third-party version.

The earlier 43-test/live-smoke result was rejected by hostile review and is superseded. It did not
cover suspended scale-state divergence, honest absent/unknown startup evidence, post-worker-spawn
constructor failure, all synchronous deadline boundaries, late-swap accounting, early pipeline
rejection, or prompt external-event wakeup. A later 49-test repair was also rejected: its abort
helper could panic while writing a diagnostic to unusable standard error, and a sticky external
event which preceded stage-waiter registration could degrade to timeout. The final 51-test
corrective evidence recorded below supersedes both rejected results and alone supports this GO
decision.

`LinuxPresentedWindow::into_webrender` consumes the exact native presenter and returns a
thread-affine `WebRenderPresentedWindow`. That owner contains the presenter, one hardware-only
WebRender `Renderer`, its API/document, one fixed-state notifier, a renderer-scoped
`TextFontRegistry`, and the surface/transaction state machine. It exposes no renderer, GL, EGL,
winit, Wayland, X11, or raw-handle authority. The earlier callback-scoped solid-color diagnostic is
retained for native-presenter regression evidence; it is not used by the WebRender path, and no
second renderer is constructed.

The accepted input is the existing immutable `wild_buzzard_renderer::CompiledScene` and the exact
ordered `wild_buzzard_text_webrender::ShapedSceneText` inventory. The adapter does not recompile
layout, invent a parallel display-list format, rasterize a scene on the CPU, read the completed
frame back, or upload a screenshot. WebRender renders directly to framebuffer zero on the exact
current EGL window surface and the presenter submits that back buffer with EGL.

## Exact frame contract

Every request binds all of these values before transaction acceptance:

- generational `SurfaceId`, complete `Rgba8Srgb`/`Window` descriptor, physical extent, and a
  presenter-created nonzero `WebRenderSurfaceRevision` which never reuses an older configuration;
- exact scene `DocumentVersion` and `PipelineKey`;
- a nonreserved strictly increasing WebRender epoch and a nonzero strictly increasing native swap
  sequence; and
- the compiled viewport, item count, pending-text count, shaped-text count, and serialized
  display-list size.

The fixed caller-nonenlargeable limits are 1,000,000 scene items, 100,000 pending shaped-text
records, 128 MiB of display-list data, and the underlying presenter's 16,384-pixel axis,
67,108,864-pixel, and 256 MiB RGBA8-equivalent surface limits. The shaped-text slice is compared
with the already bounded pending-text count before any descriptor reservation, so a hostile
oversized slice cannot drive an unbounded allocation.
`CompiledScene::pipeline()` exposes only the existing renderer-independent `PipelineKey`; the
adapter validates it at `ValidateRequest` before descriptor reservation, registry/API allocation,
text-key preparation, or composition. The continuation which can perform those operations is not
called for a mismatch.

One ten-second deadline is created before composition and reused at every asynchronous checkpoint
and relevant synchronous boundary. It is not a preemptive wall-clock interrupt for a blocked
driver call or synchronous scene/WebRender operation; elapsed time is checked immediately after
each returned boundary and before continued publication. The fixed-state
notifier uses checked counters, mutex/condition state, and two single-use capacity-one checkpoint
channels rather than an unbounded event queue. Timeout, sender disconnect, transaction drop, wrong
checkpoint, arithmetic exhaustion, foreign document/publish flags, and unexpected external
renderer events remain distinct fail-closed outcomes. Upstream `NotificationRequest` is admitted
as exactly once; capacity-one duplicate detection is defense in depth. An unauthorized external
event is sticky. A stage waiter checks the flag before deadline classification, immediately before
blocking, after timeout or disconnect, and after receiving a signal. This covers an event which
precedes waiter registration; once registered, the event also signals both checkpoint channels.
Frame-ready and shutdown condition variables are woken as well, so the event remains a distinct
prompt failure rather than degrading to timeout or disconnect.

The successful order is:

```text
validate immutable pipeline, scene/text, and native identity
  -> prepare renderer-scoped text resources and compose the display list
  -> send one atomic document-view/resource/display-list/frame transaction
  -> exact FrameBuilt and frame-ready evidence
  -> make the exact EGL context/window surface current and verify its extent/default framebuffer
  -> Renderer::update
  -> exact pipeline epoch
  -> Renderer::render directly into framebuffer zero
  -> exact FrameRendered
  -> checked GL state
  -> final pre-swap deadline check
  -> eglSwapBuffers
  -> commit outer swap sequence/accounting
  -> final post-swap deadline check
  -> publish receipt
```

`WebRenderWindowFrameReceipt` proves that the backend built the transaction, WebRender completed
its renderer submission, and EGL accepted the swap for the exact request. Its nonzero backend
publish ID and bounded RGBA8-equivalent byte count are evidence/accounting, not copied pixel data.
Neither WebRender nor EGL provides a desktop-compositor latch/display acknowledgement, so
`desktop_compositor_acknowledged()` is always false.
If EGL accepts the swap after the deadline, the outer sequence/frame count is committed first to
mirror the native presenter's already committed accounting; the combined owner then becomes
`Lost(SwapBuffers)` and returns `SwapBuffers/Timeout` without constructing a receipt.

## Resize, failure, and teardown

Resize, scale, explicit suspend, and resume require the exact last
`WebRenderSurfaceSnapshot`. The native mutation must succeed before the contract publishes the next
non-reusing revision. Stale revisions are rejected even when the requested extent happens to equal
an older one. Zero-size resize or explicit suspend removes the EGL window-surface wrapper while
retaining renderer/context ownership; nonzero resize or resume re-establishes the native surface
and forces a full WebRender draw. Scale changes publish a new revision without fabricating a
physical resize. Tests cover stale replay, explicit suspend/resume ordering, revision exhaustion,
resource bounds, and epoch/pipeline/sequence monotonicity.
Scale-only mutation while explicitly suspended preserves `Suspended` even though the retained
descriptor has a nonzero extent; frame admission continues to reject until an exact resume. Any
native identity/size/sequence/scale rejection after the outer layer admitted the same operation is
terminal `InternalDrift`, not a retryable caller mismatch. Resize resource bounds are checked by
the outer layer before native admission so legitimate oversize rejection remains retryable.

Stable error stages distinguish request validation, scene composition, renderer initialization,
transaction submission, backend checkpoints, native preparation, renderer update/epoch/draw,
swap, surface transitions, backend shutdown, renderer deinitialization, and presenter shutdown.
A deadline exceeded during pre-transaction `ComposeScene` is explicitly retryable because no
transaction or sequence was committed. Backend, notification, GL, context/device,
native-identity, ownership, post-acceptance timeout, or panic faults latch the combined owner
`Lost`; it is not retried through a stale renderer or surface. Error diagnostics are UTF-8 safe
and bounded to 2,048 bytes.

Startup preserves the original initialization failure separately from the cleanup result for the
consumed partial owner. `WebRenderTeardownEvidence` represents backend shutdown and renderer
deinitialization separately as `Confirmed`, `NotApplicable`, or `Unknown`; neither absent nor
unknown work is encoded as affirmative completion. Compatibility boolean getters return true only
for `Confirmed`.

Focused inspection of pinned `create_webrender_instance` found that shader, maximum-texture,
software-rasterizer, and constructor OOM errors return before its Rayon, scene-builder, or render-
backend workers are created. Those ordinary rejections return structured cleanup with backend and
renderer stages `NotApplicable`, while native wrapper release remains independently exact. A
`RendererError::Thread` can occur after workers or the scene thread have spawned, and a constructor
panic has no externally observable spawn stage. A `RenderApiSender::create_api` panic occurs after
successful worker startup but leaves no trustworthy API shutdown path. The imported constructor
exposes no RAII guard/join handles with which this crate could prove those workers exited, so a
pure total policy maps all three unprovable cases to fail-closed process abort. It never returns a
success-like cleanup report with possibly stranded workers. The abort helper performs no
diagnostic formatting, allocation, unwinding, or fallible I/O before its unconditional
`process::abort`, so unusable standard error cannot divert this terminal path into a panic. Once a
`RenderApi` exists, partial startup cleanup can request backend shutdown; missing acknowledgement
remains `Unknown` and causes renderer/native retention. Backend workers own no GL/EGL/window
handles, while the same-thread `Renderer` owns GL resources.

Explicit shutdown performs this sequence:

1. release renderer-scoped text resources into the final transaction, delete the document, and
   request WebRender backend shutdown;
2. require the notifier's backend shutdown acknowledgement within one fixed five-second deadline;
3. make the exact retained EGL context/surface current (or the retained context surfaceless when
   suspended);
4. call `Renderer::deinit` while that context is current; and
5. only then invoke the presenter's checked non-current and wrapper-release transaction.

The result records typed backend-shutdown and renderer-deinitialization evidence, released font
template/instance/byte counts, and the nested native release-or-retention evidence. If steps 2–4
cannot be proved, the renderer is forgotten and the presenter deliberately retains every still
live native owner. If final presenter release fails, its exact retention report is preserved. The
`Drop` path uses the same cleanup policy; an escaped cleanup panic also retains uncertain owners.
Only notification waits are deadline-bounded. `Renderer::update`, `Renderer::render`,
`Renderer::deinit`, driver/GL operations, EGL swap, and EGL release are synchronous and cannot be
preempted by this adapter if they hang. This proves Rust-side ordering and checked wrapper release
only, not native EGL destructor acknowledgement, which glutin does not expose.

## Firefox implementation and history inspected

The reference remained the read-only detached ESR153 checkout at
`c19b7e89270787889495688244ec6ee8e79288a1`. Focused implementation and ownership review covered:

- `gfx/webrender_bindings/RenderCompositorEGL.cpp/.h`;
- `gfx/webrender_bindings/RenderCompositorOGL.cpp`;
- `gfx/webrender_bindings/RenderThread.cpp/.h`; and
- `gfx/gl/GLContextProviderEGL.cpp`.

The full-history review included `ab9879a559cba5d8593cae1b9d061ecedc27f335` (make an EGL surface
non-current before destruction), `2835f8b9a9396a363aa36e4ea46664d2701ab284` (Wayland surface
deletion/resume), and `abd58ab1be7a5a54e80ba7cddc87a31b48c81445` (refresh X11 EGL surface
on compositor resume), plus recent compositor resize/atomic-swap history inspected by path. Wild
Buzzard adopts the observable ordering, identity, and fail-closed lessons, not Gecko's C++ object
graph; Firefox is not a build or runtime input.

## Dependencies and unsafe boundary

The runtime delta is local `gfx/wr/webrender` with defaults disabled and only `static_freetype`,
local `gfx/wr/webrender_api`, and first-party `wild_buzzard_dom`,
`wild_buzzard_renderer`, and `wild_buzzard_text_webrender`. The renderer change is limited to the
read-only pipeline accessor and its unit test. The opt-in smoke alone adds the
first-party `wild_buzzard_layout` dev dependency to build a genuine scene. No third-party version,
WASI/ambient capability, software renderer, capture/debugger path, or alternate GL owner was added.
The exact already admitted native dependency and runtime-soname analysis remains in
`docs/handoffs/W4-A4P-linux-presenter.md`; AppImage closure is still open.

`window_contract.rs`, `window_notifier.rs`, and `webrender_window.rs` forbid first-party unsafe.
The new presenter's WebRender bridge adds only crate-private safe operations around the already
audited EGL/GL owner. Existing unavoidable glutin/raw-EGL/gleam calls remain localized in
`egl_window.rs`, with the same exact-owner safety proofs. No first-party C or C++ was introduced.

## Verification evidence

All final acceptance build output was external under
`/home/user/Documents/wildbuzzardbuilds/w5-a4q/`, with
`TMPDIR=/home/user/Documents/wildbuzzardbuilds/w5-a4q/tmp`. The final toolchain was Cargo 1.96.0
(`30a34c682`) and rustc 1.96.0 (`ac68faa20`, LLVM 22.1.2), host and target
`x86_64-unknown-linux-gnu`.

```sh
export CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w5-a4q
export TMPDIR=/home/user/Documents/wildbuzzardbuilds/w5-a4q/tmp

cargo fmt -p wild_buzzard_linux_presenter -p wild_buzzard_renderer -- --check

cargo test -p wild_buzzard_linux_presenter --lib --locked \
  --target x86_64-unknown-linux-gnu -- \
  webrender_window::tests::abort_unproven_startup_reaches_sigabrt_with_unusable_stderr \
  --exact --nocapture
cargo test -p wild_buzzard_linux_presenter --lib --locked \
  --target x86_64-unknown-linux-gnu -- \
  window_notifier::tests::event_before_stage_waiter_registration_fails_immediately_and_distinctly \
  --exact --nocapture

cargo check -p wild_buzzard_linux_presenter -p wild_buzzard_renderer --all-targets --locked \
  --target x86_64-unknown-linux-gnu
cargo test -p wild_buzzard_linux_presenter -p wild_buzzard_renderer --all-targets --locked \
  --target x86_64-unknown-linux-gnu
cargo clippy -p wild_buzzard_linux_presenter -p wild_buzzard_renderer --all-targets --locked \
  --target x86_64-unknown-linux-gnu -- -D warnings

cargo check -p wild_buzzard_linux_presenter --all-targets --locked \
  --target x86_64-unknown-linux-gnu --features real-webrender-window-smoke
cargo test -p wild_buzzard_linux_presenter --all-targets --locked \
  --target x86_64-unknown-linux-gnu --features real-webrender-window-smoke
cargo clippy -p wild_buzzard_linux_presenter --all-targets --locked \
  --target x86_64-unknown-linux-gnu --features real-webrender-window-smoke -- -D warnings

cargo test -p wild_buzzard_linux_presenter -p wild_buzzard_renderer --doc --locked \
  --target x86_64-unknown-linux-gnu
cargo test -p wild_buzzard_linux_presenter --doc --locked \
  --target x86_64-unknown-linux-gnu --features real-webrender-window-smoke
RUSTDOCFLAGS='-D warnings' cargo doc -p wild_buzzard_linux_presenter \
  -p wild_buzzard_renderer --no-deps --locked --target x86_64-unknown-linux-gnu
RUSTDOCFLAGS='-D warnings' cargo doc -p wild_buzzard_linux_presenter --no-deps --locked \
  --target x86_64-unknown-linux-gnu --features real-webrender-window-smoke

cargo build -p wild_buzzard_linux_presenter --locked \
  --target x86_64-unknown-linux-gnu --features real-webrender-window-smoke \
  --example webrender-window-smoke
timeout 40s env WILDBUZZARD_REAL_WEBRENDER_WINDOW_TEST=1 \
  WILDBUZZARD_DISPLAY_BACKEND=wayland \
  /home/user/Documents/wildbuzzardbuilds/w5-a4q/x86_64-unknown-linux-gnu/debug/examples/\
webrender-window-smoke
timeout 40s env WILDBUZZARD_REAL_WEBRENDER_WINDOW_TEST=1 \
  WILDBUZZARD_DISPLAY_BACKEND=x11 \
  /home/user/Documents/wildbuzzardbuilds/w5-a4q/x86_64-unknown-linux-gnu/debug/examples/\
webrender-window-smoke

git diff --check -- gfx/wild_buzzard_linux_presenter \
  gfx/wild_buzzard_renderer/src/compiler.rs gfx/wild_buzzard_renderer/src/contract.rs \
  docs/handoffs/W5-A4Q-webrender-window.md
rg -n '[[:blank:]]+$' gfx/wild_buzzard_linux_presenter \
  gfx/wild_buzzard_renderer/src/compiler.rs gfx/wild_buzzard_renderer/src/contract.rs \
  docs/handoffs/W5-A4Q-webrender-window.md
```

The final hostile-rereview corrective result was: formatting, default and feature all-target
checks, strict Clippy, warning-denied rustdoc, and the example build passed. The default all-target
test ran 51 presenter tests, two renderer unit tests, and 22 renderer integration tests; all 75
passed. The feature all-target test reran all 51 presenter tests and compiled and ran the example
test harness with zero tests, without opening a display. Both presenter compile-fail doctests
passed under the default and feature configurations; the renderer has no doctests.
Focused repair tests cover suspended scale/resume and frame rejection, outer/native drift,
pre-allocation pipeline rejection, typed startup evidence and total abort dispositions, prompt
external-event failure, event-before-waiter-registration, a real subprocess abort with verified
write-failing standard error, and injected late
prepare/update/render/pre-swap/post-successful-swap boundaries. The subprocess installed a panic
hook which would return ordinary exit code 86 if fallible diagnostic I/O panicked; only
signal-terminated Linux `SIGABRT` was accepted. Deadline injection proves stage placement and exact
accounting; it does not forcibly preempt a synchronous call.

The live host was Ubuntu 26.04 LTS, Linux 7.0.0-28-generic, GNOME Wayland at
`WAYLAND_DISPLAY=wayland-0`, with Xwayland at `DISPLAY=:0`. The same final built example was run as
two separate opt-in processes. Each process forced the requested backend, ran winit on the child
process's main thread behind a 25-second parent kill deadline, compiled a real minimal
`LayoutOutput` into a genuine empty-text `CompiledScene`, observed one nonzero backend publish ID,
completed one direct WebRender draw and EGL swap, lingered 750 ms, and completed the ordered
backend/renderer/presenter teardown with both optional stages `Confirmed`. Exact stdout and status
were:

```text
W5-A4Q wayland WebRender publish=1 EGL swap=accepted compositor_ack=false
exit status: 0

W5-A4Q x11 WebRender publish=1 EGL swap=accepted compositor_ack=false
exit status: 0
```

The identical binary used for both live processes had SHA-256
`179a0249651f168f91c1873ebeab9e81cfa29521fc20a5c95808d25b7b27415a`. Its hash was checked
before Wayland, between the Wayland and X11 executions, and after X11. The final source and
manifest evidence was frozen at these SHA-256 values:

```text
6de643b5f0604fc6e8a34d3712dfe5640c813f7d374f43540b1350693ed663ad  gfx/wild_buzzard_linux_presenter/Cargo.toml
72c16e275ea1290a059cb536bc1610b6296c8febfeef7326124a356e453c97f0  gfx/wild_buzzard_linux_presenter/README.md
9606b9a50aa514e275744439e7c0d9adb27f14c8a13a792e887f54af0d559140  gfx/wild_buzzard_linux_presenter/examples/webrender_window_smoke.rs
08b539f768db9e1505a1a93e610bd75285e733e0126ca7c409bd71b99154ba5f  gfx/wild_buzzard_linux_presenter/src/egl_window.rs
1b62037527c2da9c3e633524994c84d09693614bd34efc79d80e2c5d36f8f936  gfx/wild_buzzard_linux_presenter/src/lib.rs
37c275cda985b341efcc376ca3af4cacae97d9ac8e354dd8827fed02f143d864  gfx/wild_buzzard_linux_presenter/src/webrender_window.rs
cdbab575ac3783a81c6daac49f147f5ebc2d951c96812f69ed30e7f47e71e3d5  gfx/wild_buzzard_linux_presenter/src/window_contract.rs
db70d42866c43b519dd8683d6a4288edfe7886217cae506cca26409b775dcad0  gfx/wild_buzzard_linux_presenter/src/window_notifier.rs
ad296d99b2a776e8e631f3ca7c0004f0e40f2e004bde8303e1055fe48d82f294  gfx/wild_buzzard_renderer/src/compiler.rs
f46be62d739037412301e127aba88821857aeb69e4eac3ac3439dfce80dcd603  gfx/wild_buzzard_renderer/src/contract.rs
```

Neither live run read back or uploaded a frame, proved that the desktop compositor displayed it,
exercised nonempty shaped text, or observed a native EGL destructor result.

## Explicit non-claims and next work

This gate does not connect the live browser engine, navigation, tabs, or chrome to presentation;
exercise nonempty shaped text in the live smoke; implement damage/buffer age, frame callbacks,
vsync/pacing, multiple windows, GPU-process IPC/isolation, device-loss recovery, context fallback,
hang recovery, compositor/display acknowledgement, Canvas/WebGL/WebGPU/media, accessibility, or
AppImage packaging. Synchronous driver and renderer calls are not preemptively cancellable by the
checkpoint deadline.
The scene/display-list/surface limits do not bound total WebRender/GPU memory, process RSS, shader
or cache storage, driver allocations, or imported worker-thread count, stacks, and CPU use. The
live smoke's shaped-text inventory is empty.

The next integration must carry the typed immutable scene/surface contract from the engine/browser
shell without exporting graphics authority. Later graphics gates must add nonempty-text live
evidence, explicit frame pacing/damage, GPU-process ownership and crash isolation, device-loss
reconstruction, and packaged Wayland/X11 dependency-closure tests before any broader parity claim.
