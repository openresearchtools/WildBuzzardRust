# Component ownership map

This map turns the six logical workstreams in `AGENTS.md` into non-overlapping default write
scopes. The main orchestrator may narrow a scope for a particular task, but an agent must not
silently expand it.

| Owner | Wild Buzzard paths | Primary Firefox reference roots | First integrated deliverable |
| --- | --- | --- | --- |
| Main orchestrator | root manifests, `docs/`, CI, `third_party/`, cross-component contracts | whole tree and history | reproducible workspace, provenance, shared contracts, integration gates |
| Agent 1 — foundation/platform | `xpcom/`, `ipc/`, `widget/`, `memory/`, `mozglue/`, `intl/`, preferences and profiles | matching roots plus `toolkit/components/*` primitives | event loop, preferences, profile, typed process lifecycle and IPC |
| Agent 2 — JavaScript/WebAssembly | `js/`, including the pinned `js/brimstone/` engine baseline | `js/`, especially `js/src`, tests, and relevant history | hardened Brimstone-backed host runtime, Linux x86-64 JIT tiers, and browser-owned Wasmtime integration growing against Test262 and Wasm spec tests |
| Agent 3 — DOM/style/layout | `dom/`, `layout/`, `parser/`, `servo/` | matching roots and Web Platform Tests | HTML to DOM to Stylo to layout/display-list contract |
| Agent 4 — graphics/media | `gfx/`, `image/`, `media/`, `dom/canvas/`, `dom/webgpu/`, `dom/media/` | matching roots, reftests, media tests | WebRender-backed deterministic frame and screenshot path |
| Agent 5 — network/security/storage | `netwerk/`, `security/`, `storage/`, approved local-data components | matching roots plus selected `third_party/application-services` | loopback HTTP fetch with cancellation, secure policy context, and partitioned storage |
| Agent 6 — product/UI/tooling | `browser/`, product `toolkit/`, `accessible/`, `extensions/`, `devtools/`, `remote/`, `packaging/appimage/`, WebDriver | matching Linux roots and browser tests | Linux Rust UI, navigation, tab, input, accessibility, automation facade, and AppImage |

## Integration order

The first cross-agent slice is intentionally small:

```text
Agent 6 navigation request
        -> Agent 3 URL/document lifecycle
        -> Agent 5 loopback HTTP transport
        -> Agent 3 HTML + DOM + Stylo + layout
        -> Agent 4 display list + WebRender frame
        -> Agent 1 window/surface/event loop
```

Agent 2 joins through the rooted DOM/host contract once this static path is deterministic. Its
canonical execution core is Brimstone, not the transitional first-party interpreter, and its
Wasmtime use remains behind the same reviewed JS/Wasm rooting and browser-policy boundary. Every
arrow needs a versioned public Rust interface and a contract test owned jointly through an explicit
handoff. Business logic stays with the producing component, not in a shared-types crate.

The independently locked `browser/wild_buzzard_engine` crate currently exercises the middle of
this chain synchronously, from numeric-loopback HTTP through real EGL/WebRender readback. It is an
Agent 6-owned integration seam, not a second implementation of transport, parsing, style, layout,
text, or graphics. It does not yet implement the navigation-event facade, window/UI boundary, or a
single composed text-and-decoration frame.

## Scheduling rule

The six labels describe durable ownership, not six permanently running processes. The orchestrator
selects thin parity slices, gives each worker exact writable paths and tests, and integrates often.
Use `docs/handoffs/README.md` whenever a required change crosses an ownership boundary.
