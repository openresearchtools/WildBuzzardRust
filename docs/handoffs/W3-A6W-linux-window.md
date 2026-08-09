# W3-A6W Linux window/event-shell handoff

## Accepted scope

W3-A6W admits `widget/rust/wild_buzzard_linux` as Wild Buzzard's first
Linux-only top-level window and event-loop prerequisite. It normalizes a
bounded subset of winit lifecycle, window, keyboard, pointer, touch, IME,
scale, resize, redraw, and wake activity into first-party value types. The
crate targets `x86_64-unknown-linux-gnu`, forbids first-party unsafe code, and
selects Wayland and X11 without winit's default feature set.

This is not a browser-content window, Wild Buzzard renderer/compositor
presentation integration, or UI. Winit owns the native backend window/surface;
the public `SurfaceDescriptor` is desired configuration and identity only. It
exposes no native handles, allocates no renderer-owned browser-content
presentation surface, and does not connect `gfx/wild_buzzard_headless` or
WebRender to the real window.

## Proven lifecycle contract

- Lifecycle is one-way: `Running -> Stopping -> Exited`.
- The first stop request seals and clears ordinary event admission.
- A stop requested from an event callback prevents later ordinary events in
  the same drain from reaching the callback.
- A stop requested during startup suppresses window creation and `Ready`.
- Wake admission becomes permanently closed on stop, drop, or event-loop
  error; a previously issued pending receipt cannot reopen it.
- Scale-factor changes emit only `ScaleFactorChanged`; real native resizes emit
  `Resized`.
- A created surface receives exactly one `Destroyed` before one `Stopped` in a
  normal non-panicking shutdown.
- The opt-in display smoke checks the requested backend, stable surface
  identity across `Ready` and `Destroyed`, terminal ordering/counts, matching
  shutdown report, and a closed post-run wake path.

## Dependency provenance and packaging boundary

- Package: crates.io `winit` 0.30.13.
- Checksum: `a6755fa58a9f8350bd1e472d4c3fcc25f824ec358933bba33306d0b63df5978d`.
- Upstream tag: `v0.30.13`, peeled commit
  `e9809ef54b18499bb4f2cac945719ecc2a61061b`.
- License: Apache-2.0.
- Upstream MSRV: Rust 1.70.0.
- Selected features: defaults off; `wayland`, `wayland-dlopen`, `x11` only.
- Selected Linux closure includes the transitive `wayland-csd-frame` crate and
  dynamically opened Wayland/X11/xkbcommon libraries. The universal lockfile's
  inactive non-Linux records are not supported product dependencies.

AppImage admission is still open. Packaging must audit the exact Linux target
closure, document which display/input sonames are host ABI versus bundled,
verify relocation, and run both Wayland and X11 smokes from the packaged
artifact. All build, smoke, packaging, and extracted-AppDir output belongs
under `/home/user/Documents/wildbuzzardbuilds`, never in the repository.

## Verification evidence

The owner passed formatting, locked all-target checking, 26 package tests,
project-defined strict Clippy with `-D warnings`, release checking,
warning-denied rustdoc, doctests, and forced ignored-test Wayland/X11 runs. An
independent fresh target repeated all package tests, strict project Clippy, and
both live-display smokes. Those builds used external target directories under
`/home/user/Documents/wildbuzzardbuilds`.

The independent source review returned GO for this event-shell scope after the
stop/drain, permanent-wake-closure, and scale/resize distinctions were fixed.
It did not approve browser presentation or AppImage readiness.

## Open obligations

- Count or separately report ignored non-`Removed` device events.
- Account for ordinary events suppressed when the queue is sealed.
- Define a panic boundary if terminal events must survive a client callback
  panic; currently a panic terminates the protocol.
- Give the real-display smoke an internal timeout and validate redraw surface
  identity.
- Connect a Wild Buzzard renderer/compositor presentation surface, input
  routing, accessibility, and browser chrome, then prove shutdown across those
  owners.
- Close the exact AppImage runtime-library and license-notice policy.

Extra pedantic Clippy was not clean and is not claimed: twelve advisory
diagnostics remain. The repository's project-defined strict Clippy gate passed.
