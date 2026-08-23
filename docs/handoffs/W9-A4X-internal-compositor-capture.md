# W9-A4X internal compositor capture handoff

Date: 2026-08-23
Base commit: `076242d20aaf554bc313a2ef4cf52d91d878b3bd`
Firefox reference: detached `c19b7e89270787889495688244ec6ee8e79288a1`
Target: Linux `x86_64-unknown-linux-gnu`
Workspace disposition: direct shared-workspace edits; nothing staged or committed

## Outcome

This slice implements the first explicit, one-shot, receipt-bound raw capture of the real
Wild Buzzard Linux browser compositor. A caller can request one browser frame and receive one
owned, tightly packed, top-left BGRA8 framebuffer together with its inseparable successful
`BrowserFrameReceipt`. The capture includes first-party browser chrome and exposes the exact
physical `BrowserChromeGeometry::content()` crop without a second GL readback or a duplicate
full-frame allocation.

The existing `submit_browser_frame` API remains the normal fast path. It performs no capture
allocation, `record_frame`, or `map_recorded_frame` call. The new
`submit_browser_frame_with_capture` path calls pinned public WebRender APIs only after the exact
frame has rendered and its `Checkpoint::FrameRendered` waiter has succeeded, and before the
matching EGL `swap_buffers`. Mapped bytes remain private until that swap succeeds and the exact
browser receipt is committed.

This is a raw authoritative compositor boundary for later browser-internal automation. It does
not add PNG, base64, WebDriver, BiDi, desktop screenshot, front-buffer, headless, or external
process capture to the product.

## Contract and ordering

The successful order is:

1. Validate the exact browser request, retained state, native surface, and fixed deadline.
2. Preflight all dimensions, crop coordinates, strides, areas, and byte counts, then fallibly
   reserve and initialize the sole full-frame allocation before transaction submission.
3. Submit the exact browser transaction; await `FrameBuilt`, render it, verify GL, and await its
   `FrameRendered` checkpoint.
4. Call `Renderer::record_frame(ImageFormat::BGRA8)`, validate the returned device extent, verify
   GL, call `Renderer::map_recorded_frame`, and verify GL again.
5. Validate the shared deadline, submit the exact EGL swap, record native swap acceptance, and
   validate the deadline again.
6. Revalidate capture/request/page/chrome/epoch/publish identity, commit the exact
   `BrowserFrameReceipt`, and privately construct `BrowserFrameCapture` from that receipt and the
   mapped allocation.

The capture cannot be safely constructed, relabelled, or paired with another receipt by a
caller. Its receipt binds:

- the generational surface ID and revision, backend/window identity, exact physical extent,
  scale, presentation capabilities, and pixel format;
- Blank versus exact page scene, including navigation identity, page revision, DOM document
  version, and page pipeline;
- the exact immutable chrome revision; that revision binds the chrome scene's tab identities and
  state without duplicating the full tab inventory in the receipt;
- root, page, and chrome WebRender epochs, the nonzero backend publish ID, and the strictly
  monotonic native frame/swap sequence.

Preflight rejects framebuffer axes below 2 pixels, axes above 16,384 pixels, an empty or
out-of-bounds content viewport, more than 67,108,864 pixels, or more than 268,435,456 bytes.
Every multiply/add/coordinate conversion is checked. Allocation uses `try_reserve_exact` and is
typed as `PrepareCapture/CaptureAllocationFailed`. There is no `unsafe` in the first-party
capture implementation.

The returned byte contract is tightly packed B, G, R, A bytes, top row first. Alpha is the raw
default-framebuffer alpha returned by the pinned WebRender API; it is neither normalized nor
unpremultiplied, and no additional color conversion is claimed. `BrowserBgra8Crop` provides
checked zero-copy rows plus an exact-length, exact-stride copy helper whose inter-row padding is
left unchanged.

One shared browser-frame deadline covers allocation, composition, record, map, and swap. It is
checked before and after imported record/map work, after each GL verification, and before and
after swap. A synchronous GL/driver call cannot be preempted while it is executing; a late return
is detected immediately afterward and publishes no capture or receipt. If EGL accepted a swap
before that post-call timeout, internal native accounting honestly retains that fact while the
owner transitions to `Lost` and no caller receives success evidence.

`record_frame == None`, malformed/foreign device size, `map_recorded_frame == false`, GL/device
faults, stale or foreign identity, and deadline failures preserve their exact typed stage and
fail closed. Imported record/map panics are caught by the existing browser-frame unwind boundary
at the active `RecordCapture` or `MapCapture` stage. Any failure after transaction acceptance is
terminal, invalidates hit/receipt admission, discards the mapped allocation, and performs no swap
unless the failure is the post-swap deadline check. A swap rejection publishes neither receipt
nor capture.

Normal ordered shutdown calls pinned `Renderer::deinit`, whose recorder teardown deletes pending
and available PBOs and scaling textures. Both live runs required confirmed backend shutdown and
confirmed renderer deinitialization before reporting success. If renderer/backend ordering is
unproven, the pre-existing teardown policy retains ownership or aborts fail-closed rather than
fabricating release evidence.

## Conservative acceleration correction

`WebRenderSurfaceSnapshot::capabilities` now documents the actual startup rule: one unpublished,
typed `Unverified -> Software` correction is permitted; capability relabelling after publication
is forbidden. Strict hardware admission for `Accelerated + reset protection unavailable` is
covered by a barrier test whose publication, document, frame, and swap callbacks all remain at
zero when rejected. The admitted capability token is retained by the window owner and checked
against every live surface snapshot.

## Changed paths and source hashes

All hashes are SHA-256 of the final unstaged files:

| Path | SHA-256 |
| --- | --- |
| `gfx/wild_buzzard_linux_presenter/src/browser_compositor.rs` | `fdb35b4a8173aa4883c4b62e9a377b338ac576c5f254a13be33e245e3ba0b9ec` |
| `gfx/wild_buzzard_linux_presenter/src/window_contract.rs` | `c7a766135f3aa027567ae1b95c07fa3e947f989dc1e0639f230438e7e5eed710` |
| `gfx/wild_buzzard_linux_presenter/src/webrender_window.rs` | `310f2b48688dbf1d5722d51f2e8f4beb494c3783d6f09affb721894c0d7db6d2` |
| `gfx/wild_buzzard_linux_presenter/src/lib.rs` | `44d155276620346726102678f7592779630cc8a44a55adb8a8669945b6b99e8a` |
| `gfx/wild_buzzard_linux_presenter/examples/webrender_window_smoke.rs` | `4dc5b7897787db27f62d71f16b0917ec0daf996c55612638f3e66858967b9da4` |
| `widget/rust/wild_buzzard_linux/src/shell.rs` | `7cff3dd1b3bdbd7d4448da1df9da98a7fd3ae03fd7192a7252e7753fdfa0089b` |
| `widget/rust/wild_buzzard_linux/src/lib.rs` | `879ed58bdf791e37e63e30aa7957cd1d05b925d0378ec4b3617961409bd7ee03` |
| `docs/handoffs/W9-A4X-internal-compositor-capture.md` | Reported externally because a file cannot contain its own stable digest. |

No manifest, lockfile, imported WebRender source, Firefox reference file, browser shell,
WebDriver/BiDi path, or `AGENTS.md` was edited for W9-A4X.

## Firefox and imported WebRender inspection

The Firefox checkout remained clean and read-only. The implementation and history reviewed were:

- `firefox/gfx/webrender_bindings/RendererOGL.cpp`, especially `MaybeRecordFrame` and its
  before-`EndFrame`/before-swap invariant; SHA-256
  `5d8a19d015b24db24a623de5a070aea437e9b36605b9f82ba259f6f4df31a04f`.
- `firefox/gfx/webrender_bindings/src/bindings.rs`, public record/map/release bindings; SHA-256
  `3d1acbef56487b28cac7dbb0f434de6619ab8f7e2353888191b1be0d10ce412e`.
- `firefox/gfx/wr/webrender/src/screen_capture.rs`, including top-left row correction, PBO
  mapping/deletion, and recorder deinit; SHA-256
  `49498b53873e38e9cc66d5b7a9364cb266838ee00fd362f5f27c9dd7085769d8`.
- `firefox/gfx/layers/CompositionRecorder.{h,cpp}`, compositor adapters, and relevant history,
  including the first-frame and screen-pixel request fixes.

The corresponding imported `gfx/wr/webrender/src/screen_capture.rs` has the identical SHA-256
`49498b53873e38e9cc66d5b7a9364cb266838ee00fd362f5f27c9dd7085769d8`.
Firefox is neither a build input nor a runtime dependency.

## Reproducible build environment

- Data-drive-only Podman wrapper:
  `/run/media/user/Data/Repositories/wildbuzzardbuilds/podman/podman-wildbuzzard`
- Image: `localhost/wildbuzzard-rust-tests:1.90-trixie-tools`
- Image ID: `2bd2b60e38453b22d4d13f8d303b4dbc26de6e8c42b6322dbcee31ba2119e7c6`
- Image digest: `sha256:5cb79706a1853550f400e37c712804df498b2b8621fa6faf340b9f68b0f60ea1`
- `rustc 1.90.0 (1159e78c4 2025-09-14)`, LLVM 20.1.8
- `cargo 1.90.0 (840b83a10 2025-07-30)`
- Source mount `/workspace`, artifact mount `/build`, `CARGO_HOME=/build/cargo-home`,
  `CARGO_TARGET_DIR=/build/target`, and `--network none` for every final Cargo command.

The release smoke binary is:

`/run/media/user/Data/Repositories/wildbuzzardbuilds/w9-a4x-compositor-capture/binaries/webrender-window-smoke-final`

SHA-256: `dbe1690f7ab419eceb7b800fb813f308d621aecc4f0fd5bab3ac7940d0244e99`.

## Commands and results

Rustfmt was restricted to the seven authorized Rust files:

```text
rustfmt --edition 2024 --config skip_children=true \
  gfx/wild_buzzard_linux_presenter/src/browser_compositor.rs \
  gfx/wild_buzzard_linux_presenter/src/window_contract.rs \
  gfx/wild_buzzard_linux_presenter/src/webrender_window.rs \
  gfx/wild_buzzard_linux_presenter/src/lib.rs \
  gfx/wild_buzzard_linux_presenter/examples/webrender_window_smoke.rs \
  widget/rust/wild_buzzard_linux/src/shell.rs \
  widget/rust/wild_buzzard_linux/src/lib.rs
```

Final locked/offline gates inside the fixed container:

```text
cargo test --locked --offline --manifest-path gfx/wild_buzzard_linux_presenter/Cargo.toml --all-features --all-targets
cargo test --locked --offline --manifest-path widget/rust/wild_buzzard_linux/Cargo.toml --all-features --all-targets
cargo clippy --locked --offline --manifest-path gfx/wild_buzzard_linux_presenter/Cargo.toml --all-features --all-targets --no-deps -- -D warnings
cargo clippy --locked --offline --manifest-path widget/rust/wild_buzzard_linux/Cargo.toml --all-features --all-targets --no-deps -- -D warnings
cargo clippy --locked --offline --manifest-path gfx/wild_buzzard_linux_presenter/Cargo.toml --all-features --all-targets --no-deps -- -W clippy::pedantic
cargo clippy --locked --offline --manifest-path widget/rust/wild_buzzard_linux/Cargo.toml --all-features --all-targets --no-deps -- -W clippy::pedantic
RUSTDOCFLAGS=-Dwarnings cargo doc --locked --offline --manifest-path gfx/wild_buzzard_linux_presenter/Cargo.toml --all-features --no-deps
RUSTDOCFLAGS=-Dwarnings cargo doc --locked --offline --manifest-path widget/rust/wild_buzzard_linux/Cargo.toml --all-features --no-deps
cargo build --locked --offline --release --manifest-path gfx/wild_buzzard_linux_presenter/Cargo.toml --all-features --all-targets
cargo build --locked --offline --release --manifest-path widget/rust/wild_buzzard_linux/Cargo.toml --all-features --all-targets
```

Results:

- Presenter: 139/139 library tests passed; 1/1 example test passed.
- Widget: 40/40 library tests passed; the separately opt-in real-display integration test was
  correctly ignored by the non-live unit command and was exercised manually below.
- Focused capture matrix: 9/9 passed; later exact identity and swap-discard regressions each
  passed independently and are included in the 139-test final matrix.
- Warnings-denied Clippy: passed for both crates.
- Pedantic diagnostic: presenter clean. Widget reports 21 inherited library diagnostics and
  three inherited real-display-smoke diagnostics; the new capture control API has no pedantic
  diagnostic.
- Rustdoc with warnings denied: passed for both crates.
- Release all-target builds: passed. Cargo reports one pre-existing imported WebRender warning for
  unused `RenderTaskGraph::frame_id`; no first-party W9-A4X warning.
- `git diff --check`: passed. Narrow source scans found no `unsafe`, site-specific behavior,
  Firefox build/runtime lookup, desktop screenshot route, front-buffer read, headless pixel path,
  PNG, base64, WebDriver, or BiDi implementation in the W9-A4X product files.

The final logs are under
`/run/media/user/Data/Repositories/wildbuzzardbuilds/w9-a4x-compositor-capture/logs/`.

## Live evidence

### Physical host, Wayland

Exact command environment:

```text
DISPLAY=
WILDBUZZARD_DISPLAY_BACKEND=wayland
WILDBUZZARD_REAL_WEBRENDER_WINDOW_TEST=1
WILDBUZZARD_INTERNAL_CAPTURE_BGRA_PATH=/run/media/user/Data/Repositories/wildbuzzardbuilds/w9-a4x-compositor-capture/live/physical-wayland-final/internal-frame.bgra
timeout 45s /run/media/user/Data/Repositories/wildbuzzardbuilds/w9-a4x-compositor-capture/binaries/webrender-window-smoke-final
```

Result: exit 0; visible real Wayland window; conservative `Unverified` acceleration with
`LoseContextOnReset`; first browser frame had no capture, second frame had exactly one capture;
publish ID 2; page epoch 1; chrome epoch 2; EGL swap accepted; ordered backend and renderer
shutdown confirmed. This is a physical-host graphics-stack run, not an affirmative accelerated
classification.

- Extent: 720x540, stride 2,880, 1,555,200 bytes.
- Content crop: `(x=0, y=107, width=720, height=433)`.
- Raw BGRA SHA-256: `d3f9afc6992c6421e51ca139c5606f38c1f846f0aa3f5ad3da0ade953a11c32a`.
- QA PNG SHA-256: `435ef121ef953c24811e32e53ca7e63bdad75036520acabdc0b8a27efe6f6355`.
- QA content PNG SHA-256: `38bf882c6d357713b69458bebd85169b9af4cf049364d1999e5ad42996578915`.
- Log: `logs/physical-wayland-live-final-binary.log`, SHA-256
  `e9a66f607f260c7c9a2e6572144deaf502957ef59a54f7c52ba83948308f1d59`.

The PNG files are offline lossless QA conversions of the raw internal bytes, not a product PNG
or screenshot route. Visual inspection confirmed top-left orientation, complete Wild Buzzard
chrome, content, popup, and status rendering.

### Debian 13 ordinary VM, Wayland software path

Domain: `gnozzard-test-debian13`; session libvirt; guest execution and transfer through QEMU guest
agent. The exact release binary was copied to the guest and hashed before execution.

Result: exit 0; host and guest binary SHA-256 both
`dbe1690f7ab419eceb7b800fb813f308d621aecc4f0fd5bab3ac7940d0244e99`; visible Wayland window;
typed `Software` with `LoseContextOnReset` after the permitted unpublished correction; first
browser frame had no capture, second frame had exactly one capture; publish ID 2; page epoch 1;
chrome epoch 2; EGL swap accepted; ordered shutdown confirmed.

- Extent: 720x540, stride 2,880, 1,555,200 bytes.
- Content crop: `(x=0, y=80, width=720, height=460)`.
- Raw BGRA SHA-256: `a91fe9113c7b8902299ff0452c3f24989a14edd1a73fe300355bf925f5bc93fc`.
- Internal-frame PNG SHA-256: `91f923654d8773ee29c53d12d30b9012a0dafcec14305a5e9984acd5a65baf13`.
- Internal-content PNG SHA-256: `238945e22cb19ada0fbb81fc467df84d719283a53bd2a653610a1d0fb4b71785`.
- Stable visible desktop screenshots 02 through 07 SHA-256:
  `e2ce5c8f20df51e9c5a992e7f8ed3a4d10966b8205fa5e0372c494f5fd5ecb66`.
- VM run log: `logs/debian13-wayland-live-final.json`, SHA-256
  `fc6969bae08aa47e64c68a2422ffbbc821dc80962bd2458698961fd50c9f786c`.
- Pixel comparison log: `logs/debian13-pixel-comparison-final.log`, SHA-256
  `caf8e0067c16e7bc34e68fcaed0708bd17b065a56d542fbf75a85cd2bb75947d`.

Independent and repeated exact pixel checks establish:

- Cropping `desktop-07.png` at client offset `(280,138)` with size 720x540 produces an RGBA image
  byte-for-byte identical to `internal-frame.png`: 0 of 388,800 pixels differ and every
  per-channel mean difference is 0.0.
- `internal-content.png` is byte-for-byte identical to the `(0,80,720,460)` crop from
  `internal-frame.png`: 0 of 331,200 pixels differ and every per-channel mean difference is 0.0.
- Visual inspection independently confirmed that content begins at physical `y=80` and excludes
  browser chrome.

This is explicit evidence that the authoritative internal pixels, exported crop geometry, and
visible VM client pixels agreed exactly for this controlled run. It does not change the API's
honest `desktop_compositor_acknowledged() == false`: a single observed desktop image is not a
general protocol acknowledgement from every desktop compositor.

The first VM attempt exposed an evidence-harness-only path check that incorrectly required the
host Data-drive prefix inside the guest. The smoke harness was corrected to require an absolute
caller path without assuming host mount topology; the final run above passed. Product capture
semantics were unchanged.

## Remaining gaps and non-claims

- No browser-shell, WebDriver, BiDi, PNG, base64, or screenshot transport consumes this raw API
  yet. That is the next automation slice.
- No general desktop scanout acknowledgement exists; only the controlled Debian visible-crop
  equality evidence above is claimed.
- The final live matrix covers host Wayland with conservative unverified acceleration and Debian
  13 Wayland software rendering. X11, affirmative physical GPU classification, accelerated
  virtio/virgl VM capture, Ubuntu VM capture, context-loss recovery during capture, and packaged
  AppImage execution remain open.
- Synchronous record/map/swap driver calls cannot be preempted; post-call deadline checks fail
  closed.
- This slice does not run Firefox differential capture, websites, JavaScript, media, YouTube, or
  full parity tests and makes no claim about them.
- W9-A4X is one enabling vertical slice. It is not completion of the browser, graphics program,
  automation program, AppImage, or Firefox ESR153 parity objective.
