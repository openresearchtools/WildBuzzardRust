# wild_buzzard_linux_presenter

This crate is Wild Buzzard's first direct Linux native-window presentation
boundary. It targets only `x86_64-unknown-linux-gnu`, consumes the winit
`Window`, creates a robust desktop-GL context and EGL window surface for the
exact Wayland or X11 display, renders into the native default framebuffer, and
submits completed back buffers with `eglSwapBuffers` through glutin.

It is deliberately not a software-copy compositor. The first admitted frame
source is a bounded solid-color diagnostic which executes a hardware-accelerated native-GL clear directly
against the native back buffer. A one-pixel synchronous readback proves that
the requested color reached that back buffer before swap; this diagnostic
readback is transitional test evidence, not the presentation mechanism. The
same owner and default-framebuffer path is intended to host the imported
WebRender renderer without a CPU screenshot/readback/upload cycle.

## Ownership and authority

The public `prepare_and_attach` transaction borrows the event loop's display,
privately selects a hardware-accelerated window-capable RGBA8 sRGB EGL config,
and invokes a synchronous native-window creation closure while that borrow is
still live. On X11 the closure receives only the value-only exact visual to
apply to winit's attributes. Before any unsafe context or surface creation, the
returned window's complete `RawDisplayHandle` must equal the prepared display.
The private two-phase state then consumes the window into
`LinuxPresentedWindow`; callers cannot retain or move a prepared EGL owner.
If function loading or initial surface activation fails after that owner is
established, the creation error preserves both the primary startup failure and
an explicit `PresentationTeardownOutcome`. The partial owner is synchronously
retired exactly once; a clean wrapper release or fail-closed retention outcome
is propagated into the shell's terminal report. `NotCreated` is reserved for
failures before a native presenter owner exists.

The owner releases its Rust wrappers in this dependency order:

1. the GL function table;
2. the EGL window-surface wrapper;
3. the robust desktop-GL context wrapper;
4. the EGL config/display wrappers;
5. the winit/native-window wrapper.

Normal shutdown first checks and, when needed, makes EGL non-current, then
releases those wrappers in order. Glutin performs `eglDestroySurface` and other
native destructor calls inside `Drop` and discards their return values.
`PresentationShutdownReport` therefore proves checked non-current state and
normal Rust-wrapper release only; it is not acknowledgement that EGL destroyed
a native object. An error or panic while checking, unbinding, or releasing a
wrapper produces `PresentationRetentionReport`; every still-extant native
owner is deliberately leaked fail-closed. The shell reports that outcome as
`RetainedAfterTeardownFailure` and does not publish `Destroyed`.

Raw Wayland, X11, EGL, GL, and winit window handles never enter the public
event-handler contract. Direct renderers receive a callback-scoped
`DirectFrameTarget` with only bounded operations. It cannot return an
unrestricted GL object or native handle. A future WebRender adapter belongs in
this crate and must add a narrow operation to that target or become an
internally owned renderer; it must not export the GL table as a general escape
hatch.

The private bootstrap and `LinuxPresentedWindow` carry explicit owner-thread
markers. A compile-fail `Send` assertion covers the public presenter. Prepared
native state never crosses the synchronous `prepare_and_attach` call, and the
attached window cannot move away from its winit event-loop owner thread even if
an upstream dependency changes its auto-traits.

## Surface and failure contract

- Exactly one generational `SurfaceId`, physical size, scale, and
  `Rgba8Srgb`/`Window` descriptor belong to the owner.
- Width and height are capped at 16,384; area is capped at 67,108,864 pixels;
  the RGBA8-equivalent allocation is capped at 256 MiB.
- A frame names the exact live identity and physical size and supplies a
  nonzero, strictly increasing sequence. The lifetime admits at most
  `u64::MAX - 1` successful submissions and never wraps.
- A zero width or height releases the EGL surface wrapper and enters `Suspended`.
  Nonzero resize/resume recreates it or resizes it before another frame.
  Scale changes update metadata independently and do not fabricate a resize.
- Make-current, GL, swap, surface-recreation, and unbind failures have exact
  stable stages. Runtime driver/context failures move the presenter to
  terminal `Lost`; they are not retried through a stale surface.
- The actual EGL width and height are queried after creation/resize and before
  every submission through a checked `eglQuerySurface` function loaded from
  the exact retained EGL display. The retained glutin raw display/surface
  pointers must be non-null EGL objects; each `EGLBoolean` must be canonical
  true, each `EGLint` must be positive and safely convertible to `u32`, and the
  pair must exactly match the requested extent. Glutin's unchecked
  `Surface::width`/`height` helpers are not used for this proof.
- Diagnostic readback uses a preinitialized four-byte destination. GL status is
  inspected immediately after `read_pixels_into_buffer`, before any returned
  byte is interpreted. The first GL/context-loss or diagnostic-integrity fault
  is latched by the target and overrides a renderer that swallows, remaps, or
  panics after observing it.
- A `SwapSubmissionReceipt` proves a verified draw followed by a successful
  EGL swap call. EGL supplies no desktop-compositor acknowledgement, so the
  receipt always reports `compositor_acknowledged() == false` and must not be
  described as proof that a human saw the frame.

The opt-in real-display smoke in `wild_buzzard_linux` opens the selected
backend, verifies exact Ready/Redraw surface identity, submits a blue Wild
Buzzard frame, checks its native-back-buffer sample and swap receipt, leaves it
available to the compositor for more than one second, and verifies checked
non-current admission plus normal wrapper release. It must be run separately
for Wayland and X11.

## Unsafe boundary

First-party unsafe is limited to the operations glutin and gleam require:

- create EGL `Display` from winit's live borrowed display handle;
- enumerate configs and create a context belonging to that display;
- create the EGL window surface from the consumed window's live handle;
- convert the non-null exact-display `eglQuerySurface` address to its local
  Linux C-ABI function type;
- call that function with the retained raw EGL display/surface pair and valid
  writable `EGLint` storage;
- load the GL function table from the exact current EGL display.

Each call has a local `SAFETY` proof. The crate denies
`unsafe_op_in_unsafe_fn`; all contract/state code is safe Rust. No first-party C
or C++ is introduced.

## Dependency and native provenance

No new crate version is introduced beyond versions already locked for the
headless renderer and Linux shell. This crate activates the window-system
features which the headless pbuffer does not use:

- `glutin = 0.32.3`, crates.io checksum
  `12124de845cacfebedff80e877bb37b5b75c34c5a4c89e47e1cdd67fb6041325`,
  upstream source commit `20d1c103172aa4025f02cc94ca16a3169bea789c`,
  Apache-2.0; exact features `egl,wayland,x11`;
- `gleam = 0.15.1`, checksum
  `8647cc2e2ffde598ce5ca2809452e722dd8dc127885ab8aba2fa8b469cd3ed94`,
  commit `e7b3f3296c70093f35357e644dc8711c1f79fc3d`, MIT OR
  Apache-2.0;
- `raw-window-handle = 0.6.2`, checksum
  `20675572f6f24e9e76ef639bc5552774ed45f1c30e2951e1e99c59888861c539`,
  commit `5fda8e8420b069368e9450e70c2869e32dcdffc1`, MIT OR
  Apache-2.0 OR Zlib;
- `winit = 0.30.13`, checksum
  `a6755fa58a9f8350bd1e472d4c3fcc25f824ec358933bba33306d0b63df5978d`,
  commit `e9809ef54b18499bb4f2cac945719ecc2a61061b`, Apache-2.0;
  defaults disabled with exactly `rwh_06,wayland,wayland-dlopen,x11`.

The direct presenter dynamically reaches `libEGL.so.1` and the vendor desktop
OpenGL implementation. Glutin's Wayland window surface additionally opens
`libwayland-egl.so.1`; winit's existing Wayland path opens
`libwayland-client.so.0` and its recorded xkbcommon dependencies. Glutin's X11
visual selection opens `libX11.so.6` and `libXrender.so.1`; winit retains its
existing X11/XCB/Xcursor/XInput closure. AppImage work must audit the final ELF
`DT_NEEDED` and `dlopen` closure on both backends and decide the host-ABI versus
bundling policy for every soname.

The current repaired-source host smoke binary has only `libgcc_s`, `libm`, `libc`, and the ELF
loader in `DT_NEEDED`; display, EGL, and driver libraries are dynamically
opened. Because the hostile repair changed no dependency edge, the earlier
`LD_DEBUG=libs` graph observation remains useful provenance: 52 initialized
DSOs on Wayland and 59 on X11/Xwayland on the tested Ubuntu 26.04 GLVND host,
including both probed Mesa and NVIDIA vendor stacks. It is not a claim about
the identity of the current binary. The exact host-specific soname inventory and the
reason it is not a portable AppImage closure are recorded in
`docs/handoffs/W4-A4P-linux-presenter.md`.

## Firefox reference evidence

The design was checked against the detached ESR153 reference, especially:

- `gfx/webrender_bindings/RenderCompositorEGL.cpp/.h`;
- `gfx/webrender_bindings/RenderCompositorOGL.cpp`;
- `gfx/gl/GLContextProviderEGL.cpp`;
- `widget/gtk/` surface and compositor-widget ownership.

The relevant history includes `ab9879a559cb` (make an EGL surface non-current
before destroying it), `2835f8b9a939` (Wayland surface deletion/resume), and
`abd58ab1be7a` (refresh X11 EGL surfaces on resume). Wild Buzzard adopts the
observable ownership/failure lessons, not Gecko's C++ object graph.

## Not yet claimed

This gate does not yet render a `CompiledScene` in a native window, connect the
live browser engine to WebRender presentation, implement damage/buffer-age,
vsync/frame callbacks, GPU-process isolation, compositor confirmation,
context recreation/fallback, browser chrome, multiple windows, or AppImage
acceptance. It is a direct presentation prerequisite, not a browser/UI/parity
claim.
