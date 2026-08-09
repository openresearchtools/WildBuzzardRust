# Wild Buzzard text pipeline

This directory is a Rust text-selection and shaping boundary; it is not a new
font parser or shaping engine. It adopts mature Rust components and exposes an
immutable result that layout, hit testing, and painting can share:

```text
TextRequest
  -> Fontique selection (Linux system fonts or deterministic embedded fallback)
  -> Parley Unicode analysis, bidi, segmentation, and run construction
  -> HarfRust OpenType shaping
  -> Arc<ShapedText> with exact blobs, face indices, glyph IDs, clusters, and metrics
  -> wild_buzzard_text_webrender (registration and glyph emission only)
  -> imported WebRender
```

The WebRender adapter deliberately cannot consume `PendingTextRun`, select a
font, or reshape a string. `HeadlessRenderer::render_shaped_text` is an isolated
proof of the typed seam; the existing production scene compiler continues to
report pending text unchanged until layout hands it the exact `Arc<ShapedText>`
used for measurement.

## Adopted Rust components

The root `Cargo.lock` is authoritative for the complete dependency closure.
The primary adopted revisions for this slice are:

| Component | Version / source revision | License | Purpose |
| --- | --- | --- | --- |
| Parley and Fontique | `0.11.0`, Linebender commit `033e0b000d3dd1c1bcd0097da5a4d60ded8d4937` | Apache-2.0 OR MIT | text layout, bidi/run analysis, font matching |
| HarfRust | `0.10.0`, commit `9d68be1c53e51032b7170a5c5028ce17c8ccef66` | MIT | OpenType shaping |
| Skrifa | `0.43.2`, Fontations commit `25c28a42992381173d556fdac400bc499e6a597f` | MIT OR Apache-2.0 | font metadata and variation support |
| read-fonts | `0.40.2`, Fontations commit `602c8abb1cb8acd8bcfbf0b0d8fe8bdf46493d96` | MIT OR Apache-2.0 | checked font-table reading |
| linebender_resource_handle | `0.1.1`, commit `bd8694ffca7550e8d569346c50de5fdcb5c51a7f` | Apache-2.0 OR MIT | shared blob identity without shaping-time byte copies |
| Parlance | `0.1.0`, commit `8dbecc0545a0c97eb605937b928bc186d2d1295c` | Apache-2.0 OR MIT | language metadata |
| ICU4X crates | `2.2.0` | Unicode-3.0 | Unicode properties, normalization, and segmentation |
| yeslogic-fontconfig-sys | `6.0.1`, commit `be4b2836d6d22db9322dfb485449323528437a72` | MIT | optional Linux Fontconfig discovery through `dlopen` |
| memmap2 | `0.9.11`, commit `7d76ad3157383db5670fd7e012f44de42aa7444b` | MIT OR Apache-2.0 | mapped system-font access |

The deterministic fallback is an exact copy of Fira Code from the pinned
imported WebRender snapshot. Both the active font and its SIL Open Font License
notice live in this crate's `res/` directory so this component has no build-time
dependency on the excluded Wrshell example tree.

- Font SHA-256: `5dc1651a1143c53169d4394dfc55585860d13c158cc0bcc2e56c23a1da5dd777`
- Notice SHA-256: `ac27f7c95c76a310940411220e71dd3d0317e1de38980992d94524fcea1fdae0`

`docs/upstream-components.toml` still needs the orchestrator-owned provenance
entries for this adoption before release acceptance.

## Firefox reference research

The ignored ESR153 checkout was inspected at pinned commit
`c19b7e89270787889495688244ec6ee8e79288a1`. The principal behavior and test
references were `gfx/thebes/gfxFont.{h,cpp}`, `gfxTextRun.{h,cpp}`,
`gfxHarfBuzzShaper.{h,cpp}`, `gfxPlatformFontList.{h,cpp}`, the Linux
Fontconfig/FreeType platform implementations, `gfx/harfbuzz/`, and WebRender's
font resource/display-list APIs. Full history was used, including the ESR fixes
`4a771f8bef49`, `090b83d4272e`, `eb97943bc449`, and `ad708e5ef09e` that bound or
clear malformed/partial glyph data. Those invariants motivated independent
run/glyph/cluster limits and all-or-error extraction; the C++ Gecko shaper and
vendored C HarfBuzz were not copied.

Current Servo font code was evaluated as a source of architecture and behavior,
but its Linux path still uses C HarfBuzz, FreeType, and Fontconfig. It was not
imported wholesale into this Rust-first boundary. This decision is unrelated to
Stylo: the existing Rust Stylo crates are separately imported and adapted for
CSS, not rewritten here.

## Native and unsafe audit

Both Wild Buzzard crates in this slice forbid first-party `unsafe`. Parley has
no unsafe implementation in the selected source, and HarfRust forbids unsafe.
The Linux system-font mode does have narrow third-party native boundaries:

- Fontique calls Fontconfig through `yeslogic-fontconfig-sys` and dynamically
  loaded symbols. It maps host font files through `memmap2`; those crates contain
  the audited unsafe FFI/mapping code.
- The embedded-only deterministic constructor performs no system-font lookup,
  and absence of Fontconfig leaves the embedded fallback available.
- The admitted WebRender headless renderer uses FreeType. Its current
  `static_freetype` build path can prefer a host `pkg-config` FreeType before a
  bundled build, so a self-contained AppImage dependency audit is still open.

The Linux target dependency tree activates no Windows or Apple font adapter.
There are no service endpoints, telemetry calls, or runtime network requests in
this pipeline.

## Bounds and lifecycle

Requests bound text, family and language bytes, settings, coordinates, runs,
clusters, glyphs, faces, font bytes, and cache retention before or during
shaping. The cache key compares every request field exactly. Cache accounting
charges owned key strings/vectors, the duplicate result string, every result
box, and complete unique font blobs that cached handles may keep alive.

The WebRender adapter independently revalidates shaped output. It compares both
font identity and complete bytes, copies bounded raw bytes only because the
WebRender API requires `Vec<u8>`, and deduplicates font instances by face, size,
normalized coordinates, and synthesis. Keys are scoped to one verified
`RenderApi` namespace. A prepared frame exclusively borrows the registry;
additions precede display-list use in the same transaction and become live
registry entries only after that transaction is accepted. Explicit shutdown
deletes instances before their fonts. No in-flight key is evicted; fixed
registry limits fail closed.

## Current explicit gaps

- Only one unwrapped line is accepted.
- Parley `0.11.0` does not expose a forced paragraph-direction control through
  the adopted builder, so explicit LTR and RTL requests return
  `UnsupportedDirection`; they never silently fall back to `Auto`.
- The API does not yet expose downloadable web-font registration or CSS font
  loading lifecycle behavior.
- The WebRender adapter rejects non-empty normalized variation coordinates
  until `ShapedText` also carries the axis tags and user-space values required
  by WebRender. Static Fira Code is supported by the proved slice.
- Latin glyphs, combining clusters, Unicode bidi controls, and the documented
  Fira Code `->` contextual form are tested. Complex scripts, emoji fallback,
  vertical text, line breaking, justification, decorations, selection, and
  exhaustive font fallback remain parity work, regardless of upstream library
  capability.
- The exact layout-owned `Arc<ShapedText>` integration is not complete;
  production `PendingTextRun` remains explicit and unpainted.

## Reproducible gates

All artifacts must remain outside the repository:

```sh
CARGO_TARGET_DIR=../wildbuzzardbuilds/agent-4-text-core \
  cargo test -p wild_buzzard_text --locked --target x86_64-unknown-linux-gnu
CARGO_TARGET_DIR=../wildbuzzardbuilds/agent-4-text-core \
  cargo clippy -p wild_buzzard_text --locked --all-targets --all-features \
  --target x86_64-unknown-linux-gnu -- -D warnings
CARGO_TARGET_DIR=../wildbuzzardbuilds/agent-4-text-headless \
  cargo test -p wild_buzzard_headless --locked --target x86_64-unknown-linux-gnu
CARGO_TARGET_DIR=../wildbuzzardbuilds/agent-4-text-headless \
  cargo clippy -p wild_buzzard_headless --locked --all-targets \
  --target x86_64-unknown-linux-gnu -- -D warnings
```
