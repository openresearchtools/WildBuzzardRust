# W8-A4T Flex container decoration paint

## Decision

W8-A4T closes one renderer integration gap exposed by the W8-A3L normal-desktop Flexbox slice:
`BoxKind::Flex` now participates in the same validated background and border compilation path as
block and inline element boxes. This is a generic box-kind correction, not a site-specific rule.

The accepted write set is:

- `gfx/wild_buzzard_renderer/src/compiler.rs`
- `gfx/wild_buzzard_renderer/tests/scene_compiler.rs`
- `gfx/wild_buzzard_renderer/README.md`
- `docs/handoffs/W8-A4T-flex-decoration-paint.md`

No manifest, lockfile, dependency, unsafe code, native boundary, endpoint, or provider behavior
changes. Firefox ESR153 remains read-only reference material at
`c19b7e89270787889495688244ec6ee8e79288a1`.

## Contract and regression

The renderer already used one private `paints_box_decorations` predicate both when validating exact
scene/WebRender resource counts and when constructing scene items. Adding `BoxKind::Flex` to that
single predicate preserves the preflight/construction agreement: every admitted nontransparent
Flex background becomes one `SceneItem::Background`/WebRender rectangle and every nonzero Flex
border becomes one `SceneItem::Border`/WebRender border. Anonymous blocks, text, and line-break
boxes remain excluded.

The regression builds an ordinary flex container through the live layout contract, gives only that
container a nontransparent background and nonzero border, verifies its layout kind is Flex, and
then requires the exact source-box identity to own one background followed by one border in the
compiled scene. The test does not fabricate a `LayoutOutput` or bypass resource validation.

## Verification

All build output is external under
`/home/user/Documents/wildbuzzardbuilds/w8-a4-flex-paint/`. Exact-file rustfmt and whitespace
validation passed. The locked `x86_64-unknown-linux-gnu` matrix passed two unit tests and 23
renderer integration tests, including the new regression, with zero failures. Strict all-target
Clippy passed with warnings denied; the release build and warning-denied no-dependency rustdoc also
passed.

Frozen source hashes:

```text
95bcbf0d2640dacd2f04c5d864d16ecdaba2161328bc2c3f66f0f7983e0688d6  gfx/wild_buzzard_renderer/src/compiler.rs
ec4b16df01de692c919dc057a3ec8ee76ac33e9dca62cec89d9744f5b08ccc97  gfx/wild_buzzard_renderer/tests/scene_compiler.rs
c2e4df2e522ab6c4798cf9576c4d62636c5dec59d46cf9abe27fe32a717fbe43  gfx/wild_buzzard_renderer/README.md
```

## Remaining gaps

This does not add gradients, images, border styles/colors/radii, shadows, transforms, stacking
contexts, opacity, scrolling, clipping beyond the existing viewport contract, hit testing, or full
CSS painting parity. It proves WebRender display-list admission for Flex decorations, not desktop-
compositor display. Normal sites remain blocked by broader fetch, stylesheet, layout, script,
input, storage, media, and isolation work recorded in `docs/parity/site-compatibility.toml`.
