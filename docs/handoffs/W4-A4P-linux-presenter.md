# W4-A4P direct Linux native-window presenter handoff

## Scope and decision

- Task: connect the reviewed winit Wayland/X11 event shell to a first-party renderer/compositor
  presentation boundary and draw and submit one Wild Buzzard-owned frame for compositor
  consideration in a real native window.
- Owner: Agent 4 — graphics/GPU; integration and independent review remain with the main
  orchestrator.
- Status: GO for this bounded prerequisite after a fresh locked gate and live Wayland/X11 smokes
  for the final-rereview repairs. Those repairs replace glutin's unchecked extent getters with
  checked raw EGL queries and propagate partial-owner startup teardown. Only the final-rereview
  evidence below supports acceptance. This remains NO-GO for a browser/UI, live `WebRender` scene
  presentation, GPU-process isolation, or AppImage claim regardless.
- Wild Buzzard paths: new `gfx/wild_buzzard_linux_presenter/`; focused integration changes under
  `widget/rust/wild_buzzard_linux/`; this handoff. No headless-renderer code was changed.
- Root integration: the orchestrator added `gfx/wild_buzzard_linux_presenter` to the root workspace
  and refreshed/reviewed `Cargo.lock`; this component owner did not modify either
  orchestrator-owned file.

The selected design is direct EGL presentation. The public synchronous `prepare_and_attach`
transaction holds the event loop's display borrow while a private bootstrap selects an exact
hardware-accelerated native-window-compatible EGL config, the shell creates the winit window, and
attachment completes. On X11 the closure receives only the value-only exact visual. The returned
window's complete `RawDisplayHandle` must equal the prepared display before any unsafe context or
surface creation. The private bootstrap then consumes the `Window` into `LinuxPresentedWindow`;
callers cannot retain a movable two-phase EGL owner. Rendering occurs against the native default
framebuffer and a successful frame is submitted with the EGL window surface's swap operation. There is no CPU
screenshot/readback/upload presentation loop. The one-pixel readback in the initial solid-frame
proof is diagnostic evidence only and must be removed from the normal WebRender frame path.

## Public contract and ownership proof

- One presenter owns one exact generational `SurfaceId`, `PhysicalSize`, `ScaleFactor`,
  `Rgba8Srgb` format, and `Window` role. A foreign identity, inexact size, unsupported
  format/role, zero or nonmonotonic frame sequence, and fixed resource-limit violation fail
  before drawing or sequence publication.
- The fixed caller-nonenlargeable limits are 16,384 pixels per axis, 67,108,864 pixels,
  256 MiB RGBA8-equivalent bytes, and `u64::MAX - 1` successful submissions. Counts never wrap.
- The actual EGL width and height are queried after surface creation/resizing and before every
  frame through the exact retained glutin `AsRawDisplay`/`AsRawSurface` EGL pointers. A non-null
  `eglQuerySurface` address is loaded through that display, WIDTH and HEIGHT each require canonical
  `EGL_TRUE`, returned `EGLint` values must be positive and safely convertible to `u32`, and the
  pair must exactly match. Glutin's `Surface::width`/`height` wrappers are not accepted because
  their EGL implementation discards the `eglQuerySurface` return value.
- Zero width or height removes the EGL window surface and enters `Suspended`. A later nonzero
  resize/resume recreates the exact surface before another frame. Scale changes preserve the
  last exact physical size and never fabricate a resize.
- `DirectRenderer` receives a callback-scoped `DirectFrameTarget`. It exposes identity, size,
  sequence, and narrow complete-frame operations, but no unrestricted GL object, native handle,
  winit object, or ownership token. A future WebRender adapter must remain inside this authority
  boundary rather than exporting raw graphics access.
- The private bootstrap and `LinuxPresentedWindow` carry explicit `PhantomData<Rc<()>>`
  owner-thread markers. A compile-fail doctest proves the public presenter cannot cross threads;
  prepared native state never escapes the synchronous transaction.
- Normal teardown first proves the context non-current, then releases the GL table, EGL-surface,
  context, config/display, and winit/native-window Rust wrappers in order. Glutin calls
  `eglDestroySurface` and other native destructors from `Drop` without exposing their return
  values. `PresentationShutdownReport` therefore proves only checked non-current admission and
  normal wrapper release; it is not native-destruction acknowledgement.
- If checking, unbinding, or releasing a wrapper errors or panics, each still-extant dependency
  owner is deliberately retained. `PresentationRetentionReport` records the stable stage/kind,
  the shell publishes `RetainedAfterTeardownFailure` in `Stopped`, retires the logical surface ID,
  and does not fabricate `Destroyed`. A wrapper whose own `Drop` panicked cannot itself be claimed
  as retained.
- A failure before native presenter ownership reports `NotCreated`. A function-load or initial
  surface-activation failure after ownership instead returns `PresentationStartupFailure`, keeping
  the primary stage/class separate from an exact `PresentationTeardownOutcome`. The partial owner
  is consumed into one explicit shutdown; clean release, retention, and escaped teardown panic are
  converted respectively into `WrappersReleased` or `RetainedAfterTeardownFailure` and threaded to
  the shell's `Stopped` report without double teardown.

`SwapSubmissionReceipt` means exactly: the bounded renderer completed, the diagnostic sample
matched, and EGL accepted the swap call for the named identity/size/sequence. It does not mean the
desktop compositor latched, displayed, or presented that buffer, and
`compositor_acknowledged()` is therefore always false.

## Startup and failure policy

The first accepted startup profile is deliberately strict:

- `x86_64-unknown-linux-gnu`, Wayland or X11 only;
- EGL window surface, desktop OpenGL 3.2 core profile, and robust
  lose-context-on-reset semantics;
- hardware-accelerated config only; non-float RGBA8 with eight alpha bits, sRGB capability, no
  multisampling, and the exact selected X11 visual when using X11;
- no GLES fallback, legacy GL fallback, non-sRGB fallback, software-copy fallback, or transparent
  substitution of a different X11 visual;
- swap interval `DontWait` for this proof; frame pacing/vsync policy is not yet selected.

This deliberately excludes machines which cannot meet the strict first profile. A future fallback
or context-recreation gate must define an ordered policy, preserve exact surface identity and
resource accounting, reset every renderer-owned GL resource, prove loss/recovery on both native
backends, and never silently switch to CPU presentation.

Failures are classified by stable stage: display/window handle, display/config/context/surface
creation, make-current, GL function load, swap configuration, resize/recreate, draw, swap, and
release. Native driver/GL/context failures, renderer panic, and native-back-buffer diagnostic
mismatch poison the presenter at the first failure stage. A diagnostic mismatch is terminal
because the boundary can no longer trust its draw-target integrity proof. A renderer-declared
`Rejected`, `NoCompleteFrame`, or `MultipleCompleteFrames` result is retryable and does not commit
the frame sequence.

Diagnostic readback is into a preinitialized `[0_u8; 4]` through
`read_pixels_into_buffer`. The target checks GL status immediately after the read and before
interpreting any byte. Its first GL/context-loss or diagnostic-integrity fault is authoritative:
it wins even when a renderer ignores it, remaps it to a retryable error, or panics afterward.
Native config/current/swap/unbind calls are panic-contained and classified at their exact stage.

## Firefox implementation, tests, and history inspected

The read-only detached ESR153 baseline is
`c19b7e89270787889495688244ec6ee8e79288a1`. The focused implementation references were:

- `gfx/webrender_bindings/RenderCompositorEGL.cpp/.h`;
- `gfx/webrender_bindings/RenderCompositorOGL.cpp`;
- `gfx/gl/GLContextProviderEGL.cpp`;
- relevant `widget/gtk/` compositor-widget and surface ownership code.

The full-history review included:

- `ab9879a559cba5d8593cae1b9d061ecedc27f335` — make an EGL surface non-current before destroying
  it;
- `2835f8b9a9396a363aa36e4ea46664d2701ab284` — update a Wayland EGL surface when its
  `wl_surface` is deleted;
- `abd58ab1be7a5a54e80ba7cddc87a31b48c81445` — refresh the X11 EGL surface on compositor resume.

Wild Buzzard adopts the observable ownership, resize/resume, explicit default-buffer, and
fail-closed lessons. It does not copy Gecko's C++ compositor object graph or use Firefox source as
a build input.

## Rust and native dependency closure

First-party source is MPL-2.0 and adds no first-party C or C++. The exact Rust dependencies and
selected features are recorded in `gfx/wild_buzzard_linux_presenter/README.md`:

- glutin 0.32.3: `egl,wayland,x11` only;
- gleam 0.15.1;
- raw-window-handle 0.6.2;
- winit 0.30.13: `rwh_06,wayland,wayland-dlopen,x11`, defaults disabled;
- first-party `wild_buzzard_platform`.

The native runtime boundary includes `libEGL.so.1`, the vendor desktop OpenGL implementation,
`libwayland-egl.so.1` on Wayland, and the display/input libraries in winit's existing Wayland/X11
closure. X11 EGL visual selection additionally reaches `libX11.so.6` and `libXrender.so.1`.
Final AppImage work must audit both static `DT_NEEDED` and runtime `dlopen` closure from the
packaged executable and decide host-ABI versus bundled treatment for every soname; this source
gate is not that packaging decision.

The pre-review runtime-closure audit used `readelf`, `ldd`, and successful `LD_DEBUG=libs` runs.
The hostile repair changed no manifest, lockfile, feature, or dependency edge. A fresh post-repair
`readelf` check confirmed that the current smoke binary's static `DT_NEEDED` entries remain only
`libgcc_s.so.1`, `libm.so.6`, `libc.so.6`, and `ld-linux-x86-64.so.2`, with no RPATH/RUNPATH. The
unchanged graph's earlier dynamic-loading observation initialized 52 unique DSOs on Wayland and 59
on X11/Xwayland. It remains dependency provenance rather than a fresh binary-identity claim. The
exact 51 common sonames observed on this GLVND host were:

```text
ld-linux-x86-64.so.2 libEGL.so.1 libEGL_mesa.so.0 libEGL_nvidia.so.0
libGLdispatch.so.0 libLLVM.so.21.1 libX11-xcb.so.1 libX11.so.6 libXau.so.6
libXdmcp.so.6 libbsd.so.0 libc.so.6 libdl.so.2 libdrm.so.2
libdrm_amdgpu.so.1 libdrm_intel.so.1 libedit.so.2 libelf.so.1 libexpat.so.1
libffi.so.8 libgallium-26.0.3-1ubuntu1.so libgbm.so.1 libgcc_s.so.1 libm.so.6
libmd.so.0 libnvidia-egl-gbm.so.1 libnvidia-egl-wayland2.so.1
libnvidia-egl-xcb.so.1 libnvidia-egl-xlib.so.1 libnvidia-eglcore.so.610.43.02
libnvidia-glsi.so.610.43.02 libnvidia-gpucomp.so.610.43.02 libpciaccess.so.0
libpthread.so.0 librt.so.1 libsensors.so.5 libstdc++.so.6 libtinfo.so.6
libwayland-client.so.0 libxcb-dri3.so.0 libxcb-present.so.0 libxcb-randr.so.0
libxcb-shm.so.0 libxcb-sync.so.1 libxcb-xfixes.so.0 libxcb.so.1
libxkbcommon.so.0 libxml2.so.16 libxshmfence.so.1 libz.so.1 libzstd.so.1
```

Wayland additionally initialized `libwayland-egl.so.1`. X11 additionally initialized
`libXcursor.so.1`, `libXext.so.6`, `libXfixes.so.3`, `libXi.so.6`, `libXrender.so.1`,
`libxcb-glx.so.0`, `libxcb-xkb.so.1`, and `libxkbcommon-x11.so.0`. This is exact evidence for the
tested Ubuntu/GLVND installation, not a portable closure: EGL vendor discovery initialized both
Mesa and NVIDIA stacks, and versions/driver transitive libraries will differ on another host.
That variability is why AppImage acceptance must treat the system display/EGL/driver stack as an
explicit host-ABI policy rather than blindly copying this list.

Seven necessary first-party unsafe calls are quarantined in `egl_window.rs`: create an EGL display
from a live borrowed display handle, enumerate that display's configs, create its context, create
the window surface from the consumed live window handle, convert the exact-display non-null
`eglQuerySurface` address to its fixed local Linux C ABI, invoke it with the retained raw EGL
objects and writable `EGLint`, and load desktop-GL functions from the exact current display. Each
call has a local `SAFETY` proof. Contract/state and event-shell code forbid unsafe code. No native
handle or function table escapes the owner.

## Verification evidence

The final rereview superseded the previous 45-test acceptance evidence after identifying the two
medium blockers repaired above. Every command below was rerun against the repaired source. All
build output remained external under `/home/user/Documents/wildbuzzardbuilds/w4-a4p/`. The exact
toolchain was Cargo 1.96.0 (`30a34c682`) and rustc 1.96.0 (`ac68faa20`, LLVM 22.1.2), host and target
`x86_64-unknown-linux-gnu`.

```sh
rustfmt --edition 2024 --check \
  gfx/wild_buzzard_linux_presenter/src/contract.rs \
  gfx/wild_buzzard_linux_presenter/src/egl_window.rs \
  gfx/wild_buzzard_linux_presenter/src/lib.rs \
  widget/rust/wild_buzzard_linux/src/bin/real_display_smoke.rs \
  widget/rust/wild_buzzard_linux/src/config.rs \
  widget/rust/wild_buzzard_linux/src/event.rs \
  widget/rust/wild_buzzard_linux/src/lib.rs \
  widget/rust/wild_buzzard_linux/src/lifecycle.rs \
  widget/rust/wild_buzzard_linux/src/normalize.rs \
  widget/rust/wild_buzzard_linux/src/queue.rs \
  widget/rust/wild_buzzard_linux/src/shell.rs \
  widget/rust/wild_buzzard_linux/tests/real_display.rs
CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w4-a4p/target \
  cargo check -p wild_buzzard_linux_presenter -p wild_buzzard_linux --all-targets --locked \
  --target x86_64-unknown-linux-gnu
CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w4-a4p/target \
  cargo check --workspace --all-targets --locked --target x86_64-unknown-linux-gnu
CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w4-a4p/target \
  cargo clippy -p wild_buzzard_linux_presenter -p wild_buzzard_linux --all-targets --locked \
  --target x86_64-unknown-linux-gnu -- -D warnings
CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w4-a4p/target \
  cargo test -p wild_buzzard_linux_presenter -p wild_buzzard_linux --locked \
  --target x86_64-unknown-linux-gnu
CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w4-a4p/target \
  RUSTDOCFLAGS=-Dwarnings cargo doc -p wild_buzzard_linux_presenter \
  -p wild_buzzard_linux --no-deps --locked --target x86_64-unknown-linux-gnu
CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w4-a4p/target \
  cargo build -p wild_buzzard_linux_presenter -p wild_buzzard_linux --release --locked \
  --target x86_64-unknown-linux-gnu
CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w4-a4p/target \
  WILDBUZZARD_REAL_DISPLAY_TEST=1 WILDBUZZARD_DISPLAY_BACKEND=wayland \
  cargo test -p wild_buzzard_linux --features real-display-smoke --test real_display --locked \
  --target x86_64-unknown-linux-gnu -- --ignored --exact \
  real_display_smoke_runs_on_the_subprocess_main_thread
CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w4-a4p/target \
  WILDBUZZARD_REAL_DISPLAY_TEST=1 WILDBUZZARD_DISPLAY_BACKEND=x11 \
  cargo test -p wild_buzzard_linux --features real-display-smoke --test real_display --locked \
  --target x86_64-unknown-linux-gnu -- --ignored --exact \
  real_display_smoke_runs_on_the_subprocess_main_thread
readelf -d \
  /home/user/Documents/wildbuzzardbuilds/w4-a4p/target/x86_64-unknown-linux-gnu/debug/\
wild-buzzard-real-display-smoke
git diff --check -- gfx/wild_buzzard_linux_presenter widget/rust/wild_buzzard_linux \
  docs/handoffs/W4-A4P-linux-presenter.md
rg -n '[[:blank:]]+$' gfx/wild_buzzard_linux_presenter \
  widget/rust/wild_buzzard_linux docs/handoffs/W4-A4P-linux-presenter.md
```

Final-rereview results were:

- explicit-file `rustfmt --check`: pass;
- tracked diff check passed and the untracked new crate/handoff contained no trailing whitespace;
- focused package all-target check: pass;
- root `cargo check --workspace --all-targets --locked`: pass;
- package all-target strict Clippy with `-D warnings`: pass;
- 54 unit tests: 27 shell and 27 presenter, all pass; the default display test correctly ran zero
  opt-in cases;
- five earlier hostile-regression tests still pass: initialized destination when the mock driver
  writes nothing, post-read context-loss precedence, swallowed/remapped terminal diagnostic
  authority, first-terminal-fault authority, and staged native panic containment. The earlier
  fabricated-`Option` extent test was removed rather than counted;
- ten final-rereview tests pass: missing query symbol, false and noncanonical `EGLBoolean`, invalid
  dimension, exact checked extent, two-query partial mismatch, query panic staging, partial-owner
  clean release, explicit retention, and teardown-panic retention fallback;
- one compile-fail doctest proving `LinuxPresentedWindow` is not `Send`: pass;
- warning-denied no-dependency rustdoc: pass;
- locked release build: pass;
- selected shell graph: 65 normal packages including the root (64 dependencies) and 74
  normal-plus-build packages including the root (73 dependencies); no selected
  Android, Apple, Windows, Web/wasm-bindgen, GLX, WGL, or default winit CSD-adwaita path;
- direct features matched the manifest: glutin `egl,wayland,x11`; winit
  `rwh_06,wayland,wayland-dlopen,x11` with defaults off;
- the current smoke ELF has only `libgcc_s.so.1`, `libm.so.6`, `libc.so.6`, and
  `ld-linux-x86-64.so.2` in `DT_NEEDED`, with no RPATH/RUNPATH. The dependency graph did not change
  during hostile-review repair, so the already recorded runtime-soname closure remains applicable.

Both live backends exercised the checked raw EGL extent path before a swap receipt published.

The live environment was Ubuntu 26.04 LTS, Linux 7.0.0-28-generic, GNOME Wayland at
`WAYLAND_DISPLAY=wayland-0`, with Xwayland at `DISPLAY=:0`. The ignored opt-in subprocess passed
once with `WILDBUZZARD_DISPLAY_BACKEND=wayland` (1.27 seconds) and once with `=x11` (1.28 seconds).
Those fresh runs verified exact Ready/Redraw identity (and any observed resize identity), direct native
default-framebuffer drawing, diagnostic pixel agreement, successful swap submission, more than
one second before orderly exit, checked non-current admission, normal Rust-wrapper release, and
`Destroyed` before `Stopped`. Neither can observe a desktop-compositor presentation acknowledgement
or native EGL-destructor success.

An independent final read-only hostile rereview inspected the frozen post-repair source and this
handoff and found no Critical, High, Medium, or Low defect. It rechecked the exact retained EGL objects and
checked query ABI, canonical Boolean/value/extent handling, query fault latching, partial-owner
startup cleanup, primary-error plus teardown propagation into `Stopped`, panic-time retention,
double-teardown prevention, and all earlier readback/lifetime/fault/hardware/destruction findings.
The reviewer ran no additional Cargo/rustc command because the frozen post-repair matrix above was
already complete.

## Explicit non-claims and next work

This gate does not present `gfx/wr`'s `Renderer`, a `CompiledScene`, shaped text, live engine
navigation, browser chrome, Canvas/WebGL/WebGPU, media, retained damage, buffer age, frame
callbacks, vsync, multiple windows, a GPU process, hang recovery, compositor confirmation,
accessibility, or packaged AppImage output. Its solid frame proves only a bounded first-party
direct-GPU native-window path.

The next graphics task is an internally owned WebRender window adapter which consumes immutable
display lists/transactions without exporting GL authority, then proves exact surface resize,
renderer epoch, device-loss, frame pacing, and teardown behavior on both Wayland and X11. The
engine/UI lane must use typed frame and input contracts and must not reach into this presenter's
native implementation.
