# Wild Buzzard

Wild Buzzard is a long-term project to build a privacy-respecting, general-purpose web browser in Rust with observable Firefox ESR engine and user-interface parity.

The sole product target is Linux x86_64 (`x86_64-unknown-linux-gnu`), distributed as an AppImage.
Windows, macOS, Android, iOS, and other architectures are outside the implementation and parity
scope.

The project reuses suitable Rust components already present in Firefox and ports the remaining browser behavior around explicit Rust interfaces. It does not aim to reproduce Firefox branding, Mozilla-operated services, telemetry, sponsored content, or provider-specific defaults.

This repository is at the implementation-foundation stage. The root workspace contains tested
Rust-native process/runtime contracts, a growing JavaScript interpreter, DOM ownership,
incremental HTML parsing, an immutable-DOM-to-Stylo-to-static-layout path, a bounded
numeric-loopback HTTP transport, a validated layout-to-WebRender built-display-list boundary, and
a Linux headless WebRender path that can compose deterministic page decorations and every admitted
shaped-text run in one transaction. The independently locked `browser/wild_buzzard_engine` crate
connects the static pieces in one bounded synchronous numeric-loopback URL-to-RGBA8 proof and now
offers a bounded generation-aware worker/event facade with atomic stale-frame suppression. The
synchronous pipeline now publishes one composed zero-pending page-and-text frame through that
facade. There is no browser window, browser-connected page script execution, or general networking.
It is therefore an integration proof, not a runnable browser or parity claim.

## Source layout

The live tree preserves Firefox-relative subsystem paths where that makes comparison and history research easier:

- `gfx/wr`: WebRender workspace.
- `gfx/qcms`: Rust color management.
- `gfx/wild_buzzard_renderer`: first-party validated immutable scene and WebRender display-list
  boundary, including exact checked replacement of every pending text paint slot.
- `gfx/wild_buzzard_headless`: first-party Linux x86_64 EGL/WebRender frame owner and bounded RGBA8
  readback; its composed path submits fonts, decorations, all positioned glyphs, and frame
  generation once, but it is not yet a window compositor or complete paint pipeline.
- `browser/wild_buzzard_engine`: independently locked first-party integration seam for bounded
  loopback HTTP, UTF-8 HTML, immutable DOM, imported Stylo, static layout, Rust text shaping, and
  real EGL/WebRender readback, plus a bounded typed navigation/event worker. It retains the exact
  canonical finalized shapes, submits one composed zero-pending frame, and publishes that exact
  frame through a generation-tagged lease.
- `js/brimstone`: the canonical pinned JavaScript engine adaptation. Its off-by-default W2-A2L
  proof roots an exact function/artifact across moving GC and resumes a deliberately narrow
  `Neg`/`Ret` or uncaught-`Throw` tail in an actual Brimstone VM frame. Product dispatch remains
  compile-time false; this is not a browser JIT and is prohibited for DOM or untrusted-page use.
- `js/wasmtime`: the immutable pinned Wasmtime v47.0.3/Cranelift source baseline.
- `js/wasm`: the independently locked, capability-free first-party Wasmtime adapter. It accepts
  bounded binary, import-free modules and exposes an `i32`-only call proof with explicit logical
  limits and interruption. It is product-disconnected and is not the JavaScript `WebAssembly` API.
- `js`: also retains the transitional `wild_buzzard_js` interpreter as host-contract and regression
  migration evidence; it must not become a second live page heap.
- `dom`, `parser`, and `layout`: first-party DOM, incremental HTML, and static-layout nuclei.
- `netwerk/rust/wild_buzzard_net`: bounded fail-closed HTTP/1.1 transport currently restricted to
  numeric loopback targets.
- `memory/rust`, `mozglue/rust`, `ipc/rust`, `widget/rust`, and `xpcom/rust`: Rust-native handles,
  runtime, typed IPC, platform-neutral contracts, and temporary service abstractions.
- `servo/components`: independently locked Stylo CSS-engine workspace, with the imported selector,
  generated-property, cascade, and computed-value core active behind Wild Buzzard Rust platform
  shims and a concrete immutable DOM/computed-style adapter feeding the static layout contract.
- `third_party/rust/neqo-*`: Firefox-pinned Neqo/HTTP3 source snapshot.
- `third_party/rust/wgpu-*` and `third_party/rust/naga`: Firefox-pinned WebGPU implementation snapshot.
- `third_party/rust/url` and related crates: Firefox-pinned WHATWG URL implementation.
- `third_party/rust/mp4parse*`: Rust media container parser.
- `third_party/rust/audioipc2*` and `third_party/rust/cubeb*`: transitional Rust audio layers.
- `third_party/application-services`: selected provider-neutral browser-data components, quarantined until Mozilla Sync dependencies are separated.
- `testing/geckodriver`, `testing/webdriver`, and `testing/mozbase/rust`: browser automation infrastructure.
- `firefox`: ignored, read-only Firefox ESR153 reference checkout; never a build dependency.

See `AGENTS.md` for operating rules, `docs/component-map.md` for path ownership,
`docs/import-status.md` for build-readiness boundaries, and `docs/upstream-components.toml` for
exact source provenance. Stable cross-process identity assignments live in
`docs/wire-registry.toml`. The commands and results from the initial source audit are recorded in
`docs/import-validation.md`. The ownership and acceptance contract for the first static-page
pipeline is `docs/architecture/static-page-slice.md`.

The live milestone/task ledger is `docs/program-status.toml`. It deliberately keeps runnable-browser,
normal-site, YouTube, engine-parity, and UI-parity claims false until end-to-end evidence exists.

## Initial checks

The root workspace intentionally includes only independently usable crates:

```sh
cargo test --workspace --locked
cargo metadata --manifest-path gfx/wr/Cargo.toml --no-deps --locked --format-version 1
cargo metadata --manifest-path servo/Cargo.toml --no-deps --locked --format-version 1
CARGO_TARGET_DIR=../wildbuzzardbuilds/readme-wasm \
  cargo test --manifest-path js/wasm/Cargo.toml --workspace --locked \
  --target x86_64-unknown-linux-gnu
```

Cargo is configured by `.cargo/config.toml` to place generated artifacts in the sibling
`../wildbuzzardbuilds/cargo` directory, not in this repository. Concurrent agents should override
`CARGO_TARGET_DIR` with a unique directory such as `../wildbuzzardbuilds/agent-4-webrender`.

Stylo has an independently locked nested workspace because its generated-property and prohibited
feature gates are distinct from the root workspace; its immutable adapter shares root DOM/layout
crates directly. The browser integration seam is also independently locked so its Mako-backed
Stylo build requirements remain explicit rather than becoming an implicit requirement of every
root-workspace command. Its exact external-build commands are in
`browser/wild_buzzard_engine/README.md`. Neqo, wgpu, media, and application-services imports remain
outside the root workspace until their Firefox/Gecko assumptions or normalized vendor manifests
have been replaced with Wild Buzzard-owned contracts.

The first-party `js/wasm` adapter is likewise not a root-workspace member. Its exact capability,
proposal, resource, and build boundaries are documented in `js/wasm/README.md`; all generated
artifacts must remain under `../wildbuzzardbuilds/`.

## Reference source

The local `firefox/` checkout is pinned to Firefox ESR153 commit `c19b7e89270787889495688244ec6ee8e79288a1` and retains full Git history. Wild Buzzard must always build and test without that directory present.

## Licensing

Wild Buzzard is licensed under MPL-2.0. Imported components retain their original copyright and license notices. Code under `third_party/` may use additional compatible licenses recorded by the component itself and in the provenance registry.
