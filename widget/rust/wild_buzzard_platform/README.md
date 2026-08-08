# wild_buzzard_platform

This crate defines pointer-free geometry, input, and surface contracts for the UI, engine, and
renderer boundaries. Logical floating-point inputs reject NaN and infinity; sizes reject negative or
unbounded extents; surfaces have explicit dimension and area limits; input strings and normalized
values are bounded. Surface identities carry an explicit allocator/process namespace and a typed
generation, so releasing and reusing a slot does not revive a stale target or alias another
allocator.

The types are native-handle-free and `Send + Sync` where their fields allow. This does not imply a
cross-platform product target: Wild Buzzard targets only `x86_64-unknown-linux-gnu`, with future
Wayland/X11 adapters and AppImage distribution. This wave contains no Wayland, X11, process, GPU,
clipboard, drag-and-drop, or OS FFI code.

Firefox ESR153 reference paths inspected at
`c19b7e89270787889495688244ec6ee8e79288a1`:

- `widget/BasicEvents.h`
- `widget/MouseEvents.h`
- `widget/TextEvents.h`
- `widget/InputData.h`
- `widget/CompositorWidget.h`
- `widget/generic/PCompositorWidget.ipdl`
- `widget/headless/HeadlessCompositorWidget.cpp`
- `widget/tests/test_assign_event_data.html`
- `gfx/2d/Point.h`

History for the event and compositor paths was inspected, including changes that tightened event
class initialization and copying. Wild Buzzard uses smaller validated Rust value types rather than
copying native event unions or C++ widget pointers. Wheel/gesture/IME composition, damage regions,
cursor, clipboard, drag-and-drop, and Linux native-handle adapters remain later work.
