# Wild Buzzard static layout nucleus

`wild_buzzard_layout` consumes only an owned `DocumentSnapshot`. `StyleResolver` receives immutable
element/snapshot data plus the inherited parent style; it is the replacement point for the
Wild-Buzzard-native Stylo adapter. `TextMeasurer` is the font-system boundary. Neither trait grants
access to mutable DOM, platform APIs, renderer internals, or fonts.

Geometry uses signed `Au` values at 60 app units per CSS pixel with saturating arithmetic and
explicit `Point`, `Size`, `Rect`, `Edges`, and `Viewport` types. The output is a logical box tree
whose boxes own renderer-facing fragments. Block boxes generate anonymous blocks around contiguous
inline runs. The wave-one inline context collapses normal whitespace across nested inline nodes,
honors `pre` newlines, handles `br`, wraps at words and then character boundaries, and represents
multi-line inline elements with multiple fragments.

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

History was inspected with `git log --follow`, including changes around `194f92ebae0e` (baseline
retrieval), `4e0e1888a6eb`/`e562ea4a57c4` (text-indent reflow), and `ed167330ec76` (fragmented block
layout). Those paths define future assertions; this wave intentionally implements a much smaller
Rust formatting model.

## Wave-one tests

`tests/static_layout.rs` exercises the complete parse-to-snapshot-to-layout path, body geometry,
anonymous inline runs around block children, deterministic word and overlong-word wrapping,
whitespace collapse across nested spans, forced `br` lines, custom style-driven suppression and
block/padding geometry, immutable snapshot/revision isolation, and invalid viewport rejection.
Depth tests exercise the default 256-level block boundary, structured box-construction failure at
257, and inline-layout failure when an anonymous box adds logical depth. Whitespace tests cover
NBSP, em space, and collapsible ASCII runs through the full parse-to-layout path.

## Explicit gaps

- No author CSS, selector matching, cascade, inheritance database, invalidation, or Stylo adapter.
- Horizontal left-to-right normal flow only: no bidi, vertical writing modes, text shaping,
  Unicode line breaking, hyphenation, justification, or real font metrics.
- No margin collapse, intrinsic/min/max/percentage sizing, floats, clearance, positioned layout,
  overflow/scrolling, fragmentation, columns, transforms, or stacking contexts.
- No flex, grid, table formatting, ruby, list markers, form controls, replaced elements, SVG,
  Canvas, or media sizing.
- Block descendants of inline boxes are treated as inline content with an explicit warning rather
  than performing CSS block-in-inline splitting. Inline padding/borders also emit a warning and are
  not applied yet.
- No painting/display-list conversion, hit testing, selection geometry, accessibility geometry,
  or WebRender resource ownership. The renderer must consume fragments through a later public
  contract; it must not take DOM nodes.
- Saturated coordinates are deterministic but do not yet emit overflow diagnostics.

This package is integrated into the root workspace after DOM. Follow-up work must implement
`StyleResolver` with the adapted Stylo platform feature, implement `TextMeasurer` through the
graphics/font owner, and translate immutable fragments into the renderer display-list contract.
