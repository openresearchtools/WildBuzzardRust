# W9-A3M CSS2 block automatic-margin handoff

- Task: W9-A3M — remove the generic horizontal block automatic-margin blocker exposed by the
  public navigation probe
- Owner: Agent 3 — DOM, style, and layout
- Status: accepted after independent correction review GO
- Product target: Linux x86-64; exact fixtures cover 1366×768 and 1920×1080
- Firefox baseline: ESR153 commit `c19b7e89270787889495688244ec6ee8e79288a1`

## Accepted boundary

Imported Stylo remains the CSS parser, cascade, and computed-value owner. The adapter no longer
rejects `margin-*:auto` or substitutes a fabricated length. It publishes each automatic physical
edge through `AutomaticMarginEdges` alongside the existing absolute and percentage components.
Layout alone assigns used values after it has the containing inline size and final constrained
content width.

For an ordinary horizontal, left-to-right, non-replaced block, the used-width equation now has the
following behavior:

- one automatic horizontal side absorbs positive remaining inline space;
- two automatic horizontal sides split positive space, with integer app-unit division assigning an
  odd remainder to the inline end;
- a width reduced by `max-width` participates in the same distribution, so an otherwise automatic
  width can center after constraint;
- negative remaining space leaves the inline-start margin unchanged and adjusts only the
  inline-end margin, which may become negative;
- when neither side is automatic, the existing LTR block path also solves the equation by adjusting
  the inline-end used margin; and
- automatic top and bottom margins have zero used value in normal block flow.

This gate does not add bidi or vertical flow. The existing typed vertical-writing-mode rejection
remains. The correction adds inherited `InlineDirection::{Ltr, Rtl}` to the layout style contract.
Every RTL element now returns
`LayoutError::UnsupportedInlineDirection { node, direction: InlineDirection::Rtl }` during box
construction, before that box is allocated and before layout can publish any fragment. Anonymous
boxes inherit the typed direction through `ComputedStyle::inherit_from`.

## Independent NO-GO and correction

Independent hostile review rejected the first freeze with one medium finding: Stylo's inherited CSS
`direction` was dropped, so an RTL block silently entered the physical-right LTR over-constraint
rule. That could place an over-wide RTL block incorrectly even when it had no automatic margin.

The correction deliberately does not implement partial bidi. The adapter projects Stylo's exact
computed `direction`; layout defaults its bootstrap style to LTR, preserves direction across
inherited and anonymous styles, and rejects RTL during box construction. Separate direct-layout and
real-Stylo regressions cover `direction:rtl; width:220px; margin:auto` and an ordinary RTL block
without automatic margins. Both require the exact node and typed RTL value in the error. An explicit
LTR-over-RTL cascade regression proves LTR projection rather than relying on the initial default.

## Unsupported contexts and failure model

Automatic flex-item margins remain outside the bounded W8 flex algorithm. Any automatic physical
margin on a direct flex item returns
`LayoutError::UnsupportedAutomaticMargin { context: AutomaticMarginContext::FlexItem, .. }` before
flex planning or fragment publication. An automatic margin that would enter the bounded inline
formatter likewise returns the same error with `InlineFormatting`. This includes inline elements
that previously would have emitted only an ignored-edge warning.

Intrinsic outer-size estimation treats automatic edges as zero, matching their contribution policy,
without erasing the computed-style state. Ordinary nested blocks still resolve them during actual
block layout. App-unit assignment that cannot fit the layout representation returns
`LayoutError::BlockWidthArithmeticOverflow`; no partial `LayoutOutput` is returned.

RTL rejection occurs during the same complete box-tree construction pass used by every layout entry
point. A failing descendant may follow internally allocated ancestors, but fragment layout does not
start until the complete tree succeeds, so callers receive only the typed error and no
`LayoutOutput`.

No unsafe code, native boundary, dependency, manifest, lockfile, endpoint, provider rule,
credential, telemetry, or browser-shell exception was added.

## Behavioral evidence

The layout regressions prove:

- exact both-auto centering with an odd app-unit remainder assigned to the right;
- exact one-sided left and right absorption;
- centering after an automatic width is constrained by `max-width`;
- over-wide both-auto and left-auto cases keep the left used margin at zero;
- the internal used margin becomes negative only on the right for negative space;
- vertical automatic margins consume no block space; and
- automatic flex-item and inline margins fail with distinct typed contexts;
- inherited direction survives `ComputedStyle::inherit_from`, the anonymous-style constructor; and
- RTL blocks with and without automatic margins fail before fragment publication with exact typed
  node and direction fields.

The adapter regressions exercise the same contract from real Stylo computed values. A generic
site-shaped fixture uses exactly `width:60vw; margin:15vh auto` and has no provider name, selector,
endpoint, or special case. Its exact app-unit results are:

| Viewport | x | y | width | height |
| --- | ---: | ---: | ---: | ---: |
| 1366×768 | 16,392 (273.2px) | 6,912 (115.2px) | 49,176 (819.6px) | 600 (10px) |
| 1920×1080 | 23,040 (384px) | 9,720 (162px) | 69,120 (1152px) | 600 (10px) |

The previously ignored public probe was not run here because browser and network paths were
explicitly excluded from this lane. Its reported Stylo `AutomaticMargin("margin-right")` blocker is
removed, and the reported target geometry is now covered generically. If that was its only remaining
failure, the probe should pass; at minimum it should now proceed beyond style projection and block
placement without an automatic-margin error.

## Firefox, CSS2, and history evidence

The pinned reference was inspected read-only and is not a build input:

- `layout/generic/ReflowInput.cpp`, especially `CalculateBlockSideMargins`, resolves margins after
  the content width is known, assigns negative remaining space to inline-end, and splits positive
  both-auto space with `start = space / 2` and `end = space - start`; its no-auto branch consults
  the containing block's bidi direction before choosing the ignored logical margin;
- `layout/generic/ReflowInput.h` documents the computed-margin boundary;
- `testing/web-platform/tests/css/CSS2/margin-padding-clear/margin-auto-on-block-box.html` and its
  reference prove that LTR `margin-left:auto` does not become negative while `margin-right:auto`
  may;
- `testing/web-platform/tests/css/CSS2/normal-flow/auto-margins-used-values.html` covers equal and
  one-sided positive distribution;
- `testing/web-platform/tests/css/CSS2/normal-flow/block-non-replaced-width-007.xht` covers ordinary
  both-auto centering; and
- `testing/web-platform/tests/css/CSS2/normal-flow/block-non-replaced-height-001.xht` covers zero
  top/bottom automatic used values.

`git blame` shows the negative-inline-end rule in the predecessor `nsHTMLReflowState.cpp` since
`b48da76c7b556` and the start/end split across its historical logical-edge conversion. History for
the focused WPT identifies `e03182611f37` (Bug 1846856 / WPT PR 41302), whose explicit invariant is
that LTR automatic left margins never become negative. The later file rename is recorded at
`216cc0ba39ed`.

## Verification

All Cargo commands use `--locked`, `CARGO_NET_OFFLINE=true`, target
`x86_64-unknown-linux-gnu`, and external target directory
`/home/user/Documents/wildbuzzardbuilds/w9-a3m-correction`. Adapter generation uses the admitted
Python environment at `/home/user/Documents/wildbuzzardbuilds/w6-a6g/python/bin/python` through
`PYTHON3`.

Toolchain: rustc/cargo 1.96.0 (`ac68faa20`/`30a34c682`), rustfmt 1.9.0-stable, and Clippy 0.1.96.

Final accepted gates:

- explicit-edition `rustfmt --check` over every changed Rust file;
- `cargo metadata --locked --no-deps` for the layout and adapter manifests;
- layout `cargo test --locked --all-targets`: 8 unit and 23 integration tests;
- adapter `cargo test --locked --all-targets`: 1 unit and 34 integration tests;
- layout `cargo clippy --locked --all-targets -- -D warnings`;
- adapter `cargo clippy --locked --all-targets --no-deps -- -D warnings`;
- release builds for both manifests;
- `RUSTDOCFLAGS='-D warnings' cargo doc --locked --no-deps` for both manifests; and
- tracked/new-file whitespace validation with `git diff --check`.

No repository-local `target/` or generated Stylo source was created by these gates.

An independent correction review repeated the source audit and every focused gate in
`/home/user/Documents/wildbuzzardbuilds/w9-a3m-review2`. It reproduced the original RTL probe,
confirmed rejection before box allocation and fragment publication, found no alternate fallback,
and returned GO with no findings.

## Remaining work

This is not complete CSS block layout. Margin collapsing, bidi/RTL layout, vertical writing modes,
replaced blocks, floats, clearance, intrinsic sizing, tables, positioned layout, fragmentation, and
complete overflow diagnostics remain open. RTL is now an honest typed rejection, not supported
geometry. Automatic flex-item margins need a separate bounded Flexbox gate, and
standards-compatible inline used-value zeroing needs an inline-edge implementation; until then those
two contexts intentionally fail typed. Browser-shell and public-site verification belong to their
owning lanes.

## Frozen source hashes

SHA-256 values after the final gates (this self-referential handoff is excluded):

| Path | SHA-256 |
| --- | --- |
| `layout/src/style.rs` | `d3acff0ece68d447e1247b1517501f1ab9608020c1662727cb3c0e9155b05aba` |
| `layout/src/tree.rs` | `e261b0caa29efb5a08275b238bc6c9a984578476a63e9d318c96a1f7431c8f7b` |
| `layout/src/lib.rs` | `54016b25953a6ff9a259c370c28405e03cd7363b254706cc29416bce00edac57` |
| `layout/tests/static_layout.rs` | `9433422c4644ef45e278a7551563f527f1c3cbc654aa7f00627afb98c93e32d1` |
| `layout/README.md` | `0f5647386dbd54b1c65535cdd529aea727d868610efe654c6df2aed29548f7a3` |
| `servo/components/wild_buzzard_stylo_adapter/translate.rs` | `17581a61948d4a7a7595e1bb5db58821bb81f8dc102c6a5611052bbb4c29d584` |
| `servo/components/wild_buzzard_stylo_adapter/error.rs` | `f636783131c7a4660d967864bebe46bfdde171aa339fdbc1417ab71224202d0f` |
| `servo/components/wild_buzzard_stylo_adapter/tests/static_style.rs` | `9b7ab5aa1bde99396666826b4d05e0d59be8e4c64ffcd536993c17acad078196` |
| `servo/components/wild_buzzard_stylo_adapter/README.md` | `c589361d4aa56cf0c81b9564aa04ea916d9427cd0359d742f9a4e51949d35253` |
