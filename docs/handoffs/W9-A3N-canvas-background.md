# W9-A3N: generic root/body canvas-background propagation

- Task: W9-A3N — paint the CSS canvas background for ordinary desktop pages
- Owner: Agent 3 layout contract plus the bounded Agent 4 scene-compiler boundary
- Status: **CORRECTED AFTER INDEPENDENT NO-GO — READY REREVIEW**
- Product target: Linux x86-64; exact fixtures cover 1366×768 and 1920×1080
- Corrective gate base: `39beaae7f93799e52c2090c167dadd28112d8460`
- Firefox baseline: ESR153 commit `c19b7e89270787889495688244ec6ee8e79288a1`

## Corrected outcome

Layout publishes one private `CanvasBackgroundDecision` on the root layout box after successful
box construction and fragment layout. The decision is bound to the exact `DocumentVersion` and
seals the root identity/style facts, canonical-body provenance, optional anonymous-wrapper
identity, and optional solid-color paint. Safe callers can read copied facts but cannot construct
or mutate the private identities or decision fields.

Root meaningfulness now projects Firefox ESR153's exact `nsStyleBackground::IsTransparent`
predicate: a background is transparent only when its computed color alpha is zero and its computed
background-image list contains exactly one `none` layer. Thus URL, gradient, and `none, none` roots
are meaningful and win before containment checks. In this bounded color-only gate, an image-only
root blocks body fallback without fabricating an unimplemented image or color primitive; a colored
body remains an ordinary local box background.

Only a known-transparent root with no effective containment may consider the canonical direct
HTML `body` child of an HTML `html` root. A generated inline body remains eligible; a
`display:none`, contained, noncanonical, foreign-namespace, or unrepresented body does not
propagate. Effective containment is true for nonempty computed `contain` or size containment from
computed `container-type`. A meaningful colored root still supplies the canvas even when it is
contained, matching the required root-first ordering. Unknown image or containment facts fail
closed without body fallback.

The renderer revalidates the exact decision before scene construction, inserts a selected solid
color as the first exact-viewport `DocumentCanvas` background, and suppresses ordinary background
paint on every fragment of only that sealed source. Borders, text, descendants, and a body that did
not propagate remain ordinary fragment paint. A transparent default produces no synthetic
rectangle; the existing opaque-white headless/presenter clear remains the product backstop.

## Independent NO-GO corrections

The first review found five material blockers. This corrective pass closes each one:

1. **Alpha-only root meaningfulness:** `BackgroundImageLayers::{SingleNone, Meaningful, Unknown}`
   now carries the exact computed-list distinction. Real Stylo URL, gradient, and multi-`none`
   roots block fallback; exact single `none` permits it.
2. **WPT 008/009 containment:** `EffectiveContainment::{None, Any, Unknown}` now blocks body
   propagation for a contained root or body while retaining meaningful-root precedence.
3. **Missing real-Stylo evidence:** adapter regressions exercise the image-list matrix,
   `contain:paint` on root/body, meaningful contained root, and the positive single-`none` case.
   The `contain` longhand uses dedicated `layout.contain.enabled`; a negative-control
   `counter-increment` declaration proves the shared `layout.unimplemented` sentinel stays off.
4. **Forgeable provenance:** immutable construction identities and complete canonical-body
   provenance now reject missing, same-document non-body, foreign-document, duplicate, moved,
   nested, multiply-parented, and post-layout style transplants before scene construction.
5. **Revision transplant:** the private decision stores the exact `DocumentVersion`. A regression
   mutates both `LayoutOutput.document_version` and the outer `CompileRequest` to the same forged
   revision and still receives `CanvasBackgroundDocumentVersionMismatch` from the inner seal.

The earlier alpha-only implementation and its READY claim are superseded by this handoff.

## Layout, Stylo, and graphics contract

`ComputedStyle` defaults new canvas facts to `Unknown`, so manually incomplete producers cannot
silently enable fallback. `InitialStyleResolver` explicitly publishes CSS initial
`SingleNone`/`None` facts. The production Stylo projection classifies `clone_background_image()` as
`SingleNone` only for a one-entry `Image::None` list; every other represented list is
`Meaningful`. It projects effective containment from computed `contain` and `container-type`
without reparsing CSS.

The completed root decision contains:

- its exact document identity/revision;
- the immutable root `LayoutBoxIdentity` and root canvas-style facts;
- `CanvasBodyProvenance::{NotApplicable, NonGenerating, Unrepresented, Generated}`;
- for a generated body, its immutable identity, canvas-style facts, and exact direct or sealed
  anonymous-inline-wrapper relation; and
- an optional `CanvasBackground` containing the source kind, sealed source identity, and copied
  nontransparent color.

Renderer validation checks private/public box identity agreement, document ownership, uniqueness,
canonical ancestry, unchanged root/body facts, recomputed selection, and paint/source agreement.
The canvas item consumes the same checked scene-item, WebRender-primitive, allocation, and
conservative serialized-list budgets as ordinary rectangles. Validation and construction use the
same selected source, so suppression and resource counts cannot diverge.

## Normal-desktop and hostile evidence

The `#eee` body fixture compiles to the first/bottom WebRender rectangle at both required
viewports, using 60 app units per CSS pixel:

| Viewport | x | y | width | height | RGBA | Source |
| --- | ---: | ---: | ---: | ---: | --- | --- |
| 1366×768 | 0 | 0 | 81,960 | 46,080 | 238, 238, 238, 255 | canonical body |
| 1920×1080 | 0 | 0 | 115,200 | 64,800 | 238, 238, 238, 255 | canonical body |

Focused regressions additionally prove root precedence with a locally painted colored body,
inline-body fallback through a sealed anonymous wrapper, `display:none` exclusion, transparent
absence, half-alpha exactly-once paint, multi-fragment source suppression, scene-limit accounting,
image/unknown/containment no-fallback behavior, and every hostile provenance case listed above.

## Firefox ESR, WPT, and history evidence

The ignored Firefox checkout was read only and remains reference material, never a build input:

- `layout/style/nsStyleStruct.cpp`, `nsStyleBackground::IsTransparent`, supplies the exact
  one-`none` image-count plus alpha predicate;
- `layout/painting/nsCSSRendering.cpp`, especially `FindBackgroundStyleFrame`,
  `FindCanvasBackgroundFrame`, and `FrameHasMeaningfulBackground`, supplies root precedence,
  canonical-body/generated-frame eligibility, containment, and propagated-source suppression;
- `layout/base/PresShell.cpp`, `ComputeSingleCanvasBackground`/`ComputeCanvasBackground`, and
  `layout/generic/nsCanvasFrame.cpp` supply canvas-color composition and bottom paint ordering; and
- DOM `Document::GetBodyElement` supplies the canonical direct HTML-body rule.

Focused WPT references are
`background-color-body-propagation-{001,002,003,004,008,009}.html`: empty-root fallback, meaningful
root precedence, inline-body eligibility, `display:none` exclusion, and root/body paint containment.
History inspection used `git log --follow`, `git blame`, and focused `git show`.
`383bcaba22f46cca4333379e9a9a170c8786e501` records the meaningful-background invariant;
`5c3a09d75cd440b7d584dd90ac947c55f8a2ce03` added the containment/body-propagation guards for Bug
1730763. No Firefox file was edited, fetched, generated, or depended upon.

## Verification

All compilation, test, lint, build, and documentation commands used `--locked`, offline resolution,
target `x86_64-unknown-linux-gnu`, and one reusable external target. Stylo generation used the
exact external Python environment through `PYTHON3`.

- locked/offline `cargo metadata --no-deps` passed for layout, renderer, Stylo adapter, and style
  platform;
- explicit-edition `rustfmt --check` passed for all 12 changed Rust files;
- layout passed 8 unit + 28 integration tests;
- renderer passed 2 unit + 32 integration tests;
- real Stylo adapter passed 1 unit + 37 integration tests;
- style platform passed 2 unit tests;
- strict all-target, no-dependency Clippy passed with `-D warnings` for all four manifests (all
  features for renderer, adapter, and platform);
- release all-target builds passed for all four manifests;
- no-dependency rustdoc passed with `RUSTDOCFLAGS='-D warnings'` for all four manifests; and
- assigned-path `git diff --check` passed.

Toolchain: rustc 1.96.0 (`ac68faa20`), cargo 1.96.0 (`30a34c682`), rustfmt 1.9.0-stable, and
Clippy 0.1.96.

Retained review artifacts (not cleaned pending independent verdict):

| Purpose | Exact path | Bytes after final gates |
| --- | --- | ---: |
| Reusable Cargo target | `/home/user/Documents/wildbuzzardbuilds/w9-a3n-canvas/target` | `4,652,068,799` |
| Stylo build-only Python | `/home/user/Documents/wildbuzzardbuilds/w6-a6g/python` | `11,984,857` |

No repository-local target or generated Stylo source and no additional external fixture, log, or
packaging artifact is retained.

## Deliberate remaining work

This is not complete CSS canvas-background parity. It retains only the exact image-list
classification needed for propagation and does not paint URL/gradient payloads. It retains only an
effective-any containment fact for this decision and does not implement general containment layout
or paint effects. Native appearance, forced colors, blend modes, paged-media/page canvases,
transparent-container policy, dynamic invalidation/repaint, compositor visibility, screenshots,
and broad pixel/reftest parity remain later cross-lane work.

## Scope integrity and frozen hashes

The correction touches only the approved layout, renderer, Stylo-adapter, dedicated contain-pref,
style-platform, README, and handoff paths. It adds no manifest/lock/dependency change, unsafe code,
native boundary, browser/network behavior, endpoint, credential, telemetry, packaging output, or
status-matrix edit. Unrelated live JS work belongs to another owner and was not touched. Nothing
was staged or committed.

SHA-256 values after final gates follow. This self-referential handoff is intentionally excluded;
its hash is supplied in the owner message.

| Path | SHA-256 |
| --- | --- |
| `layout/src/tree.rs` | `9e4d2ec535a37ae778f2385f2efe2e332bca010f1138f9d4da0a70bd164429e3` |
| `layout/src/lib.rs` | `5c46df2cd80fa3bf59536e59937c7a45600ea58e18574af74d302c78fa373d7f` |
| `layout/src/style.rs` | `9b06365fa8a2ff5a5023023599fbe5ea733abb9e36f5fc88cfb6994c77d31b34` |
| `layout/tests/static_layout.rs` | `a64cc8f5d56a0c9d2a440917a8e4ff8236365f993902d600eae6b2d2d485a246` |
| `layout/README.md` | `43ae70cf01c55110416bcb73ca96622f8ce72976507f5d5b57d2a2bed9b7277d` |
| `gfx/wild_buzzard_renderer/src/compiler.rs` | `203b453d39e38468dcb35102712418b6a52edeb1dabd3cb0ac7a553421ba0d18` |
| `gfx/wild_buzzard_renderer/src/contract.rs` | `f70895118db4d61e9421bf646fa367ff060d90b3fc21a3b33e0719390b4d4a5f` |
| `gfx/wild_buzzard_renderer/src/error.rs` | `20bead74947a51dfa1bca51ac7423a5dc129e19d9f7b4fa1518d6eef6055cef5` |
| `gfx/wild_buzzard_renderer/src/lib.rs` | `e4bd98061f30f9ffd73ddcc7c1cce07eae2e8a87d950c9093800553ec4967a43` |
| `gfx/wild_buzzard_renderer/tests/scene_compiler.rs` | `d41319b0742af19e50faa47ff9414c2510f84681bb4edf127a57dfb75d875754` |
| `gfx/wild_buzzard_renderer/README.md` | `8d28adb07bedd157716f750b2c1fa32f314c20fb7341917841a426cdba2c1c2d` |
| `servo/components/wild_buzzard_stylo_adapter/translate.rs` | `f45620817a0e8759298786777bea9b7ae8aaff4a667ad6736f8c978b609802c0` |
| `servo/components/wild_buzzard_stylo_adapter/tests/static_style.rs` | `55086292ef26a984b1654e6fcc26e63104e75bb2c1131a6e31a0e4c020a9c3d5` |
| `servo/components/wild_buzzard_stylo_adapter/README.md` | `1389a1eaa0e3cb21511e786311ea97f4c8a774535d5141574f5ce82a8de6cb50` |
| `servo/components/style/properties/longhands.toml` | `681d6fb10099ab0b95a2cd9f64a62abf4dd438f61fdcab5eac81d09e8502fbfe` |
| `servo/components/wild_buzzard_style_platform/lib.rs` | `2210ac6f3581405968d7fed9dc00574ef8df6ae6d5a3604250b35a0ce950dd87` |
