# Rust source import status

This repository contains reusable Rust source selected from the Firefox ESR153 reference at
`c19b7e89270787889495688244ec6ee8e79288a1`. The ignored `firefox/` checkout remains the complete
read-only implementation and history; it is not a build input.

The import deliberately does not copy Firefox's entire `third_party/rust` vendor directory. A file
being Rust does not make it an independent engine component: many Firefox crates are generated
Gecko bindings, XPCOM adapters, platform shims, duplicate registry packages, or provider code.
`docs/upstream-components.toml` is the authoritative per-component provenance registry.

## Admission state

| State | Source currently present | Build meaning |
| --- | --- | --- |
| Active root workspace | `gfx/qcms`, `modules/libpref/parser`, `third_party/skv` | built and tested by root `cargo test --workspace` |
| Active nested workspace | WebRender Rust core packages in `gfx/wr` | independently locked and tested; Glean/FOG and C++ SWGL features removed |
| Adaptation required | Stylo core; small certificate/client-certificate crates; WebDriver tooling | useful Rust algorithms remain coupled to generated Gecko or Firefox interfaces |
| Pinned source | Neqo, wgpu/Naga, WHATWG URL, mp4parse, authenticator | exact Firefox-selected source; normalized manifests are not an editable canonical workspace |
| Transitional | audioipc/Cubeb Rust layers | usable bootstrap code around native audio libraries, not an all-Rust endpoint |
| Quarantined | selected Application Services local-data source | excluded from root builds until Mozilla Sync, NSS, and provider assumptions are separated |

## Imported component groups

- WebRender and qcms under `gfx/`.
- Stylo's style, selectors, traits, derive, allocation, arc, and shared-memory support crates under
  `servo/components/`; `geckolib` is intentionally absent.
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

The WebRender import retains upstream Wrench, SWGL, examples, shader-to-C++ tooling, and example
compositor files for migration comparison, but those paths are explicitly excluded from its Cargo
workspace. They include first-party C/C++ and a Gecko-dependent Windows example and are not active
Wild Buzzard components. The active renderer manifest cannot enable SWGL. WebRender's third-party
`glslopt` build dependency, FreeType font boundary, test-only `mozangle`/ANGLE validator, and
Windows/Apple operating-system font APIs remain recorded native dependencies.

## Known source-snapshot gaps

The following imports are intentionally not in the root workspace. Their Firefox-normalized Cargo
manifests contain 24 unresolved local paths and must not be presented as buildable yet:

- Stylo: 10 Gecko-generated or Servo-workspace paths (`dom/base/rust`, XPCOM string/allocation
  derives, atoms/config, static preferences, profiler API). Agent 3 must replace these with Wild
  Buzzard contracts or a reviewed canonical Servo workspace.
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
