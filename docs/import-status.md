# Rust source import status

This repository contains reusable Rust source selected from the Firefox ESR153 reference at
`c19b7e89270787889495688244ec6ee8e79288a1` plus explicitly recorded independent upstream engine
source. The ignored `firefox/` checkout remains the complete read-only implementation and history;
it is not a build input.

The import deliberately does not copy Firefox's entire `third_party/rust` vendor directory. A file
being Rust does not make it an independent engine component: many Firefox crates are generated
Gecko bindings, XPCOM adapters, platform shims, duplicate registry packages, or provider code.
`docs/upstream-components.toml` is the authoritative per-component provenance registry.

## Admission state

| State | Source currently present | Build meaning |
| --- | --- | --- |
| Active root workspace | `gfx/qcms`, the Wild Buzzard renderer/headless/text crates, `modules/libpref/parser`, `third_party/skv` | built and tested by root `cargo test --workspace` |
| Active nested workspace | WebRender Rust core packages in `gfx/wr`; Stylo Rust core in `servo` | independently locked and tested; prohibited Gecko/C++ features are removed or fail closed |
| Adaptation required engines | exact Brimstone snapshot in `js/brimstone`; exact Wasmtime superproject and core spec suite in `js/wasmtime` | canonical JS and Wasm execution baselines, independently buildable but prohibited for untrusted pages until their safety, host, resource, and conformance gates in `AGENTS.md` pass |
| Adaptation required | small certificate/client-certificate crates; WebDriver tooling | useful Rust algorithms remain coupled to generated Gecko or Firefox interfaces |
| Pinned source | Neqo, wgpu/Naga, WHATWG URL, mp4parse, authenticator | exact Firefox-selected source; normalized manifests are not an editable canonical workspace |
| Transitional | audioipc/Cubeb Rust layers | usable bootstrap code around native audio libraries, not an all-Rust endpoint |
| Quarantined | selected Application Services local-data source | excluded from root builds until Mozilla Sync, NSS, and provider assumptions are separated |

## Imported component groups

- Brimstone under `js/brimstone/`, pinned from its independent upstream at
  `b544eff181ef6a72639f26a89b6aca1f8d6e6b50`. It is the canonical JavaScript execution baseline,
  not a completed browser engine. The first safety adaptation adds exactly-once owned contexts,
  lifetime-branded moving roots, raw-API quarantine, leak-clean focused sanitizer tests, and fixes
  for heap-metadata teardown/resize ownership. It conditionally admits contained JIT infrastructure
  work only. Remaining raw internals, host bindings, resource/interrupt controls, full conformance,
  and the Linux x86-64 baseline/optimizing JIT still block DOM or untrusted-page use.
- Wasmtime under `js/wasmtime/`, pinned at v47.0.3 revision
  `5554cc1a651da536af2cc46c7324bdc085b162e3`, plus the exact core WebAssembly specification suite
  at `0dc0343c9876267d99a7577ed4fc2289406a7869`. All 6,859 superproject blobs and 296 materialized
  spec-suite blobs were verified by Git mode, blob ID, and path. Component Model and WASI test-suite
  gitlink payloads are intentionally absent; the 210.86 MiB WASI payload is not a web-platform
  runtime dependency. The source is not yet a root-workspace dependency or browser API.
- The first-party Rust text contracts under `gfx/wild_buzzard_text` and
  `gfx/wild_buzzard_text_webrender`, using locked Parley/Fontique/HarfRust/Fontations/ICU4X crates
  and an exact OFL-licensed Fira Code fallback. These crates shape and emit real WebRender glyphs;
  the remaining layout-owned shaped-object handoff and full script/font/CSS behavior are explicit
  parity work.
- WebRender and qcms under `gfx/`.
- Stylo's style, selectors, traits, derive, allocation, arc, and shared-memory support crates under
  `servo/components/`, plus the exact ESR `malloc_size_of_derive` crate and narrow first-party
  atom/state/preference/platform shims. The immutable `wild_buzzard_stylo_adapter` invokes Stylo's
  real element resolver and publishes exact-revision computed styles to root layout; `geckolib` is
  intentionally absent.
- Neqo QUIC/HTTP3 and its Firefox-selected support source under `third_party/rust/`; Gecko's
  `neqo_glue` is intentionally absent.
- wgpu and Naga source under `third_party/rust/`; Gecko's WebGPU bindings are intentionally absent.
- WHATWG URL/IDNA/form-urlencoding/percent-encoding source.
- mp4parse, audioipc/Cubeb Rust layers, and authenticator-rs source.
- Preference parsing, `skv`, small security Rust components, and WebDriver/mozbase Rust tooling.
- Local Places, password, autofill, and WebExtension-storage source plus the minimum imported
  Application Services support closure. Its Android wrappers and Glean metric/ping definitions are
  intentionally absent. Its remaining Mozilla Sync model is reference-only migration debt.

No imported production manifest may reference `firefox/`. Provider integrations, Firefox Accounts,
Mozilla-operated Sync clients, Glean/FOG, Pocket, VPN, Relay, Monitor, Nimbus, remote settings,
sponsored suggestions, branding assets, and Firefox service credentials are outside product scope.

The imported Wasmtime source is admitted only for the audited configuration with defaults disabled
and `std,runtime,cranelift,gc,gc-drc,threads`; use Cranelift, not Winch. Its locked Linux graph reaches
23 in-tree packages and 59 unvendored registry packages with no Git dependencies. WASI, CLI, WAT,
server, automatic cache, async/stack-switching, profiling, pooling, and component-host capability
layers are not web-platform APIs and may not be enabled as shortcuts. DRC cycle leaks, the
nonfunctional copying collector, Tier-2 shared-memory limitations, and the upstream minimal-feature
unit-test compile gap remain admission blockers for their affected features. The browser-owned
JavaScript `WebAssembly` API, Brimstone/Wasmtime cross-heap rooting, streaming/CSP policy,
ArrayBuffer ownership, promises, interrupts, debugging, resource limits, and error mapping remain
required.

The WebRender import retains upstream Wrench, SWGL, examples, shader-to-C++ tooling, and example
compositor files for migration comparison, but those paths are explicitly excluded from its Cargo
workspace. They include first-party C/C++ and a Gecko-dependent Windows example and are not active
Wild Buzzard components. The active renderer manifest cannot enable SWGL. WebRender's third-party
`glslopt` build dependency, Linux FreeType/fontconfig boundary, and test-only `mozangle`/ANGLE
validator remain recorded native dependencies. Imported non-Linux branches are inactive and will
be pruned when each canonical editable workspace is established.

The Stylo import is now an active, independently locked nested CSS-engine workspace. Its default
Wild Buzzard profile runs the real Mako generator and compiles the imported selector, property,
cascade, and computed-value code without Gecko, XPCOM, C++, bindgen, or the `firefox/` checkout.
Its concrete immutable DOM-trait adapter runs the real Stylo element resolver/restyle completion
path and publishes an owned document/revision-matched style snapshot consumed by root static
layout. The adapter is not yet connected through the loader-to-frame product pipeline, and real
font/device/theme data, a complete UA sheet, live invalidation, shadow DOM, pseudo output, and the
full computed-value/layout surface are still required. Its generated property universe is the
pinned Servo profile, so this admission is not a CSS or Firefox parity claim.

## Known source-snapshot gaps

The following source snapshots are intentionally not in an active workspace. Their
Firefox-normalized Cargo manifests contain 14 unresolved local paths and must not be presented as
buildable yet:

- wgpu/Naga: 8 upstream workspace-only paths (platform dependency crates, test snapshots, and test
  support). Agent 4 must import the exact canonical workspace revision recorded in provenance.
- audioipc/Cubeb: 5 omitted native or sibling workspace paths. Agent 4 must choose and document the
  transition boundary before enabling them.
- `nss-rs`: 1 test-fixture path. It remains transitional because NSS itself is native code.

Quarantined Application Services paths resolve only because their tightly coupled Sync support was
retained for extraction. They are explicitly excluded from production manifests. Agent 5's first
task there is to create provider-neutral local-storage manifests and delete the Sync-facing code,
not to enable the snapshot wholesale.

The exact qcms snapshot does not currently pass the repository toolchain's `cargo fmt --check`.
This is a temporary import-format exemption: format it in a separate, mechanical adaptation change
so provenance review is not mixed with a bulk rewrite. Its default and all-feature tests pass.

## Import acceptance checklist

Before moving any source into an active workspace:

1. Establish an exact canonical source revision and complete local dependency closure.
2. Record licenses, native dependencies, default features, unsafe code, generated bindings, and
   local patches in `docs/upstream-components.toml`.
3. Remove or isolate Gecko/XPCOM, telemetry, branding, provider endpoints, and service credentials.
4. Give the component a Wild Buzzard-owned Rust interface and a smoke or contract test.
5. Prove the enabled build has no file, Cargo, generated-code, or runtime dependency on `firefox/`.
6. Pass its focused tests and the appropriate workspace integration gates.
