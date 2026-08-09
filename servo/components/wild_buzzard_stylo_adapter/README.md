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
  box sizing, and writing mode into the current layout crate's smaller computed-style type;
- exact document/revision publication checks and resource diagnostics.

Current explicit gaps include shadow trees, live mutation/invalidation, dynamic CSSOM stylesheet
disabled state, complete UA defaults, real font metrics, animation/transition computation, legacy
presentational hints, container sizes, and computed values that the early block/inline layout model
cannot represent. Event and form state is never inferred from markup; its owner must provide the
validated state publication. Links are deliberately unvisited, with both HTML and SVG `href` plus
legacy SVG `xlink:href` recognized. `line-height: normal` currently uses a documented provisional
1.2× font-size used value. Unsupported display, white-space, auto-margin, intrinsic/anchor sizing,
and complex length-percentage forms fail with a structured error. Vertical writing modes are
projected and layout rejects them explicitly; no fallback CSS engine or horizontal fabrication is
used.

The adapter performs no stylesheet network loading. `@import` is rejected before parsing with no
loader installed, and tests use loopback-free `.invalid` base URLs.

Historical and source-path evidence, the two-word style-sharing storage adaptation, security
notes, and gate commands are recorded in [`../../WILDBUZZARD_ADAPTATION.md`](../../WILDBUZZARD_ADAPTATION.md).
