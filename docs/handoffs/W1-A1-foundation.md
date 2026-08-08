# W1-A1 foundation handoff

- Task: W1-A1 foundation, lifecycle, IPC, and platform contracts
- Owner: Agent 1 — foundation/platform; integrated and reviewed by the main orchestrator
- Status: Complete for the Wave 1 contract scope; not a process or native-window implementation
- Firefox commit and source paths: ESR153 `c19b7e89270787889495688244ec6ee8e79288a1`; paths are enumerated in each crate README
- Firefox test paths: XPCOM registration/shutdown tests, TaskController tests, IPDL tests, widget event tests, and their recorded history
- Wild Buzzard paths changed: `memory/rust/wild_buzzard_handles`, `xpcom/rust/wild_buzzard_services`, `mozglue/rust/wild_buzzard_runtime`, `ipc/rust/wild_buzzard_ipc`, `widget/rust/wild_buzzard_platform`, root workspace manifests
- Contract added or changed: typed generational handles; typed `Arc` service registries; cancellation/lifecycle/bounded task primitives; protocol-scoped, versioned, size-bounded IPC; checked geometry/input/surface identities
- Tests run and results: owner and orchestrator gates both passed on `x86_64-unknown-linux-gnu`; 37 foundation tests passed, 0 failed/ignored; root workspace 49 tests passed; root check and release build passed; first-party scoped Clippy passed with `-D warnings`; rustdoc passed with `-D warnings`
- Parity evidence: foundational invariants only; no Firefox engine or UI parity claim
- Known behavioral differences: no process transport, sandbox, async executor, clocks/timers, preferences, profiles, localization, Wayland/X11 adapter, clipboard, drag-and-drop, or renderer integration
- Unsafe or FFI introduced: None; every new crate forbids unsafe code
- Licenses and provenance: MPL-2.0 first-party implementation informed by the ignored ESR153 reference; no copied native implementation and no new external dependency
- Provider or network implications: None; no network access, telemetry, or provider endpoint
- Blocked on: Nothing for adoption; concrete protocols must receive IDs from `docs/wire-registry.toml`
- Recommended next action: use these contracts in the network, DOM/JS, renderer, Linux shell, and process-model vertical slices; add only Linux x86_64 native adapters
