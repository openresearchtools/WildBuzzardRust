# wild_buzzard_linux

This crate owns Wild Buzzard's first Linux `x86_64-unknown-linux-gnu` top-level
window and event loop. It uses winit with only its X11, Wayland, and dynamically
loaded Wayland features. Its public API contains Wild Buzzard value types rather
than winit objects or native handles.

This is an event-shell prerequisite, not a renderer or browser product. The
`SurfaceDescriptor` published by `Ready` contains the configured **desired**
pixel format. This crate does not allocate or present Wild Buzzard
browser-content pixels; winit or the selected backend may still manage
decoration or toolkit-owned buffers. `gfx/wild_buzzard_headless` remains an
off-screen pbuffer implementation and is not a dependency.

The dependency is exactly crates.io `winit` 0.30.13, Apache-2.0, checksum
`a6755fa58a9f8350bd1e472d4c3fcc25f824ec358933bba33306d0b63df5978d`.
Its upstream `v0.30.13` tag peels to
`e9809ef54b18499bb4f2cac945719ecc2a61061b`; upstream declares Rust 1.70.0
as its minimum supported Rust version. Defaults are disabled and this crate
selects only `wayland`, `wayland-dlopen`, and `x11`. Cargo's universal lockfile
still records inactive Android, Apple, Windows, Web, and other target packages;
those records are not part of Wild Buzzard's supported Linux target graph.

The selected Linux graph includes Rust Wayland/X11 protocol clients and the
transitive `wayland-csd-frame` client-decoration crate. It dynamically opens
the host display/input libraries, including `libwayland-client`, `libX11`,
`libX11-xcb`, `libXcursor`, `libXi`, `libxcb`, `libxkbcommon`, and
`libxkbcommon-x11` when the relevant backend needs them. RandR is reached
through the selected Rust x11rb protocol path rather than a `libXrandr`
dynamic open.
The AppImage lane must audit the exact target graph and decide, library by
library, whether a soname is bundled or treated as part of the host ABI; it
must then run both backend smokes against the packaged artifact. Merely seeing
non-Linux records in `Cargo.lock` is neither a product dependency nor proof
that they are absent from a target binary.

Default tests are display-free and cover pure configuration, normalization,
identity, queue, and state-machine behavior. Native backend lifecycle is
checked only by the opt-in smoke. The `real-display-smoke` feature admits an
ignored subprocess executable; run it explicitly from a real session with
`WILDBUZZARD_DISPLAY_BACKEND` set to exactly `wayland` or `x11`. Build artifacts
belong under the external `../wildbuzzardbuilds/` tree.

Known evidence limits are explicit. Non-`Removed` raw device events are
currently ignored without increasing `ignored_native_events`; sealing the
ordinary queue discards pending events without a separate suppressed-event
counter. A panic in the user callback terminates the protocol and therefore is
not promised to publish `Destroyed` or `Stopped`. The real-display smoke relies
on an external timeout and does not yet assert the `RedrawRequested` surface
identity. These are follow-up obligations, not browser-window parity.
