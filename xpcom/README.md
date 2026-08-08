# Wild Buzzard foundation workspace

The five foundation crates are members of the root Wild Buzzard workspace. The root manifest and
lockfile are the only canonical dependency graph; the temporary Wave 1 nested workspace has been
removed after integration review.

## Crate graph

```text
wild_buzzard_handles
  |-- wild_buzzard_services
  |     `-- wild_buzzard_ipc
  `-- wild_buzzard_platform

wild_buzzard_runtime  (independent)
```

- `memory/rust/wild_buzzard_handles`: typed generational identity and storage.
- `xpcom/rust/wild_buzzard_services`: typed service contracts, wire identities, and `Arc` lookup.
- `ipc/rust/wild_buzzard_ipc`: versioned protocol/domain envelopes and bounded payload codecs.
- `mozglue/rust/wild_buzzard_runtime`: cancellation, lifecycle, bounded events, and manual tasks.
- `widget/rust/wild_buzzard_platform`: checked geometry, input values, and surface identities.

Every crate forbids unsafe code and has no telemetry, provider service, network access, C/C++, or
operating-system FFI. Handles and IDs are identity tokens rather than references. Shared mutable
owners constrain access with Rust ownership or standard-library locks. Queue producers have explicit
bounds and backpressure.

IPC protocol/domain IDs, their local message-kind assignments, and stable service kinds must come
from the orchestrator-owned checked-in registry before cross-component adoption. The code reserves
zero IDs; caller-owned handler and service-contract tables reject duplicates. There is no mutable
global registry.

The concrete product and acceptance target is only `x86_64-unknown-linux-gnu`, using Wayland and X11
as required and distributed as an AppImage. The foundation contracts intentionally contain no
native handles, so the later Linux adapters remain narrow. No Windows, macOS, Android, or mobile
implementation belongs in this workspace.

## Firefox reference

The implementation was informed by the ignored, read-only Firefox ESR153 checkout at commit
`c19b7e89270787889495688244ec6ee8e79288a1`. Each crate README lists the exact implementation,
test, and history paths inspected. The workspace has no path, build, test, or runtime dependency on
`firefox/` and must remain buildable when that checkout is absent.

## Current limitations

This wave deliberately does not provide a process launcher, sandbox, transport, generated protocol
bindings, async executor, clocks/timers, preferences, profiles, localization, allocator replacement,
Wayland/X11 adapter, native event conversion, clipboard, drag-and-drop, or renderer. These are later
layers over the validated contracts, not hidden stubs in this workspace.
