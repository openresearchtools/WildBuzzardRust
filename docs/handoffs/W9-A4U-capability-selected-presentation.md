# W9-A4U capability-selected Linux presentation

## Outcome

W9-A4U replaces the W8-A4T-R architecture-only result with a bounded Linux x86-64
implementation. The original strict profile remains available, while the product shell now uses a
fixed startup-only compatibility ladder:

1. accelerated EGL configuration plus a verified robust context using lose-context-on-reset;
2. accelerated EGL configuration plus a verified non-robust compatible context;
3. software EGL configuration plus a verified robust context using lose-context-on-reset;
4. software EGL configuration plus a verified non-robust compatible context;
5. the last typed unsupported-capability failure when no profile is usable.

There is no mid-session profile switch and no frame replay. A selected profile is immutable for the
native owner lifetime.

## Public value contracts

The presenter exports these authority-free values:

- `LinuxPresentationPolicy::{StrictHardware, AutomaticCompatible}`;
- `LinuxAccelerationClass::{Accelerated, Software}`;
- `LinuxResetProtection::{LoseContextOnReset, Unavailable}`;
- `LinuxPresentationCapabilities`.

`LinuxPresentationPolicy::default()` remains `StrictHardware`, preserving explicit callers which
request the original path. `LinuxShellConfig::wild_buzzard_default` explicitly chooses
`AutomaticCompatible` for normal product startup.

`LinuxWindowPreparation` exposes only the attempted capabilities and the selected X11 visual. It
does not expose EGL, GL, winit, or raw-handle authority. A profile window remains unpublished until
context creation, current-context verification, reset-fact verification, surface setup, and swap
configuration all succeed. During `LinuxWindowEvent::Ready`, the callback-scoped
`LinuxWindowControl::presentation_capabilities` method returns the immutable profile attached to
that event's exact surface.

## Selection facts and forbidden heuristics

The acceleration class comes only from the enumerated EGL configuration's
`GlConfig::hardware_accelerated` fact. In glutin's pinned EGL implementation this reads
`EGL_CONFIG_CAVEAT` and distinguishes `EGL_SLOW_CONFIG`; selection also requires an actual
window-capable desktop-OpenGL, RGBA8/A8, sRGB-capable, zero-sample configuration. X11 additionally
requires the exact configuration visual.

The reset class is verified only after the selected desktop OpenGL 3.2 core context is current:

- robust profiles require `GL_CONTEXT_FLAG_ROBUST_ACCESS_BIT` and
  `GL_RESET_NOTIFICATION_STRATEGY == GL_LOSE_CONTEXT_ON_RESET`;
- compatible profiles require the robust-access flag to be absent.

Negative, unavailable, contradictory, or GL-faulting query results stop startup. The code does not
read `GL_VENDOR`, `GL_RENDERER`, driver names, environment overrides, PCI topology, passthrough,
Venus, virgl, or llvmpipe names. Tests inject those words into diagnostics and prove that the typed
result—not text—controls the ladder.

An initially zero-sized window is checked through a bounded surfaceless current-context probe. The
context is verified, GL is loaded from the retained exact display, and the context is checked
non-current again before the suspended owner can publish startup evidence.

## Fallback and teardown

Only `glutin::error::ErrorKind::NotSupported` from exact context creation becomes
`PresentationErrorKind::UnsupportedCapability`. A missing matching configuration is the equivalent
typed no-profile/no-window result. Generic EGL/native errors, out-of-memory, context loss, panic,
malformed capability evidence, caller window-creation failure, and teardown uncertainty stop the
transaction immediately.

After an unpublished window exists, a typed unsupported context result may advance only after the
attempt's config/display/window wrappers release normally and the zero-frame shutdown report agrees
with the attempted capabilities. A mismatched profile, submitted frame, sequence, wrong error kind,
or retained owner converts the ladder transition into a terminal driver failure. This evidence is
Rust-wrapper release only: as documented by W4-A4P, glutin does not expose native EGL destructor
acknowledgement, and W9-A4U does not fabricate it.

The first accepted profile owns the only context/surface/window path. Existing surface identity,
surface revision, epoch, monotonic sequence, total timeout, resource limits, resize,
suspend/resume, context-loss, first-fault, and ordered teardown behavior remains in force.

## WebRender and evidence binding

`WebRenderOptions::reject_software_rasterizer` is selected from the immutable EGL acceleration
class:

- `Accelerated` keeps rejection enabled;
- `Software` disables rejection;
- reset protection does not alter this decision.

The capability pair is retained in native presentation errors when an attempted/selected profile is
known, native frame receipts, shutdown and retention reports, `WebRenderSurfaceSnapshot`,
`WebRenderWindowFrameReceipt`, renderer startup/shutdown evidence, browser frame requests and their
receipts, widget startup control, and widget shutdown summaries. Resize, scale, suspend, and resume
replace only the checked surface revision/descriptor and preserve the exact capability pair.

`LinuxPresentationCapabilities` authorizes browser-surface presentation only. W9-A4U does not
enable WebGL, WebGPU, or accelerated canvas. In particular, same-process compatible/non-robust
rendering is not release sandbox or process-isolation acceptance.

## Deterministic evidence

The W9 tests prove:

- the exact strict and automatic profile lists and full four-profile order;
- first-profile success performs no later attempt;
- typed unsupported results with exact no-owner or clean zero-frame release evidence may advance;
- generic driver error, GL/EGL out-of-memory, context loss, panic, caller error, mismatched release,
  used release, and retained/unknown teardown never advance;
- diagnostic strings, including vendor/renderer/driver/virgl/Venus/passthrough/llvmpipe words, do
  not affect control flow;
- actual robust flags and reset strategy must agree with the selected reset class;
- accelerated profiles reject a software WebRender renderer while both software profiles permit it;
- native/WebRender receipts, surface revisions, startup failure, and shutdown evidence retain one
  exact capability pair;
- all existing direct-presentation, extent, resize, suspension, sequence, timeout, epoch,
  fault-latching, renderer-worker, and teardown regressions still pass.

## Firefox reference review

The read-only pinned Firefox checkout remained absent from every Cargo input. Review covered:

- `gfx/thebes/gfxPlatform.cpp` around hardware/software WebRender qualification and fallback;
- `gfx/tests/gtest/TestConfigManager.cpp` WebRender configuration tests;
- `widget/gtk/WindowSurfaceProvider.cpp` surface/provider fallback behavior;
- relevant `modules/libpref/init/StaticPrefList.yaml` WebRender hardware, software, driver-rejection,
  fallback, and robustness entries;
- the W4-A4P and W5-A4Q presenter/WebRender contracts and the inherited W8-A4T-R architecture
  report.

Firefox behavior informed the separation between hardware/software and robustness policy. No
Firefox source, test fixture, generated file, path dependency, or runtime lookup is used.

## Build and test evidence

All container graph storage, Cargo state, targets, logs, binaries, and attempted screenshot evidence
are under:

`/run/media/user/Data/Repositories/wildbuzzardbuilds/w9-a4u-capability-selected-presentation`

| Gate | Result | Data-drive log |
| --- | --- | --- |
| Exact Rust 2024 `rustfmt --check` over every writable Rust path | Pass | `logs/rustfmt-check.log` |
| All-feature/all-target presenter and widget tests | Pass: 110 presenter + 34 widget; the one opt-in live integration wrapper remained ignored in this deterministic run | `logs/cargo-test-all-features-all-targets.log` |
| Presenter all-feature/all-target no-deps Clippy, warnings denied plus `all`/`pedantic` | Pass | `logs/clippy-presenter-all-pedantic.log` |
| Widget all-feature/all-target no-deps project Clippy, warnings denied | Pass | `logs/clippy-widget-all.log` |
| Combined presenter/widget no-deps Clippy with extra `all`/`pedantic` | Diagnostic run completed but did not pass: 21 pre-existing widget pedantic findings remain, including three in forbidden `normalize.rs`; no W9 presenter finding remained | `logs/clippy-combined-all-pedantic-diagnostic.log` |
| Warning-denied, all-feature, no-deps rustdoc | Pass | `logs/rustdoc-warnings-denied.log` |
| All-feature/all-target release build | Pass; imported WebRender emitted its pre-existing unused `frame_id` warning | `logs/cargo-build-release-all-features-all-targets.log` |
| Full-worktree `git diff --check` | Pass | `logs/git-diff-check.log` |
| Cargo target dep-info/JSON scan for a Firefox path | Pass: no match | `logs/firefox-build-input-scan.log` |
| Data-drive Podman graph-root check | Pass | `logs/podman-storage.log` |

The aggregate extra-pedantic widget gate cannot be truthfully claimed as passing within this task's
exact path boundary: `widget/rust/wild_buzzard_linux/src/normalize.rs` is explicitly unwritable, and
the remaining findings predate W9-A4U. The project-defined warning-denied widget gate and the stricter
task-owned presenter gate both pass.

Frozen task-owned source SHA-256 values (this handoff is excluded from its own table):

| Path | SHA-256 |
| --- | --- |
| `gfx/wild_buzzard_linux_presenter/src/contract.rs` | `45a702a8042b5b2ebf0d66afec907c57f30662950d69b3ecc480f9195c290f3b` |
| `gfx/wild_buzzard_linux_presenter/src/egl_window.rs` | `877c5bb3b5f843d9133e9d7b3c995c135698c6fe4f778cd2ab3998d2e8b6d355` |
| `gfx/wild_buzzard_linux_presenter/src/webrender_window.rs` | `320ceb4b3b3793248b724dbe9085541c73dda736a8844d7de6b225dc62988d02` |
| `gfx/wild_buzzard_linux_presenter/src/window_contract.rs` | `e4ad94e634d4f7f2a6b268153dfa1f4fdca9e49574d206fed2718561646cf715` |
| `gfx/wild_buzzard_linux_presenter/src/lib.rs` | `9afa9f6d9a0657fb6d759b6a0750d1c0d9f8ae1376d858186756d1df7320665b` |
| `gfx/wild_buzzard_linux_presenter/examples/webrender_window_smoke.rs` | `dac5d9f6c9f3b8f6203cde267cf925ac1087660708ad2f8b5c950d276ebc414c` |
| `widget/rust/wild_buzzard_linux/src/config.rs` | `cb49e1be3f9baa9c0aa3a4c83c6c30eaf9877d3c9ed8390492190bea7f08b1b8` |
| `widget/rust/wild_buzzard_linux/src/shell.rs` | `e1e6e02859bf2848302a43f40f7eeffbf5d8ccdaaff098159d1bff96a81439f5` |
| `widget/rust/wild_buzzard_linux/src/event.rs` | `52af2398ba52b5a1878b19cbcd0589f973275222c41676eb3cca591ffab2b636` |
| `widget/rust/wild_buzzard_linux/src/lib.rs` | `4da8167ee10d82aba9f20a37fe09d21a32c31febabfde1fa7e6636bce5b39ce8` |

Final release binary identities:

| Binary | Size | SHA-256 |
| --- | ---: | --- |
| `cargo-target/x86_64-unknown-linux-gnu/release/wild-buzzard-real-display-smoke` | 9,776,736 bytes | `b4b93ab6578d5f0f3daa1aec12890b7e21bf2761bb3ea44469052f825ec76725` |
| `cargo-target/x86_64-unknown-linux-gnu/release/examples/webrender-window-smoke` | 12,431,056 bytes | `e93b9722d3001c5d13a9f5be2e8e671a6c45278643bf107141344b5d11ed1a61` |

## Live matrix evidence

### Physical host, post-change

- Wayland direct-EGL release smoke: exited successfully after one checked diagnostic draw/swap and
  ordered shutdown.
- Wayland WebRender release smoke: exited successfully after resize, two browser/chrome publishes,
  native swaps, and ordered shutdown. It reported
  `Accelerated/LoseContextOnReset` from the selected EGL/context facts.
- GNOME denied the noninteractive screenshot request with
  `org.freedesktop.DBus.Error.AccessDenied`; no screenshot is claimed and the successful swap is not
  overclaimed as desktop-compositor visibility acknowledgement.
- The host X11/Xwayland smoke did not reach presenter startup because the host could not dynamically
  load `libxkbcommon-x11.so`. This remains a host ABI/AppImage dependency blocker, not a successful
  X11 matrix row.

No graphics-driver forcing or selection environment variable was used. The smoke's backend and
explicit test-admission variables select only the ordinary Wayland/X11 harness path.

### Ordinary VMs, pre-change observations supplied by the user

- Debian 13, standard virtio with 3D disabled, active Wayland at 1280x800: the previous release
  reached `SoftwareRasterizer` and submitted zero frames.
- Ubuntu 26.04, ordinary virgl (`virtio-vga-gl`, `accel3d=yes`, EGL-headless render node
  `/dev/dri/renderD128`, no resource blobs, host-visible resources, passthrough, Venus, or driver
  environment override), active Wayland: the previous release stopped at
  `CreateContext/Driver` because context robustness was unsupported.
- That Ubuntu VNC plus EGL-headless topology returned `screendump: no surface` from
  `virsh screenshot`; it is not a qualifying visible scanout row.

W9-A4U did not alter VM XML. Post-change guest transfer/execution and retained guest screenshots
remain pending the user's announced run and are not claimed here.

## Open security and release blockers

- Compatible/non-robust same-process rendering is not sandbox acceptance.
- WebGL, WebGPU, and accelerated canvas remain unavailable on compatible and software profiles.
- Post-change Debian software and Ubuntu virgl execution/visibility evidence is pending.
- The X11 host ABI dependency above must be resolved or bundled according to the AppImage policy,
  then tested from the packaged artifact.
- Normal wrapper release still cannot prove EGL native destructor acknowledgement because the
  pinned glutin API does not expose it.
- W9-A4U is a presentation compatibility slice, not Firefox ESR153 browser or YouTube parity.

## Changed paths

- `gfx/wild_buzzard_linux_presenter/src/contract.rs`
- `gfx/wild_buzzard_linux_presenter/src/egl_window.rs`
- `gfx/wild_buzzard_linux_presenter/src/webrender_window.rs`
- `gfx/wild_buzzard_linux_presenter/src/window_contract.rs`
- `gfx/wild_buzzard_linux_presenter/src/lib.rs`
- `gfx/wild_buzzard_linux_presenter/examples/webrender_window_smoke.rs`
- `widget/rust/wild_buzzard_linux/src/config.rs`
- `widget/rust/wild_buzzard_linux/src/shell.rs`
- `widget/rust/wild_buzzard_linux/src/event.rs`
- `widget/rust/wild_buzzard_linux/src/lib.rs`
- `docs/handoffs/W9-A4U-capability-selected-presentation.md`

`widget/rust/wild_buzzard_linux/tests/real_display.rs` remained unchanged. No file was staged,
committed, or pushed.
