# wild_buzzard_linux

This crate owns Wild Buzzard's first Linux `x86_64-unknown-linux-gnu` top-level
window and event loop. It uses winit with only its X11, Wayland, and dynamically
loaded Wayland features. Its public API contains Wild Buzzard value types rather
than winit objects or native handles.

W4-A4P connects that shell to `gfx/wild_buzzard_linux_presenter`. The
`SurfaceDescriptor` published by `Ready` now identifies the exact attached EGL
window presenter. During `RedrawRequested`, callback-scoped control can render
a bounded Wild Buzzard-owned solid frame directly through hardware-accelerated native GL into the native back
buffer and submit an EGL swap. The presenter consumes the winit window, hides
all native handles, treats zero size as suspension, rejects stale
identity/size/sequence, and synchronously holds the event-loop display borrow
through window creation and presenter attachment. The returned window's exact
display identity is checked before context/surface creation.

Shutdown reports one of three explicit outcomes: no presenter was created,
EGL was checked non-current and every Rust owner wrapper released normally, or
a teardown fault caused every still-extant owner to be retained fail-closed.
Glutin does not expose native EGL destructor results, so normal wrapper release
is not an `eglDestroySurface` acknowledgement. `Destroyed` is published only
for the normal wrapper-release outcome; `Stopped` always carries the explicit
presentation outcome.

Startup uses the same rule. A failure before native presenter ownership leaves
`NotCreated`; a function-load or initial-activation failure after ownership
explicitly retires the partial presenter and places `WrappersReleased` or
`RetainedAfterTeardownFailure` in `Stopped` without replacing the primary
startup failure stage.

This remains a presentation prerequisite, not a renderer or browser product.
The diagnostic solid frame is not the live layout/WebRender scene and the EGL
swap result is not desktop-compositor acknowledgement. The direct GPU owner is
designed to carry WebRender without a CPU readback/upload presentation cycle;
that adapter is still open. `gfx/wild_buzzard_headless` remains a separate
off-screen pbuffer implementation and is not used for native presentation.

The dependency is exactly crates.io `winit` 0.30.13, Apache-2.0, checksum
`a6755fa58a9f8350bd1e472d4c3fcc25f824ec358933bba33306d0b63df5978d`.
Its upstream `v0.30.13` tag peels to
`e9809ef54b18499bb4f2cac945719ecc2a61061b`; upstream declares Rust 1.70.0
as its minimum supported Rust version. Defaults are disabled and this crate
selects only `rwh_06`, `wayland`, `wayland-dlopen`, and `x11`. The raw-handle
feature is confined to the internal presenter handoff and does not expose a
handle in this crate's public API. Cargo's universal lockfile
still records inactive Android, Apple, Windows, Web, and other target packages;
those records are not part of Wild Buzzard's supported Linux target graph.

The selected Linux graph includes Rust Wayland/X11 protocol clients and the
transitive `wayland-csd-frame` client-decoration crate. It dynamically opens
the host display/input libraries, including `libwayland-client`, `libX11`,
`libX11-xcb`, `libXcursor`, `libXi`, `libxcb`, `libxkbcommon`, and
`libxkbcommon-x11` when the relevant backend needs them. RandR is reached
through the selected Rust x11rb protocol path rather than a `libXrandr`
dynamic open.
Direct EGL presentation additionally dynamically reaches `libEGL.so.1`, the
vendor desktop OpenGL implementation, `libwayland-egl.so.1` on Wayland, and
`libXrender.so.1` while selecting an EGL-compatible X11 visual. Exact crate,
license, unsafe-boundary, Firefox-history, and teardown provenance is recorded
in `gfx/wild_buzzard_linux_presenter/README.md`.
The AppImage lane must audit the exact target graph and decide, library by
library, whether a soname is bundled or treated as part of the host ABI; it
must then run both backend smokes against the packaged artifact. Merely seeing
non-Linux records in `Cargo.lock` is neither a product dependency nor proof
that they are absent from a target binary.

Default tests are display-free and cover pure configuration, normalization,
identity, queue, presenter limits, exact frame admission, and state-machine
behavior. Native backend lifecycle/presentation is checked only by the opt-in
smoke. The `real-display-smoke` feature admits an ignored subprocess
executable; run it explicitly from a real session with
`WILDBUZZARD_DISPLAY_BACKEND` set to exactly `wayland` or `x11`. It verifies the
exact redraw identity, native-back-buffer pixel, successful swap submission,
checked non-current state, and normal Rust-wrapper release. Build artifacts belong under the
external `../wildbuzzardbuilds/` tree.

Known evidence limits are explicit. Non-`Removed` raw device events are
currently ignored without increasing `ignored_native_events`; sealing the
ordinary queue discards pending events without a separate suppressed-event
counter. A panic in the user callback terminates the protocol and therefore is
not promised to publish `Destroyed` or `Stopped`. The real-display smoke relies
on an external timeout and cannot observe whether the desktop compositor
actually displayed the successfully submitted buffer. These are follow-up
obligations, not browser-window parity.
