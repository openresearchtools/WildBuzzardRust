# W9-A3R: bounded inline-block shrink-to-fit

## Scope and outcome

W9-A3R replaces the historical typed stop for a non-replaced `display:inline-block` with
`width:auto` by one bounded CSS2 shrink-to-fit path in `wild_buzzard_layout`. Definite-width
inline-block behavior from W9-A3Q remains on its existing path. Imported Stylo already projects
author `display:inline-block` and `width:auto` losslessly, so no translation change was needed.

The implementation baseline is repository commit
`1d7c017a13b43d5103cc93c41fbeed538e2078fd`. This task owns only:

- `layout/src/tree.rs`;
- `layout/tests/static_layout.rs`;
- `layout/README.md`;
- `servo/components/wild_buzzard_stylo_adapter/tests/static_style.rs`;
- `servo/components/wild_buzzard_stylo_adapter/README.md`; and
- this handoff.

No workspace manifest, lockfile, Stylo translation source, renderer source, parity TOML, browser
source, or protected JavaScript path is changed by this task. The ignored `firefox/` checkout is
reference material only and remains neither edited nor a build/runtime input.

## Observable sizing contract

For an auto-width inline-block, layout computes one ordered content-size pair and selects:

```text
used content width = min(max(preferred-minimum, available), preferred)
available = containing width - used subject margins - subject border/padding
```

The subject's percentage margins and padding resolve against the real definite containing width.
Its horizontal automatic margins have zero used value. The selected content width then passes
through the existing checked maximum-before-minimum constraint order, including content-box and
border-box conversion. Negative available space is retained through the formula rather than
prematurely clamped; the nonnegative preferred-minimum selects the safe result. Signed subject
margins are likewise retained, so negative margins enlarge available space.

For deterministic 16px monospace text, `aaaa bbbb` contributes `(32px, 72px)`. Containing content
widths 24px, 48px, 80px, and 100px therefore publish inline-block content widths 32px, 48px, 72px,
and 72px respectively. A 40px container with -10px subject margins on each side yields 60px of
available content. A 24px container with 20px subject margins on each side has negative available
space and still selects the 32px preferred-minimum.

### Frozen contribution table

The pair deliberately represents only the formatter behavior proven in this gate:

| Contribution | Preferred-minimum | Preferred |
| --- | --- | --- |
| Normal inline stream | Longest unbreakable segment under the existing ASCII CSS whitespace collapse and atomic-boundary rules | Widest forced line; soft opportunities do not end the preferred line |
| `nowrap` inline stream | Widest complete forced line | Widest complete forced line |
| `pre` inline stream | Widest newline/`br`-delimited preserved line | Same |
| Atomic descendant | Its signed checked outer contribution; a normal optional boundary ends a minimum segment unless the current line is negative | Appended to the current preferred line |
| Direct block children | Maximum checked outer child contribution | Maximum checked outer child contribution |
| Row flex, nowrap | Sum of item minimums plus absolute column gaps | Sum of item preferred widths plus absolute column gaps |
| Row flex, wrap | Maximum item minimum | Sum of item preferred widths plus absolute column gaps |
| Column flex | Maximum item minimum | Maximum item preferred width |

Normal collapsed spaces, `nowrap`, preserved spaces/newlines, explicit `br`, nested inline
ancestry, and no-space atom-to-atom/text-to-atom/atom-to-text boundaries stay distinct. Minimum and
preferred are accumulated together in one traversal. A direct-root regression combines text, an
atom, `br`, and more text; replaying the walker would widen its correct 32px pair to 56px and is
therefore caught.

Non-atomic inline margins, borders, and padding remain deliberately unapplied by the current inline
formatter. Intrinsic sizing excludes those edges too, and the ordinary pass still publishes
`LayoutWarningCode::InlineEdgesNotApplied`. This is consistency with a recorded limitation, not a
claim of inline-edge parity. Explicitly unrepresented replaced, table, form-control, list-marker,
ruby, media, unmodeled `wbr`, and similar contributions stop as
`LayoutError::UnsupportedInlineBlockIntrinsicContribution { node_id }` at the exact node rather
than fabricating a size.

## Cyclic percentages and box geometry

Intrinsic contributions have an indefinite containing-inline-size basis. For the supported
non-replaced cases:

- a preferred `width` with a nonzero percentage component behaves as auto;
- a `max-width` with a nonzero percentage component behaves as its initial unbounded value;
- a percentage `min-width` is ignored as a whole, including a `calc()` absolute length component;
- percentage margins and padding resolve against zero while their separately projected absolute
  lengths remain; and
- final descendant layout resolves the same values again against the selected definite containing
  width.

One regression uses cyclic `calc(10px + 50%)` width/max, cyclic
`min-width:calc(20px + 50%)`, 4px absolute left padding, 50% right padding, and `a b`. The intrinsic
outer pair is `(12px, 28px)`, so the subject publishes 28px with 100px available and 16px with 16px
available. The narrow control would incorrectly publish 24px if the calc length were retained.
Actual descendant layout then resolves the percentages against 28px or 16px; the resulting border
boxes are 52px and 40px. This intentionally demonstrates visible descendant overflow rather than
feeding the final percentage result back into the cached pair.

Signed outer contributions remain signed while composing inline and single-row flex state. An
atomic 20px child with `margin-right:-40px` contributes -20px; while the current minimum line is
negative, its optional boundary before an adjacent 48px word is ignored, producing 28px rather than
48px. The equivalent row-flex sum is also 28px. Only a completed content pair stored for subject
sizing is made nonnegative, so a lone -20px contribution publishes zero.

Subject maximum/minimum precedence is also frozen for content and border boxes. A border-box case
with 20px total border/padding, `max-width:50%`, and `min-width:60%` in a 100px containing block
publishes a 60px border box: the 50px maximum is applied before the 60px minimum.

All new subject edge resolution, border-box conversion, available-width subtraction, contribution
addition, constraint geometry, and fragment geometry use checked app-unit arithmetic. Focused
regressions reject both an overflowing descendant outer contribution and an overflowing subject
available-width subtraction as `LayoutError::InlineArithmeticOverflow`.

## Cache, recursion, and work bounds

The intrinsic cache is private to one `LayoutEngine`. It is allocated only when the first auto-width
inline-block needs it, fallibly reserves exactly the already constructed box count, and has one
entry per box:

- `Empty` has not been measured;
- `Computing` rejects a recursive self-cycle; and
- `Ready(IntrinsicInlineSizes)` stores the inseparable minimum/preferred pair.

The pair is independent of the later available width because percentage terms that would introduce
a cyclic basis are intentionally resolved by the frozen intrinsic rules above. Nested auto
inline-block regressions prove the same cached inner pair can be used after the outer selection:
24px available selects 32px for both boxes, while 48px selects 48px for both. No self-cycle occurs.

Cache initialization is charged for the actual box count before allocation. Intrinsic box/child
visits, input-byte scans, measurement bytes, ancestry allocation/copy/comparison, constraint
comparisons, maximum updates, and checked accumulations all consume the existing aggregate inline
work budget before their corresponding work. The focused `aaaa bbbb` fixture succeeds with exactly
70 charged units and fails with 69. At limit 30 it fails before the first text measurement; a
panicking `TextMeasurer` makes that ordering observable.

## Firefox ESR153 and standards evidence

The read-only reference checkout was verified at
`c19b7e89270787889495688244ec6ee8e79288a1`. Focused implementation paths inspected were:

- `layout/generic/nsIFrame.cpp`: `ComputeAutoSize`, available content-box subtraction, and
  `ShrinkISizeToFit`'s preferred-minimum/available/preferred clamp;
- `layout/generic/nsBlockFrame.cpp`: block preferred-minimum and preferred contributions;
- `layout/generic/nsFlexContainerFrame.cpp`: single-line/wrapped row and column contribution rules;
- `layout/base/nsLayoutUtils.cpp` and `layout/generic/nsIFrame.cpp`: cyclic non-replaced percentage
  width/min/max, margin, and padding treatment; and
- the existing W9-A3Q atomic inline, line-layout, box-model, and checked-geometry references listed
  in `docs/handoffs/W9-A3Q-inline-block.md`.

The core shrink-to-fit formula traces through history to `d21cb374bd0fc`, with later relevant clamp
and contribution changes at `f8f540104cdda`, `2e136118e4347`, and `6af05a09770c8`. History was used
to identify invariants, not to translate Gecko architecture.

Focused tests read in full include:

- `testing/web-platform/tests/css/CSS2/normal-flow/inline-block-non-replaced-width-{001,002,003,
  004}.xht`;
- `testing/web-platform/tests/css/css-sizing/intrinsic-percent-non-replaced-{001,002,003,
  006}.html`;
- `testing/web-platform/tests/css/css-sizing/fit-content-percentage-padding.html`;
- `testing/web-platform/tests/css/CSS2/normal-flow/intrinsic-size-with-anonymous-block.html`;
- `testing/web-platform/tests/css/CSS2/normal-flow/intrinsic-size-with-negative-margins.html`;
- `testing/web-platform/tests/css/css-flexbox/multiline-shrink-to-fit.html`; and
- the W9-A3Q whitespace, atomic-boundary, and inline-block tests recorded in the prior handoff.

These are behavioral references. This gate does not claim the imported WPT files pass wholesale.

## Focused deterministic evidence

After correcting the independent review findings, the exact focused layout integration suite
passed 56/56.
It covers:

- the four numerical shrink-to-fit branches and both desktop viewports;
- direct-root pair non-replay;
- normal collapse/nesting, `nowrap`, `pre`, newline, `br`, and atomic boundaries;
- nested auto atoms, direct blocks, and supported row/column/wrapped flex contributions;
- real subject percentage edges, zero used automatic margins, signed subject/descendant margins,
  signed atomic/row-flex accumulation, negative optional-break state, negative available space,
  ESR cyclic descendant values with a distinguishing narrow control, actual re-resolution, and
  overflow;
- content-box/border-box and percentage maximum-before-minimum precedence;
- empty content, explicit unsupported contributions including `wbr`, two checked overflow paths,
  and the deliberate inline-edge warning contract; and
- exact/next-unit work limits plus failure before measurement.

The imported-Stylo focused test
`stylo_inline_block_auto_width_uses_shrink_to_fit_at_both_desktop_viewports` also passed. It proves
author CSS leaves the atom's computed width as `SizeValue::Auto`, then publishes a 48px atom for
`aaaa bbbb` in a 48px block at 1366×768 and 1920×1080. It does not reparse CSS in layout or claim
general intrinsic keyword support.

The independent review returned NO-GO for prematurely clamped negative atomic contributions,
incorrect cyclic percentage-min behavior, and an unmodeled `wbr`. Those findings were corrected by
the signed line/row-flex state, ESR ignore rule and narrow control, and exact typed stop above. The
corrected code/tests and final citation fix then received independent GO with no remaining finding.
The user-imposed wrap deadline permitted the focused suites only; broad
check/Clippy/all-target/release/rustdoc gates and the opt-in public Google probe were not run. No
parity TOML result is changed or claimed.

### Post-commit orchestrator integration closure

On 2026-08-23 the main orchestrator closed the skipped owner gates against committed source
`92d2267dd5e5797ef8cab89f41610b182f97b97e`. The repository was mounted read-only in the reusable
Debian 13/Rust 1.90 container; its Podman graph root, Cargo home, target directory, Python support,
and every generated artifact remained under `/run/media/user/Data/Repositories/wildbuzzardbuilds`.
The tool image was extended externally with the pinned toolchain's Clippy and rustfmt components;
no toolchain or container file was added to the source tree.

The accepted closure matrix is:

- root-workspace layout: 8 unit and 56 integration tests passed, followed by locked all-target
  check, strict all-target Clippy with warnings denied, release build, and warning-denied rustdoc;
- imported-Stylo adapter: 1 unit and 41 integration tests passed, followed by locked all-target
  check, strict owner-only `--no-deps` Clippy with all/pedantic warnings denied, release build, and
  warning-denied owner rustdoc;
- the focused desktop shrink-to-fit adapter regression passed again, and exact-path Rust 1.90
  rustfmt validation required no source change;
- exact-path rustfmt and repository whitespace checks passed; imported Servo/WebRender formatting
  outside the owned files is not attributed to this gate; and
- the public Google probe was not rerun, so the previous typed blocker remains historical evidence
  rather than a new frame or parity claim.

All W9-A3R source hashes remain identical to the wrap-deadline inventory below.

Final focused source hashes at the wrap deadline:

| Path | SHA-256 |
| --- | --- |
| `layout/src/tree.rs` | `eacf66809600a1723b85b08d3cb6f46b6fed3c987171a6178a7adca3f78b6654` |
| `layout/tests/static_layout.rs` | `aa6e3ab86689819072d1960ef7ce47b11a03e6ff1e81f91e117f3440e1356a62` |
| `layout/README.md` | `d7944846e46a3e10835f494ca01339b406c590c3e770b492d74edf6d573a9a88` |
| `servo/components/wild_buzzard_stylo_adapter/tests/static_style.rs` | `17bc4bd11efb73219101ea2d731dcb15a88f7d36f5cb62734f4696d26dd67517` |
| `servo/components/wild_buzzard_stylo_adapter/README.md` | `5e2cc0e49894dbcb50c86e62b02ac7f3653d72e02a7870dd46e97a03eb73fad4` |

The retained external task tree contains only the reusable Cargo target (2,521,056,990 bytes) and
the pinned Python environment (11,991,297 bytes). No detached probe source/target, response body,
screenshot, or log was created. This handoff's non-self-referential SHA-256 is reported in the
external closeout message after its final write.

## Explicit limitations and next gates

- The intrinsic walker is not a general CSS Sizing implementation. Replaced elements, tables,
  form controls, list markers, ruby, floats, and other unrepresented contributions remain typed
  stops.
- The current ASCII whitespace/collapse and atomic-boundary model remains unchanged; there is no
  Unicode line breaking, shaping, bidi, hyphenation, or language-sensitive segmentation.
- Non-atomic inline edges remain unapplied with an explicit warning. Inline edge fragments and
  complete intrinsic contribution parity remain future work.
- The inline-block inside is still the bounded existing block/flex/inline subset, not a complete
  flow-root or block-formatting-context implementation.
- Baseline synthesis and `vertical-align` remain absent, as recorded by W9-A3Q.
- The cache is per-layout and whole-box-tree sized. It is bounded and fallible, but not a retained
  incremental style/layout cache.
- Desktop geometry evidence is deterministic structural evidence, not pixel/reftest, interaction,
  script, subresource, or general-site parity.
