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
a Linux headless WebRender path that produces deterministic pixels for the supported
background/border and isolated shaped-text slices. These components are not yet connected into a
runnable end-to-end page load, and no parity claim is made.

## Source layout

The live tree preserves Firefox-relative subsystem paths where that makes comparison and history research easier:

- `gfx/wr`: WebRender workspace.
- `gfx/qcms`: Rust color management.
- `gfx/wild_buzzard_renderer`: first-party validated immutable scene and WebRender display-list
  boundary; text shaping remains pending.
- `gfx/wild_buzzard_headless`: first-party Linux x86_64 EGL/WebRender frame owner and bounded RGBA8
  readback; it produces real pixels but is not yet a window compositor or complete paint pipeline.
- `js`: first-party Rust JavaScript/WebAssembly runtime program, currently an interpreter with a
  tracing heap, exact UTF-16 strings, string/Symbol property keys with ECMAScript own-key ordering,
  and a rooted embedding nucleus rather than a complete engine.
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
```

Cargo is configured by `.cargo/config.toml` to place generated artifacts in the sibling
`../wildbuzzardbuilds/cargo` directory, not in this repository. Concurrent agents should override
`CARGO_TARGET_DIR` with a unique directory such as `../wildbuzzardbuilds/agent-4-webrender`.

Stylo has an independently locked nested workspace because its generated-property and prohibited
feature gates are distinct from the root workspace; its immutable adapter shares root DOM/layout
crates directly. Neqo, wgpu, media, and application-services imports remain outside the root
workspace until their Firefox/Gecko assumptions or normalized vendor manifests have been replaced
with Wild Buzzard-owned contracts.

## Reference source

The local `firefox/` checkout is pinned to Firefox ESR153 commit `c19b7e89270787889495688244ec6ee8e79288a1` and retains full Git history. Wild Buzzard must always build and test without that directory present.

## Licensing

Wild Buzzard is licensed under MPL-2.0. Imported components retain their original copyright and license notices. Code under `third_party/` may use additional compatible licenses recorded by the component itself and in the provenance registry.
