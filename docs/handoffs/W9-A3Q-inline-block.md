# W9-A3Q bounded inline-block handoff

## Scope and baseline

- Task: W9-A3Q.
- Implementation base for the owned layout/adapter files:
  `4629097275c10544f057d9f03d9e375ac00070af`.
- The orchestrator advanced disjoint integration work while this task was live. The exact frozen
  integration `HEAD` is recorded in the final evidence below.
- Supported product target: Linux x86-64 only.
- All build, generated-style, and review output is isolated below
  `/home/user/Documents/wildbuzzardbuilds/w9-a3q-inline-block/`.
- No source is staged or committed by this gate.

The exact writable source scope is:

- `layout/src/style.rs`
- `layout/src/tree.rs`
- `layout/tests/static_layout.rs`
- `layout/README.md`
- `servo/components/wild_buzzard_stylo_adapter/translate.rs`
- `servo/components/wild_buzzard_stylo_adapter/tests/static_style.rs`
- `servo/components/wild_buzzard_stylo_adapter/README.md`
- `gfx/wild_buzzard_renderer/src/compiler.rs`
- `gfx/wild_buzzard_renderer/tests/scene_compiler.rs`
- `gfx/wild_buzzard_renderer/README.md`
- `docs/handoffs/W9-A3Q-inline-block.md`

## Observable contract

Imported Stylo's exact inline-outside/flow-root-inside computed value is projected to the distinct
`Display::InlineBlock`; it is never substituted with inline, block, or flex. Box construction emits
one `BoxKind::InlineBlock`. That box remains in its parent's anonymous inline run as one atomic,
unbreakable item, while its inside wraps ordinary inline runs and lays supported block descendants
through the real block formatting path. It no longer reaches the historical
`BlockInsideInlineTreatedAsInline` approximation.

The bounded used-size contract admits definite length-percentage width plus min/max and
`box-sizing` constraints. It applies physical margins, padding, borders, background-bearing
fragments, definite or natural height, and visible descendant overflow. Horizontal and vertical
automatic margins on this non-replaced inline-block have zero used value. A private block-margin
mode prevents its left/right automatic margins from entering the ordinary CSS2 block-width
distribution; normal blocks retain their prior distribution unchanged.

CSS2 auto-width inline-blocks require shrink-to-fit from preferred minimum and preferred width.
This formatter does not yet have an honest bounded measurement for both contributions, so
`width:auto` fails as `LayoutError::UnsupportedInlineBlockAutoWidth` before descendant or box
fragments are published. The gate does not fill the available width or substitute an intrinsic
guess.

The inline cursor carries atomic-boundary ancestry independently from collapsed-space state. For a
no-space boundary involving an atomic inline, it compares fallibly retained ancestor paths for the
preceding and current visible items. Those paths exclude the visible text/atom leaves, so the end of
their longest common prefix is the actual nearest common inline ancestor. Its computed
`white-space` value controls the soft opportunity. Thus a normal parent permits atom-to-atom,
text-to-atom, and atom-to-text breaks even when both child spans use `pre`, while a nowrap parent
keeps all three boundaries unbroken even when descendants and the atom override normal. Empty and
whitespace-only nodes cannot replace the preceding visible boundary state; a forced `br` and every
line transition clear it.

A definite inline-block moves as a whole at an eligible collapsed-space or atomic boundary, can
overflow as one item on a fresh line, and never splits. Collapsed-space eligibility remains owned
by the whitespace run and is not overwritten by the atomic rule. A collapsed leading space
advances geometry without creating a painted text fragment. Its advance immediately before an atom
is still measured with the atom style rather than the exact whitespace owner's shaped style, so
complete mixed-font boundary geometry is not claimed.

This slice deliberately keeps the existing line-top placement model. `vertical-align` is not in
the projected layout contract, and the formatter does not compute the last in-flow line baseline or
the bottom margin-edge fallback for an empty/overflow-hidden inline-block. Baseline, bottom, top,
middle, and other vertical alignment parity remain explicit follow-up work.

The renderer's general decoration classifier now includes `BoxKind::InlineBlock`. Every accepted
atom fragment therefore produces its nontransparent solid background and nonzero provisional solid
border in the same safe scene/WebRender path as block, inline, and flex boxes. There is no
hostname, provider, viewport-threshold, selector-stripping, or content-pattern branch.

## Bounds and failure behavior

`LayoutLimits` now includes a default one-million-box admission limit and a default one-million-unit
aggregate inline-work limit. Box admission is checked before allocation; child-ID preallocation is
capped to the remaining box allowance. Inline work charges each visited inline box, every input
text byte, every byte copied/measured by a growing-prefix attempt, every copied or compared
inline-ancestor entry, each inline-fragment aggregation entry, and each comparison against an
already aggregated line. The charge precedes the corresponding work. This bounds the formerly
quadratic long-token probe and nested-inline line search under the same limit. Block and inline
child copies plus inline fragment aggregation use fallible exact reservations and return typed
allocation errors. Existing tree-depth, flex item/line/work, document/revision, writing-mode, and
direction gates remain.

Inline-block percentage, edge, coordinate, child-stack, content-constraint, border-height, and
outer-height arithmetic has a separate checked path and returns `InlineArithmeticOverflow`.
Fragment right/bottom extents, the current atom's remaining-width query and wrap transition, and
the atomic context's later cursor advances/transitions/final absolute bottom are also checked before
publication. A context which never admits an atom retains the pre-existing cursor behavior; the
ordinary CSS2 block path is likewise unchanged. The new limits are present in both resolver- and
immutable-style-snapshot entry points. Every public in-repository `LayoutLimits` literal was
audited; all callers use struct-update syntax and retain the new defaults unless a focused test
overrides one.

## Firefox ESR153 and standards evidence

The ignored read-only checkout was verified at
`c19b7e89270787889495688244ec6ee8e79288a1`. It was inspected only as reference material and is
never a build, test, fixture, or runtime input.

Focused source paths:

- `servo/components/style/values/specified/box.rs`: `Display::InlineBlock` is exactly
  inline-outside plus flow-root-inside.
- `layout/generic/nsIFrame.cpp`: `IsAtomicInline` classifies inline-outside boxes which establish a
  non-inline formatting context as atomic.
- `layout/generic/ReflowInput.cpp` and `layout/generic/nsContainerFrame.cpp`: inline-block selects
  the shrink-wrap auto-size path and uses intrinsic minimum/preferred contributions.
- `layout/generic/nsLineLayout.{h,cpp}` and `layout/generic/nsInlineFrame.{h,cpp}`: line admission,
  atomic placement, inline coordinates, and fragment ownership.
- `layout/base/Baseline.cpp`: atomic baseline synthesis and the alignment behavior deliberately not
  represented in this gate.
- `layout/painting/nsCSSRendering.cpp` and `layout/painting/nsDisplayList.cpp`: fragment background
  and border paint behavior.

Focused tests:

- `testing/web-platform/tests/css/css-sizing/whitespace-and-break.html`
- `testing/web-platform/tests/css/css-text/line-breaking/line-breaking-030.html`
- `testing/web-platform/tests/css/css-text/line-breaking/line-breaking-031.html`
- `testing/web-platform/tests/css/css-text/line-breaking/line-breaking-032.html`
- `testing/web-platform/tests/css/css-text/line-breaking/line-breaking-atomic-007.html`
- `testing/web-platform/tests/css/css-text/line-breaking/line-breaking-atomic-008.html`
- `testing/web-platform/tests/css/css-sizing/fit-content-min-inline-size.html`
- `testing/web-platform/tests/css/CSS2/linebox/vertical-align-baseline-004.xht`
- `testing/web-platform/tests/css/CSS2/linebox/vertical-align-baseline-006a.xht`
- `testing/web-platform/tests/css/CSS2/linebox/vertical-align-122.xht`
- `testing/web-platform/tests/css/CSS2/margin-padding-clear/margin-collapse-014.xht`
- `layout/reftests/inline-borderpadding/ltr-basic.html`
- `layout/reftests/text/white-space-{1a,1b,1-ref}.html`

Full history was inspected rather than only the tip. In particular,
`a38209396aae4d19764ad8083187ef24f74a4443` introduced the focused collapsible-whitespace/
inline-block WPT and `42d52eb2feb88e52b44f1f3d54aa4c841626ea8a` refined alignment-baseline
synthesis to atomic inlines.

The five CSS Text files were read in full. Cases 030, 031, and 032 isolate atom-to-atom,
text-to-atom, and atom-to-text boundaries under a normal common `div` while both child `span`s use
`pre`. Atomic-007 requires opportunities on both sides of the atom. Atomic-008 says
`word-break:keep-all` must not suppress them; `word-break` is not projected in this slice, so that
file supplies the atomic-boundary invariant but is not claimed as a complete WPT pass.

## Deterministic evidence

The layout regression matrix covers:

- distinct `Display` and `BoxKind` identity;
- a fixed-size atom, margins, padding, border, background, and exact border-box geometry;
- a real block descendant with its own geometry rather than the block-in-inline warning path;
- a descendant extending below a definite-height atom (default visible overflow);
- collapsed-space-owned placement, no-space atom-to-atom/text-to-atom/atom-to-text boundaries
  controlled by the nearest common ancestor across both normal/pre and nowrap/normal override
  matrices, and an explicit `br`;
- left/right automatic margins resolving to zero without block centering;
- typed auto-width, horizontal and vertical arithmetic, box-count, inline-work, and text-work
  failures, including exact next-unit stops before a growing-prefix attempt and a nested-inline
  line-aggregation comparison plus an exact positive-y/maximum-line-height atom wrap which fails
  before cursor saturation; and
- the inherited complete block, inline, nowrap, canvas, flex, depth, direction, and sizing matrix.

The imported-Stylo regression enters through author `display:inline-block`, proves the exact typed
projection, fixed box model, block descendant and overflow, and reruns exact geometry at both
1366×768 and 1920×1080. A separate author-CSS case proves left/right `auto` is retained by projection
and has zero used value. A real `width:auto` computed value reaches the typed layout stop.

The renderer regression compiles the atom's exact background and border scene primitives at both
desktop viewports, including source-box identity, rectangle coordinates/dimensions, and all four
border widths. The complete renderer matrix protects paint order, WebRender conversion, canvas
provenance, text, hostile graph/geometry validation, and resource limits.

## Opt-in Google desktop probe

After every deterministic gate was green, one detached clean-source probe was created below the
task artifact root from integration commit `1b0f9a0274b83e9892192c4158cf9708b1121e2a`. Only the ten
owned tracked files listed above were overlaid. Their hashes matched the final owned-source hash
table below. The transient probe changed only the ignored public test, whose clean-base SHA-256 was
`f53f91beb2277b103bc32f07c9b52978b68f75a14288c87d009ccabd673a99cd` and whose probe-overlay
SHA-256 was `d0ea67302cc0f34dbd7ad9fd97159d7a00649ea3b323562c0f8549ba81c7bc16`.

The exact test command was:

```sh
env PYTHON3=/home/user/Documents/wildbuzzardbuilds/w9-a3q-inline-block/python/bin/python \
  CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w9-a3q-inline-block/probe-target \
  CARGO_NET_OFFLINE=true \
  cargo test --locked --offline \
  --manifest-path /home/user/Documents/wildbuzzardbuilds/w9-a3q-inline-block/probe-source/browser/wild_buzzard_engine/Cargo.toml \
  --test general_navigation public_google_reports_the_next_generic_desktop_blocker \
  -- --ignored --exact --nocapture
```

Cargo's offline flag applied only to dependency resolution; the ignored test explicitly opted into
the public top-level fetch. The pipeline remained anonymous/header-minimal and executed no page
script or subresource request. An earlier pre-freeze diagnostic showed that the deterministic
harness's test-only limits of eight DNS candidates/eight connection attempts reject Google's
current address set as `Network(LimitExceeded { kind: DnsCandidates, limit: 8 })`. The renewed
post-review run therefore used the network crate's generic bounded defaults—32 DNS candidates and
16 connection attempts—and produced the relevant post-inline-block result at both required
viewports:

- 1366×768: `Layout(UnsupportedInlineBlockAutoWidth { node_id: Some(... slot: 24) })`.
- 1920×1080: `Layout(UnsupportedInlineBlockAutoWidth { node_id: Some(... slot: 24) })`.

This proves the previous `Display(259)` projection stop advanced through distinct inline-block box
construction to the deliberate generic shrink-to-fit boundary. It does not produce a frame and is
not Google, normal-site, interaction, script, subresource, or pixel parity. The transient source
tree occupied exactly 204,286,430 bytes and its isolated Cargo target occupied exactly
4,070,056,538 bytes. The exact probe target was cleaned (5,990 files, Cargo-reported 4.0 GiB), and
the detached source worktree was then removed immediately after both results and hashes were
recorded. No screenshot, response body, or probe log remains. The retained 3,584-byte
`W9-A3Q-google-probe.patch` has SHA-256
`07cea3c9f795d81f8c93de5e35350c95592b162cc2fdb06d59c060f32b737b6d`.

## Verification

All Cargo commands use `--locked --offline`, `CARGO_NET_OFFLINE=true`, target
`x86_64-unknown-linux-gnu`, and the sole retained deterministic-gate target directory
`/home/user/Documents/wildbuzzardbuilds/w9-a3q-inline-block/target`. The opt-in probe alone used the
disposable `probe-target` recorded above. Stylo generation uses only the task-local Python
environment at
`/home/user/Documents/wildbuzzardbuilds/w9-a3q-inline-block/python`, populated from the exact pins in
`servo/style-build-requirements.txt` (Mako 1.3.10, MarkupSafe 3.0.3, toml 0.10.2).

Frozen integration `HEAD` is `1b0f9a0274b83e9892192c4158cf9708b1121e2a`. The task's owned
implementation baseline remains `4629097275c10544f057d9f03d9e375ac00070af`; disjoint integration
commits did not alter an owned file before this freeze.

Toolchain:

- `rustc 1.96.0 (ac68faa20 2026-05-25)`, host `x86_64-unknown-linux-gnu`, LLVM 22.1.2;
- `cargo 1.96.0 (30a34c682 2026-05-25)`;
- `rustfmt 1.9.0-stable (ac68faa20c 2026-05-25)`;
- `clippy 0.1.96 (ac68faa20c 2026-05-25)`; and
- CPython 3.13.14 with exactly `mako==1.3.10`, `markupsafe==3.0.3`, and `toml==0.10.2`.

The exact formatting command was:

```sh
rustfmt --check --edition 2024 \
  layout/src/style.rs layout/src/tree.rs layout/tests/static_layout.rs \
  servo/components/wild_buzzard_stylo_adapter/translate.rs \
  servo/components/wild_buzzard_stylo_adapter/tests/static_style.rs \
  gfx/wild_buzzard_renderer/src/compiler.rs \
  gfx/wild_buzzard_renderer/tests/scene_compiler.rs
```

For each exact manifest below, the five deterministic Cargo gates were check, strict no-dependency
Clippy, all-target tests, release build, and warning-denied no-dependency rustdoc:

```sh
env CARGO_NET_OFFLINE=true CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w9-a3q-inline-block/target \
  cargo check --locked --offline --target x86_64-unknown-linux-gnu \
  --manifest-path layout/Cargo.toml --all-targets
env CARGO_NET_OFFLINE=true CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w9-a3q-inline-block/target \
  cargo clippy --locked --offline --target x86_64-unknown-linux-gnu \
  --manifest-path layout/Cargo.toml --all-targets --no-deps -- -D warnings
env CARGO_NET_OFFLINE=true CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w9-a3q-inline-block/target \
  cargo test --locked --offline --target x86_64-unknown-linux-gnu \
  --manifest-path layout/Cargo.toml --all-targets
env CARGO_NET_OFFLINE=true CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w9-a3q-inline-block/target \
  cargo build --locked --offline --target x86_64-unknown-linux-gnu --release \
  --manifest-path layout/Cargo.toml
env CARGO_NET_OFFLINE=true RUSTDOCFLAGS='-D warnings' \
  CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w9-a3q-inline-block/target \
  cargo doc --locked --offline --target x86_64-unknown-linux-gnu \
  --manifest-path layout/Cargo.toml --no-deps
```

The same five exact commands were run with manifest
`servo/components/wild_buzzard_stylo_adapter/Cargo.toml` and the additional environment assignment
`PYTHON3=/home/user/Documents/wildbuzzardbuilds/w9-a3q-inline-block/python/bin/python`, then with
manifest `gfx/wild_buzzard_renderer/Cargo.toml` and no Python assignment.

All gates passed. Test counts were:

- layout: 8 unit plus 40 integration tests;
- imported-Stylo adapter: 1 unit plus 41 integration tests; and
- renderer: 2 unit plus 33 integration tests.

The first hostile review exposed nearest-common-ancestor ownership, repeated-prefix work,
fragment-line comparison work, and unchecked vertical/coordinate gaps; all were corrected before
the first full freeze. Independent re-review then found the legacy saturating cursor on an
atom-triggered wrap. The current atom and every post-atom cursor transition/extent became checked,
the positive-y/maximum-line-height regression passed, and the final independent re-review returned
GO with no findings.

Final owned tracked-source SHA-256 values:

| Path | SHA-256 |
| --- | --- |
| `layout/src/style.rs` | `c4f09e165e376d3e061e59ece8c04ff6e66e16e0ac92e75f5ba3c58ab98ce4bd` |
| `layout/src/tree.rs` | `647eb2928db3f4e8361a823c59a8066924afd64e99d396b61313bc87401e0ec0` |
| `layout/tests/static_layout.rs` | `07fee7200b5fd6ade5190565be23418b7302a689aaf6e7a0b673f4f91b2fa896` |
| `layout/README.md` | `186adcb1aa086ab02e9ab58a4d5e77b5034e7f6be944234a2c908958aac678f0` |
| `servo/components/wild_buzzard_stylo_adapter/translate.rs` | `daede36562d308a86faf4518c365a71f656851a16fac6be86ed52c6ea9a7e31b` |
| `servo/components/wild_buzzard_stylo_adapter/tests/static_style.rs` | `3af81ac222027fbc45c98cdb2654db3f763d18e994f1aa209a65819823120fbe` |
| `servo/components/wild_buzzard_stylo_adapter/README.md` | `4886c0aa2590fc1460866aca611a296f6dc4f75fa0bec99539303ff39ff71c68` |
| `gfx/wild_buzzard_renderer/src/compiler.rs` | `c5e8c8940fc4b96f218c6150d06f128af8ffc438f17d96efb8233809aaea75d8` |
| `gfx/wild_buzzard_renderer/tests/scene_compiler.rs` | `ac6bdb7ed92b75a489e7b71aeec4217e898db7789f90bdb34c3daec18b237814` |
| `gfx/wild_buzzard_renderer/README.md` | `0005d29c83e574b7c2913af012cfdebe237534f049a906dd2eb4543291affb62` |

The retained external artifact inventory is intentionally compact:

- `target/`: 4,499,080,574 bytes, containing only canonical live-source deterministic-gate output;
- `python/`: 440,005 bytes, with the three exact packages above;
- `W9-A3Q-google-probe.patch`: 3,584 bytes, SHA-256
  `07cea3c9f795d81f8c93de5e35350c95592b162cc2fdb06d59c060f32b737b6d`; and
- `W9-A3Q-tracked.patch`: 111,114 bytes, SHA-256
  `cc26cc394017082d6602a55f81ecf2a2985047b25d4b1b676bbcb37b264040ef`.

The handoff-only patch is generated after this document is frozen; its exact size and SHA-256 are
reported in the external review message to avoid a self-referential hash.

An earlier shared-target probe duplicated path-keyed objects. Before the final canonical rebuild,
the exact task target measured 9,269,167,593 bytes and was cleaned (16,948 files, Cargo-reported
9.9 GiB). The renewed final probe used its own disposable target, as recorded above. No transient
worktree, probe target, screenshot, response body, log, AppDir, AppImage, or other generated path is
retained.

## Explicit limitations and next gates

- Auto-width shrink-to-fit remains a typed stop until bounded preferred-minimum/preferred-width
  measurement exists. Intrinsic sizing keywords remain outside the adapter's admitted projection.
- Inline-block baseline synthesis and every `vertical-align` value are absent; the current atom is
  placed from the line top.
- There is no general Unicode line breaking, bidi, shaping, hyphenation, floats, clearance, margin
  collapse, positioned layout, overflow clipping/scrolling, fragmentation, transforms, stacking,
  replaced-element intrinsic sizing, or form-control sizing.
- The inside uses the existing bounded block/flex/inline subset; it is not a complete flow-root or
  block-formatting-context implementation.
- Background paint is solid color and borders are provisional solid `currentColor`; images,
  gradients, per-side styles/colors, radii, shadows, opacity, and stacking contexts remain open.
- Desktop geometry and scene compilation are deterministic evidence, not pixel/reftest or public
  site parity.
