# Wild Buzzard Stylo adapter

This crate does **not** rewrite Stylo. It connects an owned, immutable
`wild_buzzard_dom::DocumentSnapshot` to the imported Firefox ESR153 Rust Stylo engine.

Reused from Stylo without parallel implementations:

- stylesheet and inline-declaration parsing;
- media-query parsing and evaluation;
- selector parsing and matching;
- specificity, origins, source order, `!important`, inheritance, and cascade;
- generated property code and native `ComputedValues`.

New Wild Buzzard code is limited to:

- bounded `TNode`, `TElement`, and `TDocument` views over the immutable snapshot;
- an uninhabited no-shadow-root boundary for this static slice;
- deterministic device/font inputs;
- extraction of HTML `<style>` text and CSSOM-compatible static metadata;
- full primary/eager-pseudo resolution followed by Stylo's restyle-completion hook, including root
  font/line-height device propagation before descendants are cascaded;
- an optional sparse interaction/form-state publication tied to the same document revision;
- a loss-checked projection of display, edges, colors, fonts, white space, width/height/min/max,
  box sizing, writing mode, inherited inline direction, preserved automatic-margin edge state,
  canvas-relevant background-image/containment facts, and the bounded flex longhands into the
  current layout crate's smaller computed-style type;
- exact document/revision publication checks and resource diagnostics.

Current explicit gaps include shadow trees, live mutation/invalidation, dynamic CSSOM stylesheet
disabled state, complete UA defaults, real font metrics, animation/transition computation, legacy
presentational hints, container sizes, and computed values that the early block/inline layout model
cannot represent. Event and form state is never inferred from markup; its owner must provide the
validated state publication. Links are deliberately unvisited, with both HTML and SVG `href` plus
legacy SVG `xlink:href` recognized. `line-height: normal` currently uses a documented provisional
1.2× font-size used value. Unsupported display, white-space, intrinsic/anchor sizing, and complex
length-percentage forms fail with a structured error. `margin-*:auto` is projected as typed edge
state rather than fabricated as zero; ordinary horizontal blocks resolve it during used-width
layout. Automatic margins on flex items or in the bounded inline formatter reach layout and fail
with a typed context error. Vertical writing modes are projected and layout rejects them explicitly;
the inherited `direction` property is also projected, with RTL rejected as
`LayoutError::UnsupportedInlineDirection` before box or fragment publication. No fallback CSS
engine, forced LTR substitution, or horizontal fabrication is used.

For the bounded canvas-background decision, projection follows ESR153's computed-value predicate:
the image list is `SingleNone` only when it has exactly one `Image::None`; URL, gradient,
multi-layer `none`, and every other represented list are `Meaningful`. Effective containment is
`Any` when computed `contain` is nonempty or computed `container-type` establishes size
containment, and is otherwise `None`. The `contain` longhand uses the dedicated enabled
`layout.contain.enabled` product preference; the shared `layout.unimplemented` sentinel remains
disabled, with a real-parser regression proving an unrelated `counter-increment` declaration is
still rejected. These are decision facts only: this adapter does not implement image painting or
general containment layout/paint effects.

The flex projection passes Stylo's computed values directly into typed layout values for row and
column direction, nowrap/wrap, basis, grow/shrink factors, justification, item/self alignment,
gaps, and order. Inline flex fails as `UnsupportedComputedValue::Display`. Reverse axes,
wrap-reverse, baseline/safety alignment forms, non-default `align-content`, unsupported intrinsic
bases, nonlinear gaps, and out-of-range fixed factors fail as `UnsupportedComputedValue::Flex`;
the adapter never reparses author CSS or silently substitutes a nearby flex value. Flex-item
automatic margins remain outside the admitted flex algorithm and fail as
`LayoutError::UnsupportedAutomaticMargin { context: FlexItem, .. }`.

The adapter performs no stylesheet network loading. `@import` is rejected before parsing with no
loader installed, and tests use loopback-free `.invalid` base URLs.

Historical and source-path evidence, the two-word style-sharing storage adaptation, security
notes, and gate commands are recorded in [`../../WILDBUZZARD_ADAPTATION.md`](../../WILDBUZZARD_ADAPTATION.md).
