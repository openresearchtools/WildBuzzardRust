# W9-A3O typed collapsed-nowrap handoff

## Scope and baseline

- Task: W9-A3O.
- Exact live base: `06b227bccfce04286ed489c81b7dd12afd114b43`.
- All build and review output is under
  `/home/user/Documents/wildbuzzardbuilds/w9-a3o-nowrap/`.
- No source is staged or committed by this gate.

## Observable contract

Stylo's exact computed `white-space-collapse: collapse` plus `text-wrap-mode: nowrap` pair maps to
the distinct layout value `WhiteSpace::Nowrap`. It uses the same ASCII CSS whitespace collapse
state as `WhiteSpace::Normal`, including a pending collapsed space across separately boxed text and
nested inline descendants, but does not split a nowrap word or take an internal soft line break.
Content may therefore overflow its inline container. An explicit `br` still ends the line.
`Preserve/Nowrap` continues to map to `WhiteSpace::Pre`; collapsed-nowrap is never approximated as
preserved whitespace.

The pending collapsed space is a private typed state with `None`, `Unbreakable`, and `SoftBreak`
variants. Normal-owned whitespace retains its one break opportunity even when the following word
is nowrap; taking that boundary break does not make the nowrap word itself wrappable.

The adapter regression enters through the public `white-space: nowrap` shorthand that exposed the
product blocker. Explicit-longhand elements separately prove `Collapse/Wrap` remains `Normal` and
`Preserve/Nowrap` remains `Pre`. There is no hostname, provider, viewport-threshold, or content-
pattern branch.

## Firefox evidence

The read-only ESR153 checkout at `c19b7e89270787889495688244ec6ee8e79288a1` was inspected.
`nsTextFrame.cpp` selects whitespace compression from `mWhiteSpaceCollapse`, independently of
`nsLineLayout.cpp` deriving its nowrap span state from `WhiteSpaceCanWrap`. The focused WPT inputs
were:

- `testing/web-platform/tests/css/css-text/white-space/white-space-nowrap-011.html`
- `testing/web-platform/tests/css/css-text/white-space/text-wrap-nowrap-001.html`
- `testing/web-platform/tests/css/css-text/white-space/white-space-wrap-after-nowrap-001.html`
- `layout/reftests/text/white-space-1a.html`
- `layout/reftests/text/white-space-1b.html`
- `layout/reftests/text/white-space-1-ref.html`

The checkout is reference material only, never a build, test, or runtime input.

## Changed paths

- `layout/src/style.rs`
- `layout/src/tree.rs`
- `layout/tests/static_layout.rs`
- `layout/README.md`
- `servo/components/wild_buzzard_stylo_adapter/translate.rs`
- `servo/components/wild_buzzard_stylo_adapter/tests/static_style.rs`
- `servo/components/wild_buzzard_stylo_adapter/README.md`
- `docs/handoffs/W9-A3O-collapsed-nowrap.md`

## Deterministic evidence

The layout suite proves repeated SPACE/TAB/LF input collapses across a nested `span`, all soft text
stays on one overflowing line in a narrow containing block, and `br` starts the following line.
WPT-derived mixed-mode tests cover an ASCII collapsed space after a nowrap span and the two nested
Normal/Nowrap trailing-space topologies from `white-space-wrap-after-nowrap-001.html`.

ESR `white-space-1a.html` and its extra-span-boundary `white-space-1b.html` supply the inverse
boundary: `Hello<span class=nowrap> </span>Kitty` keeps the nowrap-owned collapsed space
unbreakable even though `Kitty` is Normal. The regression exercises both the exact zero-width form
and a five-character deterministic width; because this early formatter retains its pre-existing
overlong-word character fallback, the zero-width assertion isolates the final glued boundary rather
than claiming full reftest fragment parity.

The adapter suite proves all three admitted computed-value mappings and exact box/fragment geometry
at 1366x768 and 1920x1080. The complete matrices retain the existing Normal, Pre, canvas, flex,
recursion, arithmetic, and work-limit evidence.

## Verification

All Cargo commands used `--locked --offline`, `CARGO_NET_OFFLINE=true`, and the sole target
directory `/home/user/Documents/wildbuzzardbuilds/w9-a3o-nowrap/target`. The adapter used the
minimal task-local generator environment at
`/home/user/Documents/wildbuzzardbuilds/w9-a3o-nowrap/python`, containing only Mako 1.3.10,
MarkupSafe 3.0.3, and toml 0.10.2 from `servo/style-build-requirements.txt`.

Toolchain: rustc/cargo 1.96.0 (`ac68faa20`/`30a34c682`), rustfmt 1.9.0-stable, and Clippy 0.1.96.

Final results:

- package-scoped rustfmt checks passed for layout and the standalone adapter;
- both all-target `cargo check` gates passed;
- layout all-target Clippy and adapter all-target no-dependency Clippy passed with `-D warnings`;
- layout passed 8 unit and 31 integration tests;
- the adapter passed 1 unit and 38 integration tests;
- both release builds passed;
- warning-denied, no-dependency rustdoc passed for both crates; and
- `git diff --check` passed without staging or committing any source.

The retained review artifacts are:

- `/home/user/Documents/wildbuzzardbuilds/w9-a3o-nowrap/target/`: 4,001,570,680 bytes.
- `/home/user/Documents/wildbuzzardbuilds/w9-a3o-nowrap/python/`: 11,988,077 bytes.
- `/home/user/Documents/wildbuzzardbuilds/w9-a3o-nowrap/W9-A3O-tracked.patch`: 26,700 bytes;
  SHA-256 `aa51cbf4ae6e0a158d4dfa0780a3a22ef22b2d8e68519c69114020b8b22a21fa`.
- `/home/user/Documents/wildbuzzardbuilds/w9-a3o-nowrap/W9-A3O-handoff.patch`: 7,134 bytes;
  SHA-256 `17d07f5c3e78b39435ae981bd9a2883844fe6d6a61cccbf8f792135af1dc7624`.

The whole `/home/user/Documents/wildbuzzardbuilds/w9-a3o-nowrap/` task directory is
4,013,592,591 bytes.

## Frozen source hashes

SHA-256 after the final gates (this self-referential handoff is excluded):

| Path | SHA-256 |
| --- | --- |
| `layout/src/style.rs` | `52edfca3449bfefb4925611454406441835f9ff0f76bab6d75a950e51b1117e7` |
| `layout/src/tree.rs` | `5996f518e44dc16e6a7cf9aeb769683f4a6df465d879f32c6ef02417a0843ead` |
| `layout/tests/static_layout.rs` | `84229549edcc014dee3ef66884cca001d39af9d35c1d7f6735bdcd1dab3e9764` |
| `layout/README.md` | `7eac590daf3a8a875f6c635a00fef055ea904267add4c1cf57754bbf2f09918f` |
| `servo/components/wild_buzzard_stylo_adapter/translate.rs` | `189667e223ccb659c119885a7382b43897dbac9c04ca54dbc4ba303ce51d5eb5` |
| `servo/components/wild_buzzard_stylo_adapter/tests/static_style.rs` | `f5bee6cea25c3070e962f2fe251b2ff4cc76bcea40d29d94dda6c75cee771cf8` |
| `servo/components/wild_buzzard_stylo_adapter/README.md` | `76760fcbced61b5092898e6bd3b25e5e9388dddf792ef67f094a1d9944cd420b` |

## Limitations and later acceptance

This is the existing deterministic monospace, horizontal LTR inline formatter. It does not add
Unicode line breaking, shaping, bidi, hyphenation, intrinsic sizing, or complete mixed-style
line-break opportunity parity. In particular, the ideographic adjacency cases in
`white-space-wrap-after-nowrap-001.html` remain unsupported because this formatter has no Unicode
line-break analysis. The ASCII regressions are not a claim that the whole WPT passes.

This gate does not claim product rendering or public-site parity. After integration and independent
review, the orchestrator must prove the generic blocker advances at the required desktop viewports
and record the next generic limitation.
