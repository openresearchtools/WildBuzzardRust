# W8-A3L bounded normal-page flex layout handoff

- Task: W8-A3L — add the first bounded CSS Flexbox formatting context needed by generic normal-page desktop structure
- Owner: Agent 3 — DOM, style, and layout
- Status: corrected and accepted after an independent hostile-review NO-GO and independent
  correction rereview GO. The bounded renderer integration is recorded separately in W8-A4T
- Product target: Linux x86-64 only; desktop fixtures cover 1366×768 and 1920×1080
- Firefox baseline: ESR153 commit `c19b7e89270787889495688244ec6ee8e79288a1`

## Accepted boundary

The existing immutable `DocumentSnapshot` and exact-document/revision `ComputedStyleSnapshot`
remain the only production inputs. Imported Stylo computes CSS; the adapter projects supported
computed values without reparsing author text; layout consumes the smaller Rust value model.

The admitted formatting context is horizontal-writing-mode `display:flex` with:

- row and column main axes;
- nowrap and forward wrap;
- `flex-basis: auto`, `content`, and representable length-percentage values;
- nonnegative grow and scaled-shrink factors;
- explicit min/max clamps with iterative refreezing;
- start/end/center/space-between/space-around/space-evenly main-axis packing;
- stretch/start/end/center `align-items`, plus auto/stretch/start/end/center `align-self`;
- row and column gaps; and
- visual `order` with stable source-order tie breaking.

Stylo values outside that set fail as typed `UnsupportedComputedValue` variants when the value can
affect this formatting context. This includes inline flex; reverse axes, wrap-reverse, baseline and
safety alignment forms, non-default `align-content`, and nonlinear gaps on a flex container; and
unsupported intrinsic bases, nonlinear bases, or flex factors that cannot fit the fixed-point
layout representation on possible flex items. Irrelevant container-only flex values do not reject
an ordinary non-flex box. Automatic margins retain the pre-existing typed rejection. There is no
fallback CSS parser and no nearest-value substitution.

## Box construction and ordering

Direct element children become flex items and are blockified in the layout box tree. Contiguous
direct text/line-break children become one anonymous flex item; a run containing only collapsible
ASCII whitespace is suppressed. Layout preserves the flex container's box children in DOM order.
The planner sorts only an internal index vector by `(order, source_index)`, so visual placement does
not relabel DOM or future accessibility order.

The adapter continues to publish the original document and revision identities. The desktop
fixture checks the revision on each layout result; flex support introduces no live-DOM backdoor or
mutable style state.

## Hostile-review NO-GO and correction

The first W8-A3L freeze was rejected in hostile review. The review identified three concrete
defects; its NO-GO supersedes the original freeze and must remain part of the record:

1. A row item's automatic cross size was estimated against the container width before flexing, so
   final narrow item widths could wrap content into more lines than the item and container heights
   recorded.
2. An empty flex container reserved zero line entries and then used infallible `Vec::push` to create
   its specification-required empty line.
3. Adapter documentation omitted supported `display:flex` and incorrectly classified
   `display:inline-flex` as a flex-value error rather than a display-value error.

The correction keeps the existing boundary. A first fully budgeted plan resolves final item main
sizes. For row items with automatic cross size, layout charges the complete remeasurement pass,
remeasures content at each exact post-flex content width, and—if any item was remeasured—invokes the
ordinary fully budgeted planner a second time before any fragments are emitted. The intrinsic walk
continues charging its per-node and child-copy work. This mirrors the ordering inspected in
Firefox `nsFlexContainerFrame.cpp` lines 5351–5384, where flexible lengths resolve before items
whose main size can influence cross size are sized again in the cross axis.

The exact regression uses a 100px row container with `align-items:flex-start`, an item with
`flex:0 0 20px`, 10px font size and line height under the 5px-per-ASCII-glyph test measurer, and
`abcdefghij`. It proves three fragments (`abcd`, `efgh`, `ij`) and exact 30px item and container
heights. Empty containers now fallibly reserve one line whenever `max_lines > 0`; a focused unit
test covers the zero/one/many capacity cases. Adapter docs and a regression now record that inline
flex fails as `UnsupportedComputedValue::Display`.

The independent correction rereview returned GO with no blocking new finding. It reran seven
planner unit tests, two focused Flex layout integrations, and six adapter Flex tests offline. It
verified the exact 20px/three-line/30px regression, shared work-budget charging, no fragment
publication before both plans succeed, fallible empty-line reservation, corrected error
classification, and all frozen hashes. Its sole nonblocking wording nit was corrected before
integration without changing runtime behavior.

After that GO, the orchestrator corrected the comment wording and normalized imports in two
adapter files to the repository's stable rustfmt output. Exact-file rustfmt and whitespace checks,
all 7+17 layout tests, and the exact Stylo wrapped-text regression passed again; the frozen hashes
below describe that final formatting-only integration state.

## Planner and geometry

`layout/src/flex.rs` is a layout-private, deterministic planner. It constructs forward lines from
hypothetical outer main sizes and gaps, selects grow or shrink from the line's outer hypothetical
sum, performs the CSS-style initial freeze, and iterates distribution, min/max clamping, violation
freezing, and redistribution. Shrink uses scaled factors (`flex-shrink × inner flex base size`).
Division uses cumulative fixed-point allocation so integer app-unit remainders are deterministic.

After main sizes settle, the planner computes line cross sizes, default multi-line stretch,
main-axis packing, per-item cross-axis alignment/stretch, and exact row/column-axis placement.
Margins, borders, and padding remain outside the item content size. `box-sizing` conversion occurs
once when a definite basis or size enters the planner.

`auto` and `content` basis use a bounded deterministic contribution from the current text measurer
and supported descendant boxes. That is sufficient for this slice but is not the complete CSS
min-content/max-content, automatic flex-item minimum-size, replaced-element, aspect-ratio, or
transferred-size algorithm.

## Resource and failure model

`LayoutLimits` now carries per-container item and line limits and one aggregate document flex-work
budget. Defaults are 4,096 items, 1,024 lines, and 1,000,000 charged units. Every item copy/sort,
line construction/pass, redistribution, clamp/freeze, cross-size, and placement pass is charged
before its effects. Intrinsic contribution walks share the same budget and the existing logical
depth bound.

Flex-owned vector reservations use `try_reserve_exact`. Planner and flex projection geometry use
checked integer arithmetic, including percentages, fixed-point factors, app-unit sums/products,
offsets, and dimensions. Exhaustion returns one of:

- `LayoutError::FlexItemLimitExceeded`;
- `LayoutError::FlexLineLimitExceeded`;
- `LayoutError::FlexWorkLimitExceeded`;
- `LayoutError::FlexAllocationFailed`; or
- `LayoutError::FlexArithmeticOverflow`.

The layout call returns no partial `LayoutOutput` on failure. No unsafe code, native FFI, process
boundary, dependency, manifest, lockfile, runtime endpoint, provider rule, credential, or telemetry
was added by this task.

## Behavioral evidence

The focused tests cover:

- exact Stylo-to-layout projection for every admitted flex value family;
- typed rejection of reverse/wrap-reverse, non-default `align-content`, baseline, unsupported
  intrinsic basis, nonlinear gap, and out-of-range factor inputs;
- exact grow and scaled-shrink results, including min/max violation refreezing;
- exact post-flex width-dependent cross remeasurement before fragment generation;
- row/column, wrap, gap, justify, item/self alignment, stretch, and order geometry;
- anonymous direct-text item creation and whitespace-only suppression, with direct element
  blockification;
- stable DOM box-child order despite visual `order` placement;
- fail-before-effect planner work admission, typed item/line/work limits, and checked extent
  overflow; and
- one generic header, search form, action region, and results-list fixture at 1366×768 and
  1920×1080, with exact header, flexible form/field, action, result width, and vertical gap geometry.

The generic fixture contains no provider names, selectors, endpoints, or page-specific exceptions.
Its field-shaped child is an ordinary styled box. The fixture is evidence for flex structure only,
not for native input/button intrinsic sizing, form-control painting, replaced elements, text
shaping, network loading, search behavior, or any named search/video site.

## Firefox and standards references inspected

At the pinned ESR checkout, implementation and tests were inspected independently of the Wild
Buzzard build:

- `layout/generic/nsFlexContainerFrame.h` and `.cpp`: flex line generation, flex base and
  hypothetical sizes, freeze/redistribute logic, main/cross trackers, gaps, and placement;
- `layout/reftests/flexbox/`: focused layout regression structure;
- `testing/web-platform/tests/css/css-flexbox/`: focused basis, grow/shrink, min/max, row/column,
  wrap, justify, align/self, gap, and order cases. Concrete behavioral anchors included
  `flex-basis-001.html`, `flex-grow-006.html`, `flex-shrink-001.html`,
  `justify-content_space-between-001.html`, `order_value.html`, `flexbox_flow-row-wrap.html`, and
  the gap-axis structure in `flexbox-column-row-gap-001.html` and its reference. Unsupported parts
  of those files, including auto margins and non-default `align-content`, were not treated as
  passing coverage; and
- `git log --follow -- layout/generic/nsFlexContainerFrame.cpp`: invariants and later corrections
  around line generation, sizing, and gap/collapse behavior, including
  `a46a009084aa`/`e1613ad57e0a` (gap with `visibility:collapse`) and `ed167330ec76` (fragmented block
  layout).

These references informed the bounded algorithm and assertions. They are read-only evidence and
are neither copied build inputs nor a claim that the full Flexbox specification is complete.

## Verification

All commands used `--locked`, target `x86_64-unknown-linux-gnu`, and external target directory
`/home/user/Documents/wildbuzzardbuilds/w8-a3l-correction`. The adapter commands used the previously
admitted exact generator environment at
`/home/user/Documents/wildbuzzardbuilds/w6-a6g/python/bin/python` (Mako 1.3.10, MarkupSafe 3.0.3,
toml 0.10.2). No network access was used.

Toolchain: rustc/cargo 1.96.0 (`ac68faa20`/`30a34c682`), rustfmt 1.9.0-stable, and Clippy 0.1.96.
The final acceptance run passed:

- explicit-edition `rustfmt --check` over all changed Rust files;
- `cargo metadata --locked --no-deps` for both manifests;
- layout `cargo test --locked --all-targets`: 7 unit and 17 integration tests;
- adapter `cargo test --locked --all-targets`: 1 unit and 28 integration tests;
- layout `cargo clippy --locked --all-targets -- -D warnings`;
- adapter `cargo clippy --locked --all-targets --no-deps -- -D warnings`; `--no-deps` keeps the
  first-party gate strict without re-linting recorded imported Stylo procedural-macro debt;
- release builds for both manifests;
- `RUSTDOCFLAGS='-D warnings' cargo doc --locked --no-deps` for both manifests; and
- tracked and new-file `git diff --check` whitespace validation.

The test, lint, release, and documentation commands set `CARGO_NET_OFFLINE=true`. No repository
`target/`, generated Stylo output, manifest, or lockfile was created or changed by this lane.

## Remaining work

This is not complete Flexbox and not a normal-page compatibility claim. Reverse axes,
wrap-reverse, inline flex, baseline and multi-mode content alignment, automatic flex margins,
complete intrinsic and automatic minimum sizing, aspect-ratio transfer, replaced/form controls,
fragmentation, orthogonal/vertical flows, bidi, shaping, grid, tables, painting, hit testing, and
accessibility geometry remain later gates. W8-A4T separately admits `BoxKind::Flex` to the existing
validated renderer decoration path, closing the first-freeze background/border integration blocker;
that does not complete broader CSS painting. A provider compatibility ladder must be driven by
generic missing primitives and recorded reference/conformance tests; this slice adds no provider-
specific hacks.

## Frozen source hashes

SHA-256 after the final gates (the self-referential handoff file is excluded):

| Path | SHA-256 |
| --- | --- |
| `layout/src/flex.rs` | `45ca690b90334b970a9fff1e804ecdfc970bef9254a49e1a1f88a887f7108b3a` |
| `layout/src/style.rs` | `0424fcaa42bc8876f0c546cf798a3be0f55d792f7bb643e1d2b0affa7a3f0933` |
| `layout/src/tree.rs` | `ce05d7c8d83b26e99179ce2fb12be11dfbab91c5caaf9af357602d1de1554a65` |
| `layout/src/lib.rs` | `82cfe4b2fcafd8c06df9a84b18952e77e0680ca84341e0c2c372e83a5ddaf9a9` |
| `layout/tests/static_layout.rs` | `a533eb8bc7faa644c918c46ffab0ead949d022b47d5259fc76617a110f492c53` |
| `layout/README.md` | `d0d4db439f9d8c8889a9405701b8328a8978f9435d6c5b9053a04a92607db2fb` |
| `servo/components/wild_buzzard_stylo_adapter/translate.rs` | `d4d1d6868d9bb6b47ea384a7f16e3d8d3cb8a48462074f9e65356a93acb3a8d8` |
| `servo/components/wild_buzzard_stylo_adapter/error.rs` | `8087b1c86d3000a9b4cf821a3173db2c3c3e42d3c6d70eff1f20ba3b6f419365` |
| `servo/components/wild_buzzard_stylo_adapter/tests/static_style.rs` | `0ab08e08c6a2b1ce223d534eea3f54db8fb65ac72bda1cda04af7b98df439d94` |
| `servo/components/wild_buzzard_stylo_adapter/README.md` | `349c7eed791e2089d9202c82baf37643856baa29efcdc07a1cdf66a38ceeee39` |

Relative to the rejected first freeze, the correction changed exactly
`layout/src/flex.rs`, `layout/src/tree.rs`, `layout/README.md`, adapter `error.rs`, adapter
`tests/static_style.rs`, adapter `README.md`, and this handoff. The other four implementation files
in the original 11-path scope retain their first-freeze hashes.

## Files

- `layout/src/flex.rs`
- `layout/src/style.rs`
- `layout/src/tree.rs`
- `layout/src/lib.rs`
- `layout/tests/static_layout.rs`
- `layout/README.md`
- `servo/components/wild_buzzard_stylo_adapter/translate.rs`
- `servo/components/wild_buzzard_stylo_adapter/error.rs`
- `servo/components/wild_buzzard_stylo_adapter/tests/static_style.rs`
- `servo/components/wild_buzzard_stylo_adapter/README.md`
- `docs/handoffs/W8-A3L-normal-page-flex-layout.md`

The code and documentation are MPL-2.0. Firefox and WPT references retain their upstream licenses
and were not imported into this change.
