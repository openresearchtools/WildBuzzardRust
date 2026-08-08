# Wild Buzzard

Wild Buzzard is a long-term project to build a privacy-respecting, general-purpose web browser in Rust with observable Firefox ESR engine and user-interface parity.

The project reuses suitable Rust components already present in Firefox and ports the remaining browser behavior around explicit Rust interfaces. It does not aim to reproduce Firefox branding, Mozilla-operated services, telemetry, sponsored content, or provider-specific defaults.

This repository is currently at the source-import and architecture-foundation stage. It is not yet a runnable browser and makes no parity claim.

## Source layout

The live tree preserves Firefox-relative subsystem paths where that makes comparison and history research easier:

- `gfx/wr`: WebRender workspace.
- `gfx/qcms`: Rust color management.
- `servo/components`: reusable Stylo CSS crates.
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
exact source provenance. The commands and results from the initial source audit are recorded in
`docs/import-validation.md`.

## Initial checks

The root workspace intentionally includes only independently usable crates:

```sh
cargo test --workspace
cargo metadata --manifest-path gfx/wr/Cargo.toml --no-deps --locked --format-version 1
```

Cargo is configured by `.cargo/config.toml` to place generated artifacts in the sibling
`../wildbuzzardbuilds/cargo` directory, not in this repository. Concurrent agents should override
`CARGO_TARGET_DIR` with a unique directory such as `../wildbuzzardbuilds/agent-4-webrender`.

Stylo, Neqo, wgpu, media, and application-services imports remain outside the root workspace until their Firefox/Gecko assumptions or normalized vendor manifests have been replaced with Wild Buzzard-owned contracts.

## Reference source

The local `firefox/` checkout is pinned to Firefox ESR153 commit `c19b7e89270787889495688244ec6ee8e79288a1` and retains full Git history. Wild Buzzard must always build and test without that directory present.

## Licensing

Wild Buzzard is licensed under MPL-2.0. Imported components retain their original copyright and license notices. Code under `third_party/` may use additional compatible licenses recorded by the component itself and in the provenance registry.
