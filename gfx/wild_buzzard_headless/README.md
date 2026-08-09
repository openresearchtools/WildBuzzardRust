# Wild Buzzard Linux headless WebRender boundary

This crate creates a real imported WebRender renderer and document on Linux
`x86_64-unknown-linux-gnu`, submits a validated
`wild_buzzard_renderer::CompiledScene` or an exact pre-shaped text frame as a
WebRender transaction, renders into an EGL pbuffer, and returns a bounded owned RGBA8 frame. Readback rows are
normalized from GL's bottom-left origin to top-left screenshot order.

This is a real frame/pixel result, not a claim of Firefox rendering parity. The
current production scene compiler sends solid backgrounds and borders and keeps
its text as an unchanged typed pending resource. The isolated
`render_shaped_text` seam proves real glyph pixels from an exact
`Arc<ShapedText>` without reshaping that pending record. Images, gradients, transforms, stacking contexts,
filters, Canvas, WebGL/WebGPU, color management, compositor integration, and
normal browser-window presentation remain later work.

## Data flow

```text
immutable LayoutOutput
  -> wild_buzzard_renderer validation + real BuiltDisplayList
  -> bounded headless validation
  -> WebRender Transaction (display list, root pipeline, epoch, frame request)
  -> imported WebRender scene builder + render backend
  -> imported WebRender Renderer on current desktop GL 3.2 context
  -> Linux EGL RGBA8 pbuffer
  -> fallibly reserved RGBA8 Vec
  -> top-left-oriented RgbaFrame

exact Arc<ShapedText>
  -> wild_buzzard_text_webrender validation + renderer-scoped font registry
  -> same-transaction raw-font/instance additions + glyph display list
  -> the same imported WebRender/EGL/readback path
```

The primary context path enumerates EGL devices and creates an EGL display from
the selected device, so no X11 or Wayland window is needed. If EGL device
enumeration or every device context fails, policy may permit one Linux-only
fallback using EGL's default X11 display with a pbuffer. Every failed source and
stage is returned as a bounded `ContextAttempt`; there is no Wrench, C++ SWGL,
fake display list, or independent CPU rasterizer fallback.

`reject_software_rasterizer` is deliberately false: a system Mesa llvmpipe or
softpipe GL context remains a real imported WebRender/GL execution path and is
useful for Linux CI. It is not a performance or hardware-GPU guarantee. This
does **not** enable WebRender SWGL, Wrench, or a Wild Buzzard toy rasterizer;
those paths are absent from the dependency graph.

The crate deliberately emits a compile error on every target except Linux
x86_64. Dependency crates can contain inactive upstream platform source, but
this first-party implementation has no Windows, macOS, Android, iOS, or other
architecture path.

## Contracts and bounds

- The pbuffer has a fixed non-zero `FrameSize`; dimensions must fit `i32` and
  configured width/height maxima.
- A scene viewport must exactly match that pbuffer at device scale 1 and must be
  an integral number of CSS pixels (60 Wild Buzzard app units per CSS pixel).
- The caller supplies the exact immutable `DocumentVersion` (document identity
  plus local revision) and a strictly increasing, non-reserved WebRender epoch.
  The requested version must exactly match the scene. A lower revision is
  rejected when the immediately preceding submission has the same document
  identity; a distinct document may legitimately have a lower local revision.
  Stale epochs and `Epoch::invalid()` are rejected before transaction
  submission. Switching pipeline IDs removes the superseded display list in
  the replacement transaction instead of retaining an unreachable pipeline.
- Scene items, pending text records, serialized display-list bytes, dimensions,
  and exact `width * height * 4` output bytes are bounded again at this boundary.
- The output allocation uses `try_reserve_exact` before its length is set.
- One capacity-one channel is used for each expected WebRender checkpoint. A
  single deadline, capped at 60 seconds by configuration validation, bounds the
  asynchronous waits before and after renderer submission. Timeout or
  transaction loss poisons the renderer so a caller cannot accidentally reuse
  asynchronous state of uncertain age.
- The render thread waits for both the backend `FrameBuilt` checkpoint and the
  later `new_frame_ready` callback before its nonblocking `Renderer::update`.
  WebRender sends renderer-side notification requests between those events;
  this ordering prevents a false `FrameRendered` timeout caused by ingesting
  the queue too early.
- Every GL update, render, readback, and deinitialization first verifies that
  this instance's exact EGL context and pbuffer are current, restoring them if
  another same-thread renderer superseded the current context.
- A same-thread `Renderer::render`, pixel readback, or native driver call cannot
  be preempted by a Rust deadline. A wedged GL driver therefore requires the
  browser's eventual GPU-process isolation and watchdog/restart policy; this
  in-process slice does not claim a hard wall-clock bound over driver code.
- Explicit shutdown asks the backend to stop asynchronously, waits at most the
  configured (maximum 30 second) budget for its notifier acknowledgement,
  deinitializes WebRender while GL is current, then makes EGL non-current.
  Renderer-deinit and context-release panics are caught independently so local
  cleanup continues and explicit shutdown returns a structured diagnostic. If
  a broken EGL implementation refuses to unbind a current context, its native
  owners are intentionally retained instead of being destroyed while current;
  shutdown returns `ContextRelease` rather than risking deferred destruction or
  use of an unrelated context.
- `RgbaFrame` owns exactly one tightly packed RGBA8 buffer and records its
  `DocumentVersion`, epoch, stride, and the count of text runs intentionally
  left pending.
- Shaped-text frames validate the reserved epoch, pipeline identity, UTF-8
  ranges, metrics, finite positions, complete font identity/bytes, and resource
  bounds again. Font and instance keys belong to exactly one checked WebRender
  namespace, are reused only by full identity, and are explicitly deleted at
  shutdown (instances before fonts). A prepared frame exclusively borrows its
  registry, and additions are committed to live registry state only after the
  same transaction that first uses them has been accepted by WebRender.

`DocumentVersion` is publication identity, not a navigation-generation token.
Because this low-level owner retains only the immediately preceding submitted
version, an `A@10 -> B@1 -> A@5` sequence is not recognized as a regression of
the older A document. The current synchronous `StaticPageEngine` cannot
reintroduce retained scenes, but a future asynchronous product facade must add
a monotonic navigation-generation/capability and enforce it at presentation.

## Native and unsafe audit

All first-party unsafe operations are isolated in `src/linux_egl.rs`:

1. create a display from a `glutin`-enumerated EGL device;
2. request EGL's documented default X11 display for the optional fallback;
3. enumerate configs from that initialized display;
4. create a context from a config returned by that display;
5. create a pbuffer from the same config and validated non-zero dimensions;
6. load desktop GL functions through the exact current EGL display.

Each call has a local `SAFETY` explanation. The context is a glutin type that is
not `Send`, so the renderer and its current context remain on one thread. No raw
native handle, pointer, or GL object escapes the module.

The active upstream WebRender component already records its transitional native
boundaries in `docs/upstream-components.toml`:

- glutin dynamically loads the system EGL implementation (`libEGL.so.1`);
- gleam calls the GL entry points returned by EGL;
- WebRender's shader build uses its recorded `glslopt` native build dependency;
- WebRender's Linux glyph rasterizer links system FreeType through the explicit
  `static_freetype` feature. On Linux the feature's `freetype-sys` build first
  resolves `freetype2` with `pkg-config`; the tested host selected
  `libfreetype.so`. The shaped-text test now exercises this glyph-rasterization
  boundary; AppImage self-containment remains unproved.

No new C or C++ implementation was added. FreeType and shader-tool removal or
replacement is component-level migration work, not hidden by this crate.

## Reference research

The read-only Firefox checkout was inspected at the pinned ESR153 baseline
`c19b7e89270787889495688244ec6ee8e79288a1`, including full history:

- `gfx/wr/webrender/src/renderer/init.rs` — renderer/API construction;
- `gfx/wr/webrender/src/render_api.rs` — atomic display-list/root-pipeline/frame
  transactions and shutdown;
- `gfx/wr/webrender/src/render_backend.rs` — `FrameBuilt` publication ordering;
- `gfx/wr/webrender/src/renderer/mod.rs` — `update`, `render`, bounded-slice
  `read_pixels_into`, epoch publication, and GL-live `deinit` ordering;
- `gfx/gl/GLContextProviderEGL.cpp` and `gfx/gl/GLLibraryEGL.cpp` — Firefox EGL
  pbuffer selection and headless/offscreen context behavior;
- `gfx/webrender_bindings/RenderCompositorEGL.cpp` — Linux EGL surface/current
  context lifecycle, used as behavioral reference only;
- `gfx/wr/wrench/src/png.rs`, `reftest.rs`, and `rawtest.rs` — screenshot
  readback, vertical normalization, notifications, and pixel comparisons,
  inspected as excluded reference code only;
- `widget/headless/HeadlessCompositorWidget.cpp` and `HeadlessWidget.cpp` —
  Firefox product headless behavior, not copied or linked.

Relevant full-history points included `18a5c1737dd7` (minimal EGL pbuffer request,
2012), `418fa19b60c9` (WebRender screenshot infrastructure split, 2019),
`d14bbba400f6` (multiple EGL displays, 2020), and `a7c2899fb724` (Wrench's glutin
0.32 update, 2026). History informed lifecycle and API use; excluded Wrench and
Gecko C++ adapters were not imported into this implementation.

## Tests and gates

`tests/real_frame.rs` exercises the imported renderer and EGL context. It proves:

- stale revision, stale epoch, reserved-invalid epoch, viewport mismatch, and
  scene-resource rejection;
- explicit context-unavailable diagnostics without relying on host failure;
- exact bounded output length and top-left row order;
- stable byte-for-byte output across two real WebRender submissions;
- deterministic replacement while alternating root pipeline IDs;
- safe context restoration while two real renderer/context pairs remain live,
  alternate frames, and shut down independently;
- known solid border, background, and clear pixels;
- pending text is reported without a fake text display item;
- backend acknowledgement, WebRender deinit, and EGL release on clean teardown.

`tests/shaped_text_frame.rs` additionally proves that real HarfRust glyph IDs
and positions create non-clear framebuffer pixels, repeated exact faces and
instances are reused, a new size creates only a new instance, a new renderer
starts with a fresh namespace, and teardown deletes every registered resource.

All output remains below the external build tree. The standalone commands are:

```sh
rustfmt --edition 2024 --check \
  gfx/wild_buzzard_headless/src/error.rs \
  gfx/wild_buzzard_headless/src/frame.rs \
  gfx/wild_buzzard_headless/src/headless.rs \
  gfx/wild_buzzard_headless/src/lib.rs \
  gfx/wild_buzzard_headless/src/linux_egl.rs \
  gfx/wild_buzzard_headless/src/notifier.rs \
  gfx/wild_buzzard_headless/tests/real_frame.rs \
  gfx/wild_buzzard_headless/tests/shaped_text_frame.rs
CARGO_TARGET_DIR=../wildbuzzardbuilds/agent-4-headless-wave2 \
  cargo check --manifest-path gfx/wild_buzzard_headless/Cargo.toml --all-targets --locked
CARGO_TARGET_DIR=../wildbuzzardbuilds/agent-4-headless-wave2 \
  cargo clippy --manifest-path gfx/wild_buzzard_headless/Cargo.toml \
  --all-targets --all-features --locked -- -D warnings
CARGO_TARGET_DIR=../wildbuzzardbuilds/agent-4-headless-wave2 \
  cargo test --manifest-path gfx/wild_buzzard_headless/Cargo.toml --locked
CARGO_TARGET_DIR=../wildbuzzardbuilds/agent-4-headless-wave2 \
  cargo build --manifest-path gfx/wild_buzzard_headless/Cargo.toml --release --locked
CARGO_TARGET_DIR=../wildbuzzardbuilds/agent-4-headless-wave2 \
  RUSTDOCFLAGS=-Dwarnings cargo doc \
  --manifest-path gfx/wild_buzzard_headless/Cargo.toml --no-deps --locked
```

## Integration handoff

The crates are root-workspace members. The next cross-owner step is for layout
to retain and hand off the exact `Arc<ShapedText>` it measured, replacing a
specific pending record through an orchestrator-approved scene contract. Until
that contract exists, the production compiler must keep `PendingTextRun`
explicit and must not call the isolated method as an implicit reshaping path.
The browser/GPU facade must keep the renderer on its creating thread and call
`shutdown` explicitly. No integration may depend on the ignored `firefox/`
tree.
