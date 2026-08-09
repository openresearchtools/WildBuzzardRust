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

The horizontal block path applies non-intrinsic computed width/height and min/max constraints,
percentage inline sizes, and both `content-box` and `border-box` interpretation. A definite parent
height supplies the percentage basis for child block sizes. CSS minimums win when a minimum exceeds
the corresponding maximum. Vertical writing modes remain typed in `ComputedStyle` and return
`LayoutError::UnsupportedWritingMode` before box construction can fabricate horizontal geometry.

`LayoutLimits` bounds logical recursion during box construction, block layout, and inline layout.
The default maximum is 256. `layout_document_with_limits` permits a caller-selected bound and
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
- `layout/generic/nsInlineFrame.{h,cpp}` for inline continuations/fragments.
- `layout/generic/nsLineLayout.{h,cpp}` for inline-coordinate advancement, line starts, wrapping,
  and baseline placement.
- `layout/generic/nsTextFrame.{h,cpp}` for the text-measurement boundary.
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
- `testing/web-platform/tests/css/css-writing-modes/writing-mode-vertical-rl-003.htm` as behavior
  that must remain an explicit unsupported error until vertical flow exists.

History was inspected with `git log --follow`, including changes around `194f92ebae0e` (baseline
retrieval), `4e0e1888a6eb`/`e562ea4a57c4` (text-indent reflow), and `ed167330ec76` (fragmented block
layout). Those paths define future assertions; this wave intentionally implements a much smaller
Rust formatting model.

## Static-layout tests

`tests/static_layout.rs` exercises the complete parse-to-snapshot-to-layout path, body geometry,
anonymous inline runs around block children, deterministic word and overlong-word wrapping,
whitespace collapse across nested spans, forced `br` lines, custom style-driven suppression and
block/padding geometry, exact computed-style publication, document/revision rejection, percentage
edge resolution against the containing inline size, preferred/min/max block geometry, box sizing,
vertical-writing-mode rejection, and invalid viewport rejection.
Depth tests exercise the default 256-level block boundary, structured box-construction failure at
257, and inline-layout failure when an anonymous box adds logical depth. Whitespace tests cover
NBSP, em space, and collapsible ASCII runs through the full parse-to-layout path.

## Explicit gaps

- The separate immutable adapter invokes imported Stylo for author CSS, selector matching,
  cascade, inheritance, and computed values. Layout itself deliberately contains none of those CSS
  algorithms. Live invalidation, shadow trees, pseudo-element output, and a complete computed-value
  projection remain absent.
- Horizontal left-to-right normal flow only: vertical writing modes fail explicitly; no bidi,
  text shaping, Unicode line breaking, hyphenation, justification, or real font metrics.
- No margin collapse, intrinsic sizing, auto-margin resolution, floats, clearance, positioned
  layout, overflow/scrolling, fragmentation, columns, transforms, or stacking contexts. Percentage
  block sizes with an indefinite containing-block height follow the current auto-size path; broader
  CSS sizing algorithms and replaced-element constraints remain absent.
- No flex, grid, table formatting, ruby, list markers, form controls, replaced elements, SVG,
  Canvas, or media sizing.
- Block descendants of inline boxes are treated as inline content with an explicit warning rather
  than performing CSS block-in-inline splitting. Inline padding/borders also emit a warning and are
  not applied yet.
- No painting/display-list conversion, hit testing, selection geometry, accessibility geometry,
  or WebRender resource ownership. The renderer must consume fragments through a later public
  contract; it must not take DOM nodes.
- Saturated coordinates are deterministic but do not yet emit overflow diagnostics.

This package is integrated into the root workspace after DOM. Follow-up work must connect the
imported-Stylo adapter through the root engine pipeline, implement `TextMeasurer` through the
graphics/font owner, and translate immutable fragments into the renderer display-list contract.
