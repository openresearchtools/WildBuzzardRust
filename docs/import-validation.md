# Initial import validation

Validation date: 2026-08-08
Toolchain: `rustc 1.96.0`, `cargo 1.96.0`

This report records checks for the initial Rust source import. It is not a Firefox-parity claim and
does not admit the adaptation, pinned-source, transitional, or quarantined groups to production.

## Build results

| Scope | Command | Result |
| --- | --- | --- |
| Root active workspace | `cargo test --workspace --locked` | pass; 12 tests, 0 failures |
| qcms full feature set | `cargo test --manifest-path gfx/qcms/Cargo.toml --all-features --locked` | pass; 27 tests, 0 failures |
| WebRender Rust core | `cargo test --manifest-path gfx/wr/Cargo.toml --workspace --all-features --locked` | pass; 143 tests/doc-tests, 0 failures, 5 ignored doc examples |

Build artifacts were directed to directories outside the repository. The checked-in Cargo
configuration now defaults to `../wildbuzzardbuilds/cargo`; concurrent agents use unique
task-specific subdirectories under `../wildbuzzardbuilds/`. No in-tree `target/` directory is
required or permitted.

The WebRender workspace metadata contains exactly these active packages: `peek-poke`,
`peek-poke-derive`, `webrender`, `webrender_api`, `webrender_build`, `wr_glyph_rasterizer`, and
`wr_malloc_size_of`. SWGL, Wrench, examples, shader-to-C++ tooling, and the example compositors are
excluded, absent from the active lock/dependency graph, and cannot be enabled through a renderer
feature. The all-feature test includes the `gecko` compatibility flag but no Glean/FOG hook.
It also exercises the test-only `mozangle` shader-validation boundary. This Linux run does not
constitute support or validation for any non-Linux platform path; those paths are outside product
scope.

## Source-policy results

- Root Cargo metadata contains only qcms, the preference parser, and `skv`; no workspace package or
  path dependency comes from `firefox/` or quarantined Application Services.
- No symlink exists outside `.git/` and the ignored reference checkout.
- No `firefox/` path occurs in a live Cargo manifest, Cargo build script, or Cargo configuration.
- No Glean or FOG reference occurs in an active Rust source, manifest, or lockfile.
- The WebRender all-feature dependency tree contains no SWGL, Wrench, example compositor, Glean,
  FOG, or Firefox package.
- Both root configuration TOML and the provenance registry parse successfully.
- All 14 provenance component groups have existing destination and explicitly registered license
  paths.

At this snapshot the imported source contains 3,628 files, including 1,715 Rust files, and occupies
approximately 108 MiB. These counts exclude the 9.9 GiB ignored Firefox reference checkout.

## Reference checkout

- Outer remote: `https://github.com/openresearchtools/WildBuzzardRust.git`
- Reference revision and `origin/esr153`: `c19b7e89270787889495688244ec6ee8e79288a1`
- Checkout state: detached `HEAD`, non-shallow
- Reachable reference history: 1,055,323 commits
- Ignore rule: root `.gitignore` contains `/firefox/`

## Recorded exemption

`cargo fmt --all -- --check` reports formatting differences in the exact qcms import under the
current toolchain. The source was not bulk-formatted during the import. Perform that mechanical
change separately once the snapshot is accepted; default and all-feature qcms tests already pass.
