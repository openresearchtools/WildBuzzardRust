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
honors `pre` newlines, handles `br`, wraps at words and then character boundaries, and represents
multi-line inline elements with multiple fragments.

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

`LayoutLimits` bounds logical recursion during box construction, block, inline, and flex layout.
The default depth maximum is 256; per-container flex items default to 4,096, lines to 1,024, and
aggregate flex work to 1,000,000 charged units. Each item, line, and redistribution pass is charged
before it mutates planner state, and flex-only reservations and arithmetic have typed failures.
`layout_document_with_limits` permits caller-selected bounds and
returns `LayoutError::TreeDepthLimitExceeded { limit, node_id, phase }`; the original
`layout_document` API remains the default-limits convenience entry point. This applies after an
iterative DOM snapshot, including to script-created trees.

Normal white-space collapsing uses the wave-one CSS set (TAB, LF, FF, CR, and SPACE). NBSP and
other Unicode spaces remain in text fragments for the eventual shaping backend.

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
- `layout/reftests/first-letter/inline-height-empty.html` to identify empty-inline behavior still
  missing here.
- `layout/reftests/abs-pos/continuation-positioned-inline-1.html` to identify continuation and
  positioned-inline behavior still missing here.
- `testing/web-platform/tests/css/css-sizing/min-width-max-width-precedence.html` for minimum-size
  precedence.
- `testing/web-platform/tests/css/css-sizing/box-sizing-content-box-001.xht` and
  `box-sizing-border-box-001.xht` for sizing interpretation.
- `testing/web-platform/tests/css/CSS2/margin-padding-clear/margin-auto-on-block-box.html` and
  `testing/web-platform/tests/css/CSS2/normal-flow/auto-margins-used-values.html` for centered,
  one-sided, and over-constrained automatic margins.
- `testing/web-platform/tests/css/CSS2/normal-flow/block-non-replaced-height-001.xht` for zero used
  values on automatic vertical margins.
- `testing/web-platform/tests/css/css-writing-modes/writing-mode-vertical-rl-003.htm` as behavior
  that must remain an explicit unsupported error until vertical flow exists.
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
paths define future assertions; this wave intentionally implements a much smaller Rust formatting
model.

## Static-layout tests

`tests/static_layout.rs` exercises the complete parse-to-snapshot-to-layout path, body geometry,
anonymous inline runs around block children, deterministic word and overlong-word wrapping,
whitespace collapse across nested spans, forced `br` lines, custom style-driven suppression and
block/padding geometry, exact computed-style publication, document/revision rejection, percentage
edge resolution against the containing inline size, preferred/min/max block geometry, box sizing,
vertical-writing-mode rejection, and invalid viewport rejection.
Canvas tests exercise an exact immutable computed-style publication, canonical HTML-body fallback,
root precedence, inline-body eligibility, `display:none`, transparent absence, source-box identity,
and the invariant that provenance is attached only to the root layout box.
Depth tests exercise the default 256-level block boundary, structured box-construction failure at
257, and inline-layout failure when an anonymous box adds logical depth. Whitespace tests cover
NBSP, em space, and collapsible ASCII runs through the full parse-to-layout path.
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
  font metrics.
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
- Block descendants of inline boxes are treated as inline content with an explicit warning rather
  than performing CSS block-in-inline splitting. Inline padding/borders also emit a warning and are
  not applied yet. Any automatic margin that would enter this bounded inline formatter fails typed
  instead of being silently discarded.
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
  diagnostics. CSS2 block-width margin assignment and flex planning have typed arithmetic failures;
  flex also rejects invalid nonnegative geometry.

This package is integrated into the root workspace after DOM. Follow-up work must connect the
imported-Stylo adapter through the root engine pipeline, implement `TextMeasurer` through the
graphics/font owner, and translate immutable fragments into the renderer display-list contract.
