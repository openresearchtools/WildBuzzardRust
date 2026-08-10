# Wild Buzzard static layout nucleus

`wild_buzzard_layout` consumes only an owned `DocumentSnapshot`. Its production-facing static path
accepts an exact document/revision-matched `ComputedStyleSnapshot` projected from the imported
Stylo engine by `servo/components/wild_buzzard_stylo_adapter`. The older `StyleResolver` seam and
`InitialStyleResolver` remain deterministic test/bootstrap paths. `TextMeasurer` is the font-system
boundary. None of these contracts grants access to mutable DOM, platform APIs, renderer internals,
or fonts.

Geometry uses signed `Au` values at 60 app units per CSS pixel with saturating arithmetic and
explicit `Point`, `Size`, `Rect`, `Edges`, and `Viewport` types. The output is a logical box tree
whose boxes own renderer-facing fragments. Block boxes generate anonymous blocks around contiguous
inline runs. The wave-one inline context collapses normal whitespace across nested inline nodes,
supports collapsed `nowrap` without admitting soft breaks, honors `pre` newlines, handles `br`,
wraps normal text at words and then character boundaries, and represents multi-line inline elements
with multiple fragments. A typed pending-space state retains soft-break eligibility contributed by
Normal descendants across mixed inline boundaries. `nowrap` never aliases to `pre`, which preserves
whitespace. A separate atomic-boundary state records a break opportunity without inventing a text
space fragment. For a no-space boundary involving an atomic inline, the formatter compares the
bounded inline-ancestor paths of the preceding and current visible items; the `white-space` value
on their nearest common inline ancestor controls whether the boundary is soft. The visible text or
atomic leaf itself is excluded from that path. Empty and whitespace-only nodes do not replace the
last visible boundary owner, while a forced or soft line transition clears it.

`display:inline-block` is retained as `Display::InlineBlock` and constructs a distinct
`BoxKind::InlineBlock`. Its outer box remains one unbreakable inline-level item in its parent's
anonymous inline run, while its inside wraps ordinary inline runs and lays block descendants through
the supported block formatting path. Definite computed widths retain their prior
length-percentage, min/max, and `box-sizing` resolution. For `width:auto`, a lazy fallible cache
computes the content's preferred-minimum and preferred width together and applies CSS2
`min(max(preferred-minimum, available), preferred)`, where available is the containing inline size
less the subject's real used margins, borders, and padding. Subject percentages use that definite
containing size and subject automatic margins have zero used value. The resulting content width is
then constrained in maximum-before-minimum order.

The bounded intrinsic walker models only behavior supported by the current formatter. Under
`white-space:normal`, preferred-minimum is the longest current ASCII-collapse/unbreakable segment
and preferred width is the widest forced line. `nowrap`, `pre`, `br`, nested inline content, and the
existing nearest-common-ancestor atomic boundary rule remain distinct. Direct block contributions
use their maximum child contribution. The supported flex subset uses a row sum for preferred width,
a row nowrap sum or wrapped-row maximum for preferred-minimum, and a column maximum for both;
applicable absolute main-axis gaps participate. Replaced, table, form-control, list-marker, ruby,
and other explicitly unrepresented contributions fail as
`LayoutError::UnsupportedInlineBlockIntrinsicContribution` instead of inventing a size.

Intrinsic percentage resolution follows the bounded ESR cyclic rules: a non-replaced percentage
preferred width, percentage maximum, and percentage minimum are ignored as whole values, including
any `calc()` length component. Percentage margins/padding resolve against zero while retaining
separately projected lengths. Actual descendant layout then resolves those percentages again
against the final definite containing width. Signed atomic outer contributions are preserved in
inline and row-flex state; an optional minimum break is ignored while the current line is negative,
and only the final cached content pair is made nonnegative. Non-atomic inline edges remain deliberately
unapplied by this formatter; the intrinsic result likewise excludes them and ordinary layout still
publishes `InlineEdgesNotApplied`, so this is not a claim of inline-edge parity. Definite or natural
height, physical margins, padding, borders, background-bearing fragments, and descendant overflow
use the existing block box model. Inline-block automatic margins never enter ordinary CSS2 block
auto-margin distribution. Its physical edge resolution, fragment right/bottom extents, atomic
cursor advance, wrap transition, remaining-width query, and final line height use checked app-unit
arithmetic. After an atom is admitted, later transitions and advances in that same inline context
remain checked; atom-free contexts retain their pre-existing cursor behavior.

Atomic placement consumes a collapsed leading space without painting one, moves the complete item
at an eligible soft-space or nearest-common-ancestor atomic boundary, never splits it, retains an
unbreakable boundary when that common ancestor is `nowrap`/`pre`, and honors `br`. Collapsed-space
break eligibility remains owned by the whitespace run rather than being overwritten by the atomic
rule. The current line formatter places the atomic outer margin box at the line top. It does not
project `vertical-align` or implement last-line/empty-inline-block baseline synthesis, so default
baseline and bottom alignment remain explicit gaps rather than fabricated geometry. The pending
collapsed space immediately before an atom is still measured with the atom's text style rather
than the whitespace owner's exact shaped style; complete mixed-font boundary geometry is therefore
not claimed.

The output root also owns one private, immutable `CanvasBackgroundDecision` derived from the same
document snapshot and computed styles used for box construction. The decision is sealed to that
exact `DocumentVersion`, root box/node construction identity, canvas-relevant root style facts,
and canonical-body provenance. Background transparency follows ESR153's exact bounded predicate:
a transparent color is empty only when the computed background-image list contains exactly one
`none`; URL, gradient, and even `none, none` lists make the root meaningful. A meaningful root wins
before containment is considered. Only a known-transparent, uncontained root may consult the
canonical HTML-namespace `body` child of an HTML-namespace `html` root; an effectively contained
body cannot propagate. `display:inline` remains eligible while `display:none` does not. Unknown or
unrepresented facts fail closed without body fallback.

This gate emits only a copied nontransparent color. A meaningful image-only root therefore blocks
body propagation but does not fabricate a color primitive; the colored body remains locally
paintable. `CanvasBackgroundSource::{RootElement, HtmlBody}` describes the origin while a separate
private construction identity seals the exact source box and node. A fully transparent result is
represented by absence, not a fabricated white or transparent paint command. Graphics can fill
the exact viewport from the copied color/identity and suppress every fragment of that exact source
without consulting mutable DOM state.

The horizontal block path applies non-intrinsic computed width/height and min/max constraints,
percentage inline sizes, and both `content-box` and `border-box` interpretation. A definite parent
height supplies the percentage basis for child block sizes. CSS minimums win when a minimum exceeds
the corresponding maximum. It preserves automatic physical margins through computed style and
resolves their used values for ordinary left-to-right blocks after width constraints: one automatic
inline side absorbs positive remaining space, two split it with any app-unit remainder assigned to
the inline end, and negative over-constraint adjusts only the inline-end margin. Automatic
block-axis margins have zero used value. Vertical writing modes remain typed in `ComputedStyle` and
return `LayoutError::UnsupportedWritingMode` before box construction can fabricate horizontal
geometry. The inherited CSS `direction` value is likewise retained as `InlineDirection`; any RTL
element returns `LayoutError::UnsupportedInlineDirection` during box construction, before box or
fragment publication, because this gate implements only the LTR width equation. Anonymous styles
inherit the same typed direction through `ComputedStyle::inherit_from`.

The normal-page path also includes one bounded, horizontal-writing-mode CSS Flexbox formatting
context. It consumes Stylo-projected computed values rather than parsing CSS: `display:flex`, row
and column axes, nowrap and wrap, `flex-basis` auto/content/length-percentage, grow and scaled
shrink, explicit min/max clamps, main-axis packing, cross-axis item/self alignment and stretch,
row/column gaps, and visual `order`. Flex items retain their DOM box-tree order; `order` affects
only placement. Contiguous direct text becomes an anonymous flex item, whitespace-only runs are
suppressed, and element children are blockified. The flexible-length loop uses checked integer
app-unit arithmetic, deterministic remainder assignment, and iterative min/max freezing.
For a row item whose cross size is automatic, the first plan supplies the final content width;
layout then remeasures width-dependent content at that exact width and runs a second private plan
before emitting fragments. The remeasurement pass, its intrinsic walk, and the duplicate planner
work all consume the same document flex-work budget.

`LayoutLimits` bounds box admission, logical recursion during box construction, aggregate inline
work, and flex layout. The defaults are 1,000,000 boxes, depth 256, 1,000,000 inline charged units,
4,096 flex items per container, 1,024 flex lines, and 1,000,000 aggregate flex charged units. Box
child reservation is capped by the remaining box allowance. Block/inline child copies and inline
fragment aggregation use fallible exact reservations with typed errors. Each inline visit, input
text byte, byte in a repeated growing-prefix probe, copied or compared inline-ancestor entry,
fragment aggregation entry, existing-line comparison, flex item, line, and redistribution pass is
charged before the corresponding formatter/planner work. This prevents a long unspaced token or a
many-line nested inline from hiding quadratic work behind a linear-looking budget.
The shrink-to-fit cache is allocated only on first use, fallibly reserves exactly the final box
count, and is charged before allocation. Intrinsic visits, text scans and measurements, ancestry
copies/comparisons, constraint comparisons, and checked accumulations consume that same aggregate
inline-work budget. Cache entries distinguish empty, currently computing, and ready pairs so a
recursive self-cycle cannot be treated as a completed contribution.
`layout_document_with_limits` permits caller-selected bounds and
returns `LayoutError::TreeDepthLimitExceeded { limit, node_id, phase }`; the original
`layout_document` API remains the default-limits convenience entry point. This applies after an
iterative DOM snapshot, including to script-created trees.

Normal white-space collapsing uses the wave-one CSS set (TAB, LF, FF, CR, and SPACE). NBSP and
other Unicode spaces remain in text fragments for the eventual shaping backend. Both `normal` and
`nowrap` use that collapsing set; only `normal` contributes soft line-break opportunities. An
explicit `br` remains a forced break under `nowrap`.

`InitialStyleResolver` is a deterministic minimal UA baseline only. It hides non-rendered head
content, maps common HTML elements to block/inline display, supplies body/paragraph/heading/pre
defaults, and honors `hidden`. It does not parse author CSS or claim Stylo parity.

## ESR153 and test references inspected

Pinned reference: `firefox/` at `c19b7e89270787889495688244ec6ee8e79288a1` (read-only, never a
dependency).

- `gfx/src/AppUnits.h` for the 60-app-unit CSS pixel scale.
- `layout/generic/nsBlockFrame.{h,cpp}` and `BlockReflowState.{h,cpp}` for block/line ownership and
  normal-flow reflow structure.
- `layout/generic/ReflowInput.{h,cpp}`, especially `CalculateBlockSideMargins`, for post-constraint
  CSS2 block-width auto-margin distribution, inline-end over-constraint, direction-dependent
  ignored margins, and app-unit rounding.
- `layout/generic/nsInlineFrame.{h,cpp}` for inline continuations/fragments.
- `layout/generic/nsLineLayout.{h,cpp}` for inline-coordinate advancement, line starts, wrapping,
  and baseline placement.
- `layout/generic/nsTextFrame.{h,cpp}` for the text-measurement boundary.
- `servo/components/style/values/specified/box.rs` for the exact inline-outside/flow-root-inside
  `Display::InlineBlock` computed representation.
- `layout/generic/nsIFrame.cpp` (`IsAtomicInline`), `layout/generic/ReflowInput.cpp`, and
  `layout/generic/nsContainerFrame.cpp` for atomic-inline classification and the CSS2 shrink-wrap
  size path; `nsIFrame.cpp`'s `ComputeAutoSize` and `ShrinkISizeToFit` supply the available-width
  subtraction and preferred-minimum/available/preferred clamp order.
- `layout/generic/nsBlockFrame.cpp` for block preferred-minimum and preferred contributions, and
  `layout/generic/nsFlexContainerFrame.cpp` for the bounded row/column, wrap, and gap contribution
  cases admitted here.
- `layout/base/Baseline.cpp` for atomic-inline baseline synthesis and the alignment behavior not yet
  represented by this formatter.
- `layout/painting/nsCSSRendering.cpp`, especially `FindBackgroundStyleFrame`,
  `FindCanvasBackgroundFrame`, and `FrameHasMeaningfulBackground`, for root/body selection and the
  rule that a propagated source does not also paint its frame background.
- `layout/base/PresShell.cpp`, especially `ComputeSingleCanvasBackground` and
  `ComputeCanvasBackground`, plus `layout/generic/nsCanvasFrame.cpp`, for CSS canvas color,
  default-background composition, and bottom-of-canvas paint ordering.
- `layout/generic/nsFlexContainerFrame.{h,cpp}` for flex-line construction, hypothetical and base
  sizes, iterative freezing/redistribution, post-flex main-size cross remeasurement (notably
  `nsFlexContainerFrame.cpp` lines 5351–5384), gap accounting, and main/cross packing.
- `layout/reftests/inline-borderpadding/ltr-basic.html` and
  `layout/reftests/inline-borderpadding/ltr-span-only.html`.
- `layout/reftests/text/white-space-1a.html`, `white-space-1b.html`, and
  `white-space-1-ref.html` for collapsed-space break eligibility across mixed Normal/Nowrap inline
  boundaries, including the extra-span form.
- `layout/reftests/first-letter/inline-height-empty.html` to identify empty-inline behavior still
  missing here.
- `layout/reftests/abs-pos/continuation-positioned-inline-1.html` to identify continuation and
  positioned-inline behavior still missing here.
- `testing/web-platform/tests/css/css-sizing/min-width-max-width-precedence.html` for minimum-size
  precedence.
- `testing/web-platform/tests/css/CSS2/normal-flow/inline-block-non-replaced-width-{001,002,003,
  004}.xht` for auto shrink-to-fit and width/min/max interaction.
- `testing/web-platform/tests/css/css-sizing/intrinsic-percent-non-replaced-{001,002,003,
  006}.html` and `fit-content-percentage-padding.html` for cyclic non-replaced percentage
  contributions.
- `testing/web-platform/tests/css/CSS2/normal-flow/intrinsic-size-with-anonymous-block.html` and
  `intrinsic-size-with-negative-margins.html` for anonymous block and signed-margin contributions.
- `testing/web-platform/tests/css/css-flexbox/multiline-shrink-to-fit.html` for wrapped flex
  preferred-minimum behavior.
- `testing/web-platform/tests/css/css-sizing/box-sizing-content-box-001.xht` and
  `box-sizing-border-box-001.xht` for sizing interpretation.
- `testing/web-platform/tests/css/CSS2/margin-padding-clear/margin-auto-on-block-box.html` and
  `testing/web-platform/tests/css/CSS2/normal-flow/auto-margins-used-values.html` for centered,
  one-sided, and over-constrained automatic margins.
- `testing/web-platform/tests/css/CSS2/normal-flow/block-non-replaced-height-001.xht` for zero used
  values on automatic vertical margins.
- `testing/web-platform/tests/css/css-writing-modes/writing-mode-vertical-rl-003.htm` as behavior
  that must remain an explicit unsupported error until vertical flow exists.
- `testing/web-platform/tests/css/css-text/white-space/white-space-nowrap-011.html`,
  `text-wrap-nowrap-001.html`, and `white-space-wrap-after-nowrap-001.html` for collapse versus
  wrapping, forced breaks, and mixed inline boundary behavior.
- `testing/web-platform/tests/css/css-sizing/whitespace-and-break.html` for the collapsible-space and
  forced-break boundary after an inline-block.
- `testing/web-platform/tests/css/css-text/line-breaking/line-breaking-{030,031,032}.html` for the
  rule that the nearest common ancestor's `white-space` controls atom-to-atom, text-to-atom, and
  atom-to-text boundaries even when both descendants override it with `pre`.
- `testing/web-platform/tests/css/css-text/line-breaking/line-breaking-atomic-{007,008}.html` for
  soft opportunities before and after atomic inlines. Atomic-008 establishes that this opportunity
  is not suppressed by `word-break:keep-all`; `word-break` itself is not projected by this slice,
  so the full WPT is a future conformance target rather than a claimed pass.
- `testing/web-platform/tests/css/CSS2/linebox/vertical-align-baseline-004.xht` and
  `vertical-align-baseline-006a.xht` for empty/last-line inline-block baselines retained as a gap.
- `testing/web-platform/tests/css/CSS2/margin-padding-clear/margin-collapse-014.xht` for independent
  inline-block margins and the absence of sibling margin collapse.
- Focused `testing/web-platform/tests/css/css-flexbox/` row/column, wrap, basis, grow/shrink,
  min/max, justify, align/self, gap, and order cases, including
  `flexbox-column-row-gap-001.html` and the corresponding references.
- `testing/web-platform/tests/css/css-backgrounds/background-color-body-propagation-{001,002,003,
  004,008,009}.html` for body fallback, root precedence, inline-body eligibility, `display:none`
  exclusion, and root/body paint-containment rejection; CSS2 `background-root-*` and root/body
  background cases were also used as observable-behavior references.

History was inspected with `git log --follow`, including changes around `194f92ebae0e` (baseline
retrieval), `4e0e1888a6eb`/`e562ea4a57c4` (text-indent reflow), and `ed167330ec76` (fragmented block
layout), plus `a46a009084aa`/`e1613ad57e0a` (flex gap and `visibility:collapse` interaction). Those
paths define future assertions; `a38209396aae` introduced the focused inline-block whitespace WPT
and `42d52eb2feb8` refined atomic-inline alignment-baseline synthesis. This wave intentionally
implements a much smaller Rust formatting model.

## Static-layout tests

`tests/static_layout.rs` exercises the complete parse-to-snapshot-to-layout path, body geometry,
anonymous inline runs around block children, deterministic word and overlong-word wrapping,
whitespace collapse across nested spans, collapsed-nowrap overflow, forced `br` lines, custom
style-driven suppression and block/padding geometry, exact computed-style publication,
document/revision rejection, percentage edge resolution against the containing inline size,
preferred/min/max block geometry, box sizing, vertical-writing-mode rejection, and invalid viewport
rejection.
Canvas tests exercise an exact immutable computed-style publication, canonical HTML-body fallback,
root precedence, inline-body eligibility, `display:none`, transparent absence, source-box identity,
and the invariant that provenance is attached only to the root layout box.
Inline-block tests exercise distinct box generation, fixed atomic placement at Normal and Nowrap
boundaries, the exact nearest-common-ancestor `white-space` rule for atom-to-atom, text-to-atom, and
atom-to-text pairs under both parent-normal/descendant-pre and parent-nowrap/descendant-normal
overrides, forced breaks, exact margin/padding/border/background geometry, an independent block
descendant, definite-height overflow, and zero used automatic margins. Auto-width cases cover the
exact CSS2 three-way clamp, normal/nowrap/pre/`br` text pairs, direct-root non-replay, atomic
descendants, nested auto atoms, block and bounded flex contributions, signed margins and negative
atomic accumulation, subject and cyclic descendant percentages, content/border box constraints,
explicit unsupported contributions (including unmodeled `wbr`), the existing
`InlineEdgesNotApplied` contract, and checked arithmetic. Exact cache/work tests pass
at 70 charged units and reject 69, while a panic measurer proves the limit can fail before the first
measurement. Exact-edge hostile tests still prove rejection before the next growing-prefix attempt
and before the next nested-inline line-aggregation comparison. A positive line origin plus a
maximum line height also proves an atom-triggered wrap fails before saturating its next y coordinate.
Depth tests exercise the default 256-level block boundary, structured box-construction failure at
257, and inline-layout failure when an anonymous box adds logical depth. Whitespace tests cover
NBSP, em space, collapsible ASCII runs, uniform collapsed-nowrap overflow, forced breaks, and both
directions of typed mixed Normal/Nowrap pending-space eligibility through the full parse-to-layout
path.
Flex tests exercise checked grow/shrink redistribution, min/max refreezing, wrapping and gaps,
row/column geometry, main- and cross-axis placement, visual ordering with stable DOM child order,
anonymous-item construction, and typed item, line, work, and arithmetic boundaries. Allocation
failures use the same typed failure surface but are not induced nondeterministically in tests.
The Stylo adapter suite adds exact computed-value projection and generic desktop header, form, and
results geometry at 1366×768 and 1920×1080. It also proves post-flex cross remeasurement with a
100px row container whose 20px item wraps ten 5px glyph advances into three 10px lines, producing
exact 30px item and container heights. Automatic-margin tests cover typed Stylo projection,
both/one-sided distribution, constrained width, negative over-constraint, vertical zeroing,
deterministic odd-app-unit splitting, and a generic `width:60vw; margin:15vh auto` block with exact
geometry at both desktop viewports. Flex-item and inline automatic margins fail with
`LayoutError::UnsupportedAutomaticMargin` and a typed formatting-context discriminator. Direction
tests prove inherited style retention, explicit Stylo LTR projection, and exact pre-fragment typed
rejection for RTL blocks both with and without automatic margins.

## Explicit gaps

- The separate immutable adapter invokes imported Stylo for author CSS, selector matching,
  cascade, inheritance, and computed values. Layout itself deliberately contains none of those CSS
  algorithms. Live invalidation, shadow trees, pseudo-element output, and a complete computed-value
  projection remain absent.
- Horizontal left-to-right normal flow only: RTL direction and vertical writing modes fail
  explicitly; no bidi, text shaping, Unicode line breaking, hyphenation, justification, or real
  font metrics. In particular, the ideographic no-space boundary cases in
  `white-space-wrap-after-nowrap-001.html` remain open; the ASCII collapsed-space boundary tests do
  not claim that the complete WPT passes.
- No margin collapse, general intrinsic sizing, right-to-left block-width resolution, floats,
  clearance, positioned layout, overflow/scrolling, fragmentation, columns, transforms, or stacking
  contexts. Percentage block sizes with an indefinite containing-block height follow the current
  auto-size path; broader CSS sizing algorithms and replaced-element constraints remain absent. RTL
  is preserved in computed style and rejected rather than silently entering the LTR solver.
- Flex support is deliberately bounded: no reverse axes, wrap-reverse, inline flex, baseline
  alignment, non-default `align-content`, automatic margins, full min-content/max-content
  contribution and automatic flex-item minimum-size algorithms, aspect-ratio transfer,
  fragmentation, or replaced-element intrinsic sizing. The generic desktop fixture uses an
  ordinary styled box for its field shape; it is not
  evidence for native form-control or replaced-element behavior. There is no grid, table
  formatting, ruby, list-marker, form-control, replaced-element, SVG, Canvas, or media sizing.
- Block descendants of ordinary inline boxes are treated as inline content with an explicit warning
  rather than performing CSS block-in-inline splitting; an inline-block is the bounded exception and
  owns a real block formatting context. Ordinary inline padding/borders also emit a warning and are
  not applied yet. Automatic margins on ordinary inline boxes fail typed; inline-block automatic
  margins have their CSS2 zero used value.
- No painting/display-list conversion, hit testing, selection geometry, accessibility geometry,
  or WebRender resource ownership. The renderer must consume fragments through a later public
  contract; it must not take DOM nodes. W8-A4T admits `BoxKind::Flex` to the bounded renderer
  decoration path, but that does not establish complete CSS painting or a rendered-page parity
  claim.
- Canvas paint remains a solid `background-color` subset. The computed-style contract retains only
  the exact image-list classification needed to decide ESR transparency and an effective-any
  containment fact; it does not retain or render image/gradient payloads or implement containment
  layout/paint effects generally. Native appearance, forced-colors decisions, paged-media canvas
  distinctions, blend modes, and transparent-container policy remain open. These are explicit
  gaps, so this slice does not claim full CSS canvas-background parity.
- General block/inline coordinates still use deterministic saturation without complete overflow
  diagnostics. Inline-block sizing/placement, CSS2 block-width margin assignment, and flex planning
  have typed arithmetic failures; flex also rejects invalid nonnegative geometry.

This package is integrated into the root workspace after DOM. Follow-up work must connect the
imported-Stylo adapter through the root engine pipeline, implement `TextMeasurer` through the
graphics/font owner, and translate immutable fragments into the renderer display-list contract.
