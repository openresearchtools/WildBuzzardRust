# W6-A4R Rust browser compositor handoff

## Scope and decision

- Task: extend the W5-A4Q renderer-owned Linux window into one durable Rust browser-composition
  boundary which can atomically publish an immutable page scene and independently revisioned
  first-party chrome through WebRender, then submit the exact default EGL framebuffer without a
  CPU pixel-copy path.
- Owner: Agent 4 — graphics/GPU. Root integration, `Cargo.lock`, product status, and independent
  acceptance remain owned by the main orchestrator.
- Baseline: repository `HEAD` `4f2c83ade33ee26eb6d0f6a8afabd9a4849c1fc6`; read-only Firefox ESR153
  reference `c19b7e89270787889495688244ec6ee8e79288a1`.
- Status: GO for this bounded same-process browser-compositor prerequisite after the unit,
  static-analysis, documentation, release-build, and live Wayland/X11 evidence below. It remains
  NO-GO for Firefox browser-UI parity, multiprocess composition, desktop-compositor
  acknowledgement, AppImage closure, or release acceptance.
- Writable scope used: `gfx/wild_buzzard_linux_presenter/**` and this handoff. No renderer, widget,
  browser-engine, JavaScript, root manifest, toolchain, CI, or Firefox-reference source was changed
  by this component owner. The orchestrator refreshed the root lockfile for the presenter's one
  direct first-party text dependency.

The implementation is a production-oriented capability boundary, not a screenshot mockup. It
retains exact identities and monotone revisions, publishes real WebRender pipelines, uses shaped
Linux font resources, renders into framebuffer zero, verifies backend-published pipeline epochs
and removals, and swaps the existing hardware-only EGL surface. The intentionally small visible
chrome slice comprises tabs,
tab close/loading state, an address field with selection/caret/focus, a status overlay, and the
clipped page viewport. That slice is an architectural integration gate, not a claim that the
browser product UI is complete.

## Public contract

`LinuxPresentedWindow::into_browser_compositor` consumes the exact W4/W5 native owner and returns
the existing thread-affine `WebRenderPresentedWindow`; it does not construct a second renderer,
context, or window. The principal operation is:

```rust
WebRenderPresentedWindow::submit_browser_frame(
    BrowserPageUpdate,
    Option<BrowserChromeScene>,
    BrowserFrameRequest,
) -> Result<BrowserFrameReceipt, WebRenderWindowError>
```

The capability-neutral value model includes:

- opaque nonzero `BrowserNavigationIdentity` and `BrowserTabIdentity` values;
- nonzero, never-reused `BrowserPageSceneRevision` and `BrowserChromeRevision` values;
- `BrowserPageSnapshot::{Blank, Scene}` so absence is represented directly rather than by a fake
  document or revision;
- `BrowserPageUpdate::{Retain, Install, ClearToBlank}` for exact page-pipeline transitions;
- `BrowserChromeState`/`BrowserChromeScene` for tabs, active/loading state, shaped titles, shaped
  address and status strings, address selection, focus, and scale-derived physical geometry;
- `BrowserFrameRequest`, which binds the exact surface snapshot, resulting page identity, chrome
  revision, strictly increasing root epoch, and strictly increasing native swap sequence; and
- `BrowserHitTarget`/`BrowserHitTestResult`, which bind a typed target and content-local page point
  to the exact last successful `BrowserFrameReceipt`.

The browser compositor reserves two renderer-namespace-scoped WebRender pipeline IDs for root and
chrome and rejects any page collision. Page, chrome, root, and swap identities are validated before
transaction acceptance. Page and chrome may replace or retain independently during an unchanged
surface revision. After resize, scale, suspend, or resume, new exact chrome is mandatory; a live
page must also be replaced for the new content viewport. A page may instead be cleared to `Blank`
while publishing new chrome.

`request_inner_size(PhysicalSize)` is a value-only window-system request. The presenter does not
mutate EGL or WebRender inside that request. In the final W6 integration, the widget shell routes a
synchronously applied winit size, or the later native `Resized` event when the request is
asynchronous, through the same checked resize transition. `set_ime_allowed(bool)` and
`set_ime_cursor_area(LogicalRect)` similarly expose only first-party value types, not winit or raw
window authority.

## Composition, text, and hit authority

The successful composition order is:

```text
validate exact surface/page/chrome/root/swap identities and fixed limits
  -> validate and compose any replacement page scene/text
  -> stage replacement page and chrome shaped text in one dense resource partition
  -> build replacement page/chrome display lists as required
  -> build the root display list with a translated/clipped page iframe
  -> append the full-window chrome iframe last (topmost)
  -> submit one resource/display-list/root/frame transaction
  -> exact FrameBuilt and frame-ready evidence
  -> make the exact EGL surface/context current
  -> WebRender update and consume the backend's full current `PipelineInfo` epoch map
  -> exact root/chrome/live-page epoch and retired `(PipelineId, DocumentId)` removal checks
  -> WebRender draw directly to default framebuffer zero
  -> exact FrameRendered and checked GL state
  -> eglSwapBuffers and native sequence/accounting commit
  -> publish one exact receipt and typed hit map
```

Page and chrome text use the normal `wild_buzzard_text` Linux shaper and the existing
`wild_buzzard_text_webrender::TextFontRegistry`; there is no bitmap 5x7 fallback. A private
`DocumentVersion` partitions each transaction's newly staged page and chrome entries densely.
Even an equal shared `Arc<ShapedText>` in both partitions retains two exact ordered slots, and page
resolution is checked against the original immutable page document identity. The private resource
document identity is not exposed in receipts.

The fixed chrome limits are 64 tabs, 66 shaped strings, 1 MiB aggregate source UTF-8, 100,000
glyphs, 4,096 shaped runs, 16 MiB serialized chrome display-list data, and 1 MiB serialized root
display-list data. The combined replacement-page, chrome, and root display lists also remain under
the existing caller-nonenlargeable 128 MiB window limit. Existing page item/text and native surface
limits continue to apply. At most one visible physical pixel per admitted tab is required, so a
surface too narrow for the exact tab count rejects instead of silently dropping a tab.

The chrome display list paints selected/loading tabs, a bounded close button and visible close
mark, address selection/caret, status, and focus. Caret placement uses shaped cluster boundaries,
including RTL ordering, rather than proportional UTF-8 byte arithmetic. The root clips and
translates page content below the top chrome and places chrome above page/status overlap.

`hit_test_browser` deliberately resolves against a small typed Rust hit map retained with the last
successful receipt. Its topmost order is status, address field, tab close, tab body, then clipped
page; page coordinates are translated to the content viewport. The display lists also contain
WebRender hit-test primitives, but this gate does not yet use the asynchronous WebRender hit-test
API as browser input authority. A foreign surface, accepted in-flight request, surface transition,
legacy-scene acceptance, or terminal compositor state invalidates hit admission rather than
guessing against stale geometry.

## Failure, resize, and teardown semantics

Rejection before WebRender transaction acceptance preserves the prior successful receipt and hit
map. An accepted browser transaction is a point of no retry: any later notification, renderer,
epoch, GL, deadline, native, swap, or contained-panic failure invalidates browser hit authority and
latches the combined owner terminal. A contradictory native admission after the outer contract
accepted an apparently identical request is likewise terminal internal drift. The consumed page
update is never returned; a shell needing another installation attempt must request an exact
engine rerender.

The legacy `submit_scene` API remains for W5 regression compatibility. Once its resource
transaction is accepted, any prior browser receipt/hit map becomes terminally invalid because the
document root has changed. A legacy rejection before acceptance leaves the browser composition
authoritative.

The live Wayland smoke exposed an important resize invariant: after winit observed the requested
configure, resizing the existing `wl_egl_window` wrapper could still leave `eglQuerySurface`
reporting its previous extent. `LinuxPresentedWindow::resize` now makes the surface non-current,
releases only the Rust EGL window-surface wrapper, recreates that wrapper for the requested nonzero
extent around the persistent context/window owner, makes it current, and verifies the queried EGL
extent before committing the Rust resize contract. The same path passed Wayland and X11/Xwayland.
This proves checked wrapper recreation and exact queried extent; glutin still exposes no native EGL
destructor acknowledgement, so none is fabricated.

The later integrated W6 shell smoke exposed a distinct verification defect while clearing a live
page to `Blank`. `Renderer::current_epoch` reads an accumulating renderer cache: `Renderer::update`
inserts published epochs and removal records but does not delete a removed pipeline's old epoch.
Cache absence therefore cannot prove pipeline retirement. Every requested backend frame instead
publishes `PipelineInfo` containing the full current scene epoch map and the removals drained for
that frame. Browser-frame completion now calls `Renderer::flush_pipeline_info` after the exact
renderer update, requires the expected root, chrome, and optional live-page epochs from that
publication, and requires either no removal or the one exact retired `(PipelineId, DocumentId)`
tuple. The same derived retired page pipeline drives both `Transaction::remove_pipeline` and
verification. A stale cached retired epoch is harmless only when accompanied by the exact removal
event; a missing, wrong, duplicate, or unexpected removal still fails terminally at `VerifyEpoch`
with renderer classification. The repair adds no retry path and does not weaken post-acceptance
failure semantics.

Shutdown retains the W5 ordering: release the renderer-scoped font registry, delete the document,
confirm backend shutdown, deinitialize the renderer with the exact context current, and then run
the presenter's checked non-current/wrapper release. The live smoke requires nonzero released font
template, instance, and byte counts.

The font lifecycle has an explicit limitation: `TextFontRegistry` is global to this renderer,
monotone, and bounded. Removing or replacing a page pipeline does **not** retire that page's older
font template/instance keys. Pipeline removal retires backend scene use only; all accumulated font
resources are ordered for release at renderer shutdown. Per-document/page font-key reference
tracking and retirement remain later work.

## Firefox reference implementation, tests, and history inspected

The ignored reference checkout remained detached and read-only. Focused inspection covered:

- `gfx/layers/wr/WebRenderLayerManager.cpp`, `WebRenderScrollData.cpp/.h`,
  `WebRenderBridgeChild.cpp`, `HitTestInfoManager.cpp/.h`, and relevant
  `WebRenderCommandBuilder.cpp` sections;
- `gfx/webrender_bindings/WebRenderAPI.cpp` and `RendererOGL.h`;
- `gfx/wr/webrender/src/hit_test.rs` and `scene_builder_thread.rs`;
- `gfx/wr/wrench/src/rawtest.rs` hit-test ordering tests; and
- iframe clip/crash references including
  `gfx/wr/wrench/reftests/clip/iframe-nested-in-stacking-context.yaml` and
  `gfx/wr/wrench/reftests/crash/iframe-dup.yaml`.

Focused history used `git log --follow` for WebRender and Gecko hit-testing and `git log -S` for
iframe/pipeline transitions. In particular, the history around remote-frame hit-test boundaries,
split display-item hit regions, irregular-area hit testing, and scene/frame snapping reinforces
three invariants adopted here: keep pipeline/epoch identity explicit, bind hit authority to a
rendered composition, and preserve clip/z-order semantics across iframe boundaries. Wild Buzzard
adopts those observable invariants, not Gecko's C++ object graph; Firefox is neither a build nor a
runtime input.

## Dependency and unsafe audit

The only manifest delta is a direct local `wild_buzzard_text` dependency needed to accept normal
Rust-shaped chrome strings. `cargo tree --locked -e features` confirms the existing local
WebRender/renderer/text bridge and admitted winit Wayland/X11 graph; no new third-party version,
native library, endpoint, ambient capability, software renderer, telemetry, or first-party C/C++
was added.

`browser_compositor.rs`, `webrender_window.rs`, and the live example forbid first-party unsafe and
contain no unsafe blocks. The EGL resize change adds no unsafe operation; existing audited
glutin/raw-EGL/gleam calls remain localized in `egl_window.rs`. Focused searches found no pixel
readback or pixel-copy operation in the browser composition or smoke path. The one URL-shaped test
string is local test data, not a network endpoint.

## Verification evidence

All build output was kept under the required external tree:
`/home/user/Documents/wildbuzzardbuilds/w6-a4r`. The final target tree occupied 3.3 GiB. The
toolchain was Cargo 1.96.0 (`30a34c682`) and rustc 1.96.0 (`ac68faa20`, LLVM 22.1.2), host/target
`x86_64-unknown-linux-gnu`.

```sh
export CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w6-a4r

cargo fmt --manifest-path gfx/wild_buzzard_linux_presenter/Cargo.toml -- --check

cargo test --manifest-path gfx/wild_buzzard_linux_presenter/Cargo.toml \
  --locked --all-targets --all-features

cargo clippy --manifest-path gfx/wild_buzzard_linux_presenter/Cargo.toml \
  --locked --all-targets --all-features -- -D warnings

RUSTDOCFLAGS='-D warnings' cargo test \
  --manifest-path gfx/wild_buzzard_linux_presenter/Cargo.toml \
  --locked --doc --all-features

RUSTDOCFLAGS='-D warnings' cargo doc \
  --manifest-path gfx/wild_buzzard_linux_presenter/Cargo.toml \
  --locked --no-deps --all-features

cargo build --manifest-path gfx/wild_buzzard_linux_presenter/Cargo.toml \
  --locked --release --all-targets --all-features

cargo build --manifest-path gfx/wild_buzzard_linux_presenter/Cargo.toml \
  --locked --release --features real-webrender-window-smoke \
  --example webrender-window-smoke

cargo metadata --manifest-path gfx/wild_buzzard_linux_presenter/Cargo.toml \
  --locked --no-deps --format-version 1 >/dev/null

git diff --check -- gfx/wild_buzzard_linux_presenter \
  docs/handoffs/W6-A4R-browser-compositor.md
```

Results: formatting, locked metadata, strict Clippy, warning-denied rustdoc, release builds, and
diff checks passed. The final post-repair all-target/all-feature test ran 71 presenter tests with
zero failures; the example harness compiled and contained zero tests. Both compile-fail doctests
passed. The release all-target build completed with one pre-existing imported WebRender `frame_id`
dead-code warning;
strict `clippy -D warnings` for the owned crate was clean.

The new regressions cover dense equal-`Arc` page/chrome text slots and per-transaction resource
restaging; root iframe translation/clip/z-order and explicit Blank; page clear/removal accounting;
foreign/repeated surface/epoch/sequence rejection with previous receipt preservation; accepted and
legacy invalidation; exact resize replacement matrices; independent chrome revisioning; page
pipeline collision; 64 compressed but visible/hittable/painted tabs; tiny close-paint bounds;
multibyte variable-width shaped caret placement; one-pixel end-caret containment; zero-content
geometry; every injected post-acceptance error becoming externally terminal; admission of a stale
retired epoch only with the exact backend removal tuple; and terminal rejection of missing, wrong,
duplicate, or unexpected removal evidence.

### Standalone A4 release-example evidence

The live host was Ubuntu 26.04 LTS, Linux 7.0.0-28-generic, GNOME Wayland at
`WAYLAND_DISPLAY=wayland-0`, with Xwayland at `DISPLAY=:0`. The same original standalone A4
release-example binary ran in two separate child processes behind its internal 25-second parent
kill deadline:

```sh
WILDBUZZARD_REAL_WEBRENDER_WINDOW_TEST=1 \
WILDBUZZARD_DISPLAY_BACKEND=wayland \
/home/user/Documents/wildbuzzardbuilds/w6-a4r/x86_64-unknown-linux-gnu/release/examples/\
webrender-window-smoke

WILDBUZZARD_REAL_WEBRENDER_WINDOW_TEST=1 \
WILDBUZZARD_DISPLAY_BACKEND=x11 \
/home/user/Documents/wildbuzzardbuilds/w6-a4r/x86_64-unknown-linux-gnu/release/examples/\
webrender-window-smoke
```

Each run constructed a Rust DOM/layout page with nonempty paint, shaped real Linux tab/address/status
text, forced and observed a native resize, published page plus chrome in one transaction, checked
address and content-local page hits, completed one direct WebRender draw and EGL swap, and required
ordered resource/backend/renderer/presenter shutdown. Exact successful output was:

```text
W6-A4R wayland page+chrome publish=1 page_epoch=Some(1) chrome_epoch=1 resize=observed EGL_swap=accepted compositor_ack=false
W6-A4R x11 page+chrome publish=1 page_epoch=Some(1) chrome_epoch=1 resize=observed EGL_swap=accepted compositor_ack=false
```

The standalone example binary was 12,000,600 bytes with SHA-256
`2a35d20af83304fd3051accc16580709d7765fd9491c47ac902afbbc2652840b`. This evidence predates the
`PipelineInfo` repair and exercises one page/chrome publication plus resize; it is not the
integrated multi-transition W6 shell evidence below and its binary hash is not a hash of the final
post-repair source state.

### Integrated W6 shell evidence after the repair

The integrated release shell initially reproduced the defect on both live backends when
`ClearToBlank` retired the page pipeline: the page's old epoch remained visible through
`Renderer::current_epoch` even though the backend had removed it. After the A4 repair above, the old
`VerifyEpoch` failure disappeared. A separate A6 Wayland resize-progression defect was then repaired
in its owning widget/browser paths; no further A4 source change was required.

The latest frozen rerun used this one exact release binary for both backends:

```text
/home/user/Documents/wildbuzzardbuilds/w6-a6g/shell/x86_64-unknown-linux-gnu/release/wild-buzzard
22,384,592 bytes
SHA-256 0e83e75a46274b438671a118aa563ac21576206a2dd02f4aaa424780dfae5986
```

Each backend ran as:

```sh
WILDBUZZARD_REAL_DISPLAY_TEST=1 \
timeout --signal=TERM --kill-after=5s 30s \
  /home/user/Documents/wildbuzzardbuilds/w6-a6g/shell/x86_64-unknown-linux-gnu/release/wild-buzzard \
  --smoke --backend wayland  # or x11
```

Both processes exited 0 after completing the exact page install, clear/removal, restore,
resize-away/resize-back, tab-close, and terminal-hold sequence. Wayland emitted its success marker
at composition 12 and shut down `Requested` after 58 compositions; its final receipt bound surface
1600x1000 at scale 1.25/revision 3, page-scene revision 4/page epoch 10, and chrome epoch/backend
publish ID 58. X11 emitted its success marker at composition 13 and shut down `Requested` after 84
compositions; its final receipt bound surface 2560x1600 at scale 2/revision 3, page-scene revision
4/page epoch 11, and chrome epoch/backend publish ID 84. The bounded terminal hold can admit
additional valid redraws after the success marker, which accounts for the larger final composition
counts.

The exact retained logs were:

```text
ab9111b56311d4362ba9f555d2dc3b4f80243db1277eb0cfd042a49c6d814f6e  /home/user/Documents/wildbuzzardbuilds/w6-a6g/wild-buzzard-wayland.log
4f878c074652caec66437fb1ef1d6dcfb68d647d9d0000e05bd56dedbad1e6df  /home/user/Documents/wildbuzzardbuilds/w6-a6g/wild-buzzard-x11.log
```

This is integrated evidence for the repaired page-pipeline transition and shell progression. It
does not replace the standalone A4 example's narrower evidence. Screenshot evidence was rejected,
and neither run proves desktop-compositor acknowledgement.

Current post-repair source/manifest hashes were recomputed from the frozen A4-owned files:

```text
af8861f1757024c494de8ba8442b71e0c17e48514038badbb47ffe19b9dc401d  gfx/wild_buzzard_linux_presenter/Cargo.toml
1c51cfb92120e6414fe18e352fb287dfcc8c6e176da94f8b4cb1b656993880a1  gfx/wild_buzzard_linux_presenter/examples/webrender_window_smoke.rs
061c44f5d67699811f61680ed7431d4468d2d8fa3e079482ad828e54ec36cd4c  gfx/wild_buzzard_linux_presenter/src/browser_compositor.rs
533e1fa47b31193da578f641c34d6b9e4c2273e7ada3a0ff4c2620eec94415a9  gfx/wild_buzzard_linux_presenter/src/egl_window.rs
497212d15c128178bb2e871877329b7a1acc3dbaf4071730499e9626b95b3c9a  gfx/wild_buzzard_linux_presenter/src/lib.rs
b46133e3265640c19277af150a61d92d5f2cce7a4b5b7f079976b743e4f0d78f  gfx/wild_buzzard_linux_presenter/src/webrender_window.rs
b83b2a8a154948f93fc6aa8c92c35950079b562b74957dde2b1a0841703d30de  gfx/wild_buzzard_linux_presenter/src/window_contract.rs
```

Neither standalone live run nor either integrated shell run read pixels back, uploaded a composed
screenshot, proved that the desktop compositor displayed the swapped buffer, or observed a native
EGL destructor result.

## Explicit non-claims and next work

This gate is not Firefox UI parity. It does not yet implement or connect back/forward/reload/home
controls, menus, prompts, downloads, bookmarks, history, passwords, permissions, settings,
developer tools, accessibility semantics, drag-and-drop, browser-keyboard parity, multiple windows,
or complete tab/window/session behavior. The chrome types are deliberately extensible value
contracts for later first-party surfaces, not a frozen visual design.

It also does not provide WebRender-authoritative dynamic DOM hit testing, APZ/async scrolling or
zoom, selection editing, animation, damage/buffer age, occlusion, frame callbacks/vsync pacing,
GPU-process IPC/isolation, cross-process resource transfer, device-loss reconstruction, driver-hang
recovery, Canvas/WebGL/WebGPU/media composition, compositor/display acknowledgement, or AppImage
dependency closure. Page layout must currently be rendered for the exact physical content viewport
after a surface change. Synchronous WebRender, driver, EGL, and teardown calls remain
non-preemptible by the existing checkpoint deadline.

The fixed display-list, text, and surface limits do not bound total WebRender/GPU memory, process
RSS, shader/cache storage, driver allocations, worker-thread stacks/CPU, or the cumulative global
font registry below its separate registry limits. Later graphics work must add per-document font
retirement, authoritative rendered-frame hit testing, frame pacing/damage, GPU-process ownership
and crash isolation, device-loss recovery, broader browser chrome surfaces, accessibility, and
packaged live Wayland/X11 closure before a wider compatibility claim.
