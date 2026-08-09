# Wild Buzzard Stylo adaptation

Status: standalone workspace for the imported Stylo core plus an immutable-DOM/static-layout
adapter, verified 2026-08-09. This is not a CSS, layout, browser, or Firefox-parity claim.

This directory admits the pinned Stylo Rust core as a real, independently buildable Wild Buzzard
component. The default build executes Stylo's Mako property generator and compiles the imported
property parser, selectors, cascade, computed values, invalidation support, and their Rust support
crates. It does not use Gecko C++ bindings, XPCOM, bindgen, Firefox-generated headers, a toy CSS
replacement, or the read-only `firefox/` checkout as a build input.

## Supported configuration

- Product target: `x86_64-unknown-linux-gnu` only. `servo/.cargo/config.toml` selects that target,
  and `wild_buzzard_style_platform` checks architecture, OS, environment, vendor, 64-bit pointer
  width, and empty target ABI. This excludes musl and the x32 GNU ABI as well as other platforms.
- Workspace MSRV: Rust 1.90, aligned with the root workspace and above the Rust 1.86 floor required
  by locked ICU 2.2 crates. No lower toolchain floor is claimed by this adaptation.
- Default style feature: `wild_buzzard`. It composes the imported Servo-side Stylo algorithms with
  narrow Wild Buzzard atoms, state bits, preferences, thread configuration, and profiler hooks.
- `gecko`, `gecko_debug`, and `gecko_refcount_logging` remain feature names only so the mechanically
  imported, inactive `cfg` branches can be compared with upstream. Enabling any of them produces a
  compile error. Their former dependencies are absent from the active manifests.
- Dormant source for Gecko and other systems remains where preserving the imported snapshot makes
  review practical. No such adapter is in the default dependency graph. Seven Android/macOS/
  Windows platform-media atom literals used only by inactive `gecko/media_features.rs` were pruned;
  cross-platform vendor syntax that remains observable CSS data was not globally renamed.
- Build products, generated properties, generated atoms, Python environments, and documentation
  output are external under task-owned `../wildbuzzardbuilds/agent-3-stylo-*` directories. The
  current adapter-correction gates use `../wildbuzzardbuilds/w2-a3t-stylo-servo`.

## Source and historical evidence

The source baseline is the read-only Firefox ESR153 checkout at commit
`c19b7e89270787889495688244ec6ee8e79288a1`. The existing provenance entry is `Stylo core` in
`docs/upstream-components.toml`; it covers these pinned source paths:

- `servo/components/malloc_size_of`
- `servo/components/selectors`
- `servo/components/servo_arc`
- `servo/components/style`
- `servo/components/style_derive`
- `servo/components/style_traits`
- `servo/components/to_shmem`
- `servo/components/to_shmem_derive`

`malloc_size_of_derive` was additionally imported from
`xpcom/rust/malloc_size_of_derive` at the same pinned commit because the admitted crates require the
derive implementation, not an XPCOM adapter. Its MIT and Apache-2.0 texts are retained verbatim in
that crate. Its `Cargo.toml`, `README.md`, `lib.rs`, and both license files are byte-for-byte copies
of the pinned ESR153 source; Wild Buzzard adaptation notes intentionally live in this document.
The root provenance registry lists this additional source path, the complete active manifest set,
and the local Wild Buzzard adaptation crates.

Three changes from the full reference history informed the native Rust boundary:

- `5e99333e24000f540fd4e74fba4f6c30e7f25b94` (2018-05-28), “Bug 1464834: Remove dead servo
  code.” Its parent retains the historical standalone `servo/components/atoms` and Servo style
  unit-test shape.
- `9c99aa9be348dc9218c8fd358bd347b97f036471` (2025-03-15), “Bug 1953984 - Rename servo_atoms
  crate to stylo_atoms.” This explains the imported crate-name contract used by current Stylo.
- `b2b0fca7ea90c49a419c7b3aaca18d8b0d2b86cf` (2017-07-08), “Avoid overriding the root font
  size from a getDefaultComputedStyle call.” Its movement of root-device updates into restyle
  completion reinforces that an embedding must run `finish_restyle`, not mutate the device during
  generic cascade or project a manually resolved primary style early.

Current Servo commit `9978b489c80eea1c13ff0d7b62f0b9306add2e58` was inspected only as an
architectural comparison. Its style wrappers are coupled to Servo's live script DOM, GC/rooting,
layout ownership, and unsafe self-referential access patterns, so none of that source was copied.
Wild Buzzard instead accepts an owned immutable snapshot and maintains adapter-local side data.

The following pinned tests were inspected for observable parser, CSSOM, cascade, and selector
expectations. The Wild Buzzard tests below cover focused analogous assertions; they are not a port
or execution of the complete upstream files:

- `layout/style/test/test_cascade.html`
- `layout/style/test/test_priority_preservation.html`
- `layout/style/test/test_grid_shorthand_serialization.html`
- `layout/style/test/test_at_rule_parse_serialize.html`
- `layout/style/test/test_font_family_parsing.html`
- `layout/style/test/test_namespace_rule.html`
- `layout/style/test/test_font_face_cascade.html`
- `layout/reftests/css-selectors/attr-case-insensitive-1.html`
- `layout/reftests/css-selectors/nth-child-1.html`
- `layout/reftests/css-selectors/nth-child-2.html`
- `layout/reftests/css-selectors/state-dependent-in-any.html`
- `testing/web-platform/tests/css/selectors/has-basic.html`
- `testing/web-platform/tests/css/selectors/has-relative-argument.html`
- `testing/web-platform/tests/css/selectors/not-links.html`
- `testing/web-platform/tests/css/selectors/pseudo-enabled-disabled.html`
- `testing/web-platform/tests/css/css-values/rem-unit-root-element.html`
- `testing/web-platform/tests/css/css-values/rem-root-font-size-restyle-1.html`
- `testing/web-platform/tests/css/css-sizing/min-width-max-width-precedence.html`
- `testing/web-platform/tests/css/css-sizing/box-sizing-content-box-001.xht`
- `testing/web-platform/tests/css/css-sizing/box-sizing-border-box-001.xht`
- `testing/web-platform/tests/css/css-writing-modes/writing-mode-vertical-rl-003.htm`
- `testing/web-platform/tests/svg/linking/scripted/xlink-href-compat.html`
- `dom/html/HTMLStyleElement.cpp`
- `testing/web-platform/tests/css/cssom/style-sheet-interfaces-001.html`
- `testing/web-platform/tests/html/semantics/document-metadata/the-style-element/style_type_html.html`

The last two establish that `HTMLStyleElement.disabled` is CSSOM state on the associated sheet,
not a reflected content attribute. An immutable markup snapshot has no such dynamic bit, so a
literal `<style disabled>` remains enabled here. A future live-document snapshot must carry the
actual stylesheet-disabled state explicitly rather than infer it from markup.
The style `type` comparison is likewise exact: empty or ASCII-case-insensitive `text/css` applies,
while surrounding whitespace and MIME parameters do not. Stylesheet source uses descendant node
text content, matching `HTMLStyleElement::GetInnerHTML`/`GetNodeTextContent` even for a
programmatically constructed tree that ordinary HTML parsing would not produce.

## Adaptation inventory

`servo/Cargo.toml` and `servo/Cargo.lock` define the canonical nested workspace. All workspace path
dependencies resolve within `servo/components`; Cargo metadata contains no path into `firefox/`.
The lock contains 130 packages. Target-specific package metadata may mention `windows-sys`, but the
active default Cargo tree has no Windows adapter. Its native platform dependency is the Rust
`libc` crate reached indirectly by standard threading/synchronisation crates on Linux.

The imported generator remains the build path. `components/style/build.rs` invokes
`components/style/properties/build.py` with Mako. It writes `properties.rs`, CSS-property JSON, and
CSS-property HTML into Cargo `OUT_DIR`, and also writes the JSON/HTML pair into the target-level
`doc/stylo/` directory calculated from `OUT_DIR`. Both locations are below the external
`CARGO_TARGET_DIR`; the earlier claim that every file went only to `OUT_DIR` was inaccurate. The
verified debug build generated 107,660 lines (4,623,422 bytes) of Rust with 256 longhands and 69
shorthands. It is compiled into the normal `style` crate; no checked-in generated substitute
exists. The Python requirements are pinned in `style-build-requirements.txt`.
`PYTHONDONTWRITEBYTECODE=1` prevents Python caches in the live tree.

The Wild Buzzard platform crates are deliberately small:

- `wild_buzzard_stylo_atoms` generates a `string_cache` static atom set from 173 audited strings
  and exports the compile-time `atom!` contract expected by Stylo. The removed platform-only
  literals are `-moz-in-android-pip-mode`, the three `-moz-mac-*` entries, and the three
  `-moz-windows-*` entries that had no active Linux-profile consumer.
- `wild_buzzard_web_atoms` provides distinct local-name, namespace, and prefix atom domains plus
  standards namespace macros.
- `wild_buzzard_style_platform` provides the exact ESR153 `ElementState` and `DocumentState` bit
  layout, Linux thread-count and local-profiler hooks, and typed release preference values.
- `wild_buzzard_style_prefs` preserves the imported `static_prefs::pref!` call shape. Unknown
  preference names fail compilation instead of acquiring a silent default.
- `wild_buzzard_stylo_adapter` owns an immutable `DocumentSnapshot`, precomputes bounded atom and
  relationship tables, implements Stylo's generic DOM traits without a Gecko/XPCOM wrapper, runs
  the imported `Stylesheet`, `Stylist`, and `StyleResolverForElement` paths, and publishes an exact
  document/revision-matched layout style snapshot. It does not implement CSS parsing, selector
  matching, specificity, cascade, inheritance, or computed-value semantics.

The state bits were checked against `dom/base/rust/lib.rs` at the pinned revision. Of the 54
compile-time preference mappings, 47 mirror ESR153 `modules/libpref/init/StaticPrefList.yaml` and
seven are explicit Wild Buzzard/Servo release policy: `layout.threads`, columns, container queries,
marker restriction, grid, variable fonts, and writing mode. The imported
`layout.unimplemented` property-metadata sentinel is compiled directly to `false` by the property
generator and is not exposed as a preference key. Unknown actual keys therefore still fail
compilation. This facade is a compile-time release policy, not a preference service.
`global_style_data.rs` now uses only the Unix pthread handle on the supported target, reads the Wild
Buzzard thread policy, and calls local profiler hooks. The build manifest removes Gecko bindgen,
`mozbuild`, `nsstring`, Gecko profiler, `thin-vec/gecko-ffi`, absent Servo config/atom paths, and
Firefox-generated preference paths.

The Servo URL representation remains the real parsed `url::Url`. Attempting to clone it into the
imported shared-memory form now returns an explicit error because the future typed IPC stylesheet
URL contract is not implemented. A bounded-builder regression test confirms that this boundary
returns the structured error without panicking or consuming buffer space.

## Architecture boundary and integration handoff

The product-wide contract in `docs/architecture/static-page-slice.md` requires this flow:

```text
immutable revisioned DOM snapshot
  -> Wild Buzzard Stylo adapter
  -> immutable computed-style snapshot with the same revision
  -> layout
```

The static adapter now provides concrete `TNode`, `TElement`, and `TDocument` implementations over
an owned immutable Wild Buzzard snapshot. Its `TShadowRoot` type is uninhabited, so shadow-only
matching fails closed until a real shadow-tree snapshot contract exists. Adapter handles carry the
adapter identity and node-table index; opaque addresses remain transient matcher equality tokens
whose referenced boxed records live for the complete synchronous style preparation. No address is
dereferenced, serialized, or published. The computed result owns native Stylo `ComputedValues`
and a smaller layout-facing projection, both tied to the exact input document and revision.

Interaction and form state is a separate sparse immutable publication carrying that same document
identity and revision. The adapter validates element identity, rejects contradictory state pairs,
and never infers event/form state from attributes. Visited state is deliberately unavailable;
recognized HTML and SVG links are always cascaded through Stylo's unvisited path. HTML `href`, SVG
`href`, and legacy SVG `xlink:href` participate in `:link`/`:any-link` matching.

The imported style-sharing TLS erases the concrete `TElement` type into `FakeCandidate`. Upstream's
Servo product handle is one word; the safe immutable Wild Buzzard handle is two words because it
must distinguish both its owning adapter and its index. A one-word implementation would require a
self-referential arena, leaked/global lookup state, or a raw-pointer back-reference. Those designs
are prohibited for this boundary. The sole imported algorithm-tree delta for this slice is thus a
`cfg(feature = "wild_buzzard")` two-`usize` storage slot in
`components/style/sharing/mod.rs`. The existing runtime size and alignment assertions remain and a
focused regression constructs the cache. No cache, matching, parsing, cascade, inheritance, or
computed-value behavior changed.

The adapter resolves all element styles in parent-before-child order, stores them in Stylo's
`ElementData`, and calls the imported `MatchMethods::finish_restyle` hook. The root call updates the
device's root style, font size, line height, and metrics before descendant `rem`/`rlh` values are
cascaded. It does not duplicate or post-correct Stylo's unit computation. Thread setup is idempotent
for an uninitialized or already layout-capable thread; a thread initialized only for another Stylo
role returns a structured error instead of triggering imported initialization panic.

The deterministic font provider is intentionally provisional. It lets Stylo compute ordinary CSS
lengths without opening fonts; layout uses an explicitly documented 1.2× used value for
`line-height: normal`. The projection now carries width, height, physical min/max sizes, box sizing,
and writing mode. Horizontal block layout applies non-intrinsic sizes; vertical modes are retained
and rejected explicitly. Real font metrics and several computed display/white-space/value forms
are still integration gaps and fail with structured errors rather than fabricated layout values.

The public seams already exercised here are:

- `style::properties::parse_style_attribute` and generated `PropertyId`/`LonghandId` tables;
- the actual default `style::selector_parser::SelectorParser`, including Firefox-facing
  `:has()` and `:nth-child(... of ...)` syntax;
- generic `selectors::parser::SelectorList` plus `selectors::matching::matches_selector_list` over
  a safe immutable test implementation of `selectors::tree::Element`;
- Stylo `Device`, `Stylist`, `StyleBuilder`, custom-property builder, priority passes, and generated
  cascade functions producing `ComputedValues`.
- Stylo `Stylesheet::from_str`, `MediaList::parse`, `Stylist::append_stylesheet`/`flush`,
  `parse_style_attribute`, `StyleResolverForElement::resolve_style`, and
  `MatchMethods::finish_restyle` over the concrete immutable DOM adapter.

The imported Servo product parser returned `false` from `parse_has` and `parse_nth_child_of`, while
the pinned ESR153 Gecko parser returns `true` for both. Wild Buzzard enables both because the shared
parser, matcher, selector flags, dependency collector, and relative-selector invalidation core are
present. Actual default-parser tests cover accepted and malformed syntax. The separate matcher
tests use `selectors::parser::tests::DummyParser` only to isolate generic matcher behavior; they are
not presented as a second product parser. Live mutation/invalidation still requires the future DOM
adapter to implement all `TElement` selector-flag and traversal contracts and needs WPT coverage.

The concrete immutable adapter now reaches the full public resolver and generated cascade path.
The earlier no-DOM cascade tests remain useful isolated coverage of declaration iteration,
custom-property resolution, priority/non-priority generated cascade passes, logical mapping,
inheritance, viewport resolution, and the `ComputedValues` builder. Mutation snapshots and live
incremental restyle traversal remain future work.

## Conformance and regression evidence

Focused Wild Buzzard coverage:

| Location | Tests | Observable behavior |
| --- | ---: | --- |
| `components/style/tests/wild_buzzard_properties.rs` | 4 | Generated shorthand expansion, longhand CSSOM access, source order, `!important`, invalid-property rejection, colour normalisation, custom properties, `var()`, `calc()`, CSS-wide shorthand serialization, and parse/serialize/reparse |
| `components/style/tests/wild_buzzard_selectors.rs` | 2 | Actual default Wild Buzzard `style::SelectorParser` accepts compounds, combinators, `:is`, `:not`, `:has`, and `:nth-child(... of ...)`, and rejects malformed Level 4 forms |
| `components/style/tests/wild_buzzard_boundaries.rs` | 1 | Bounded `SharedMemoryBuilder` proves unsupported stylesheet-URL transfer returns the exact structured error without panic or buffer consumption |
| `components/style/properties/cascade.rs` | 2 | Real generated cascade into `ComputedValues`: custom-property substitution, priority groups, font size, colour, padding, viewport units, RTL logical mapping, parent inheritance, percentage font size, and inherited custom properties |
| `components/selectors/matching.rs` | 2 | Lower-level generic matcher capability using the selectors crate's explicitly broad `DummyParser`: type/id/class compounds, combinators, structural/state pseudos, `:has`, and `:nth-child(... of ...)` |
| Wild Buzzard platform/atom crates | 7 | Compile-time exact-target assertion, state-bit stability, typed preferences, static/dynamic atom equality, exported atom macro, standards namespaces, local-name domain, and preference re-export |
| `components/wild_buzzard_stylo_adapter` | 20 | Two-word safe-handle sharing-cache contract; full root restyle completion and descendant `rem`; type/id/class/attribute/structural/relative/state matching; exact-revision state validation; HTML/SVG links with visited privacy; author/inline/important/inherited cascade; UA and author `display:none`; media/type handling and descendant stylesheet text; colors, fonts, edges, sizing, box sizing, writing-mode rejection, and geometry; bounded diagnostics/import/resource failures; stale/wrong-document rejection; thread-role safety; fail-closed unsupported and shadow-only pseudos |

The corrected complete workspace reports 64 passing tests and 3 ignored imported documentation
examples, with zero failures. Thirty-eight passing tests are the focused rows above; the remaining 26
are imported support/style regression tests. The `shadow_parts` test correction made its
expectation agree with both the parser and its other regression case: a single token is a valid
self-mapping.

This evidence does not cover live DOM invalidation/traversal, a complete production UA sheet, full
CSS/WPT/reftest corpora, font discovery/shaping, mature layout, pixels, WebDriver, accessibility,
or browser-product behaviour.

## Dependency, native-code, and unsafe audit

`cargo tree -p style --edges normal,build --locked` shows Rust crates from this workspace and the
locked registry graph. The active build contains no `bindgen`, `clang-sys`, `cc`, C++, XPCOM,
`nsstring`, `mozbuild`, Gecko profiler, or Firefox-generated input. There is no direct native
library link introduced by this adaptation. Mako/MarkupSafe/TOML run only in the external Python
build environment and are not linked into the browser runtime; MarkupSafe may use its ordinary
Python wheel accelerator in that build environment.

The pre-existing platform shims have `#![forbid(unsafe_code)]`. The immutable adapter contains no
unsafe block, raw-pointer dereference, or transmute. Five `TElement` methods must retain `unsafe fn`
signatures imposed by the imported trait; each has a local ownership/exclusive-preparation safety
contract and a safe body. The imported core deliberately contains
auditable unsafe implementation that cannot be relabelled as a safe rewrite. The important active
categories are:

- `servo_arc`: raw allocation pointers and atomic reference-count ownership;
- `to_shmem`: caller-provided shared-memory buffer allocation, pointer arithmetic, and placement;
- `style/shared_lock.rs`: `UnsafeCell` access tied to read/write guard invariants;
- `style/rule_tree`: manual reference counts and free-list reclamation;
- generated property code: generated enum/function dispatch and tightly scoped internal unsafe;
- `malloc_size_of`: unsafe external callback types supplied by the embedding process.

`wild_buzzard_boundaries.rs` contains one test-only `unsafe` constructor call for
`SharedMemoryBuilder`; its `SAFETY` comment ties the pointer and capacity to a live, uniquely
borrowed 64-byte array. The tested `UrlExtraData::to_shmem` adaptation returns before allocating.
This does not change or mask the imported allocator's separate programmer-invariant panics:
`SharedMemoryBuilder` still asserts bounds/alignment and may panic if a caller supplies insufficient
capacity or violates its unsafe constructor contract.

The only direct `extern "C"` import found in the inspected style source is in the inactive Gecko
rule-tree leak checker under `gecko_refcount_logging`, a feature that is compile-time rejected. The
Linux thread handle uses Rust's standard-library Unix extension trait rather than a new FFI module.
This is a source audit, not Miri, sanitizer, or formal proof coverage.

## Licensing, privacy, and branding audit

- Imported Stylo/support source retains its MPL-2.0 headers and authorship. The added derive crate
  retains exact MIT and Apache-2.0 license files from the pinned reference.
- New adaptation source uses MPL-2.0 headers and Wild Buzzard package names. It introduces no
  Firefox artwork, application identity, profile identity, or affiliation claim.
- The shims and active manifests contain no telemetry, Glean/FOG, studies, crash upload, remote
  configuration, service credentials, search defaults, or provider endpoint.
- The only URL literals in the shims are standards namespace identifiers, license text, repository
  metadata, and `.invalid` test URLs. Property generation and the style runtime perform no network
  request. Cargo/pip dependency acquisition is build setup, outside runtime behaviour.
- A scan after generation found no `__pycache__` directory or `.pyc` file under `servo/`.

## Reproduction and gate results

Create the build-only Python environment outside the repository:

```sh
python3 -m venv /home/user/Documents/wildbuzzardbuilds/agent-3-stylo-wave2/python
/home/user/Documents/wildbuzzardbuilds/agent-3-stylo-wave2/python/bin/pip \
  install -r servo/style-build-requirements.txt
```

Run Cargo commands from `servo/` with:

```sh
export CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w2-a3t-stylo-servo
export PYTHON3=/home/user/Documents/wildbuzzardbuilds/agent-3-stylo-wave2/python/bin/python
```

Verified commands:

```sh
cargo check --workspace --all-targets --locked
cargo clippy \
  -p wild_buzzard_stylo_atoms \
  -p wild_buzzard_web_atoms \
  -p wild_buzzard_style_platform \
  -p wild_buzzard_style_prefs \
  --all-targets --locked --no-deps -- -D warnings
cargo clippy -p wild_buzzard_stylo_adapter \
  --all-targets --locked --no-deps --target x86_64-unknown-linux-gnu -- -D warnings
cargo test -p style --test wild_buzzard_properties --locked
cargo test -p style --test wild_buzzard_selectors --locked
cargo test -p style --test wild_buzzard_boundaries --locked
cargo test -p style --lib wild_buzzard_tests --locked
cargo test -p selectors wild_buzzard_matching_capability_tests --locked
cargo test -p wild_buzzard_stylo_adapter \
  --locked --target x86_64-unknown-linux-gnu
cargo test --workspace --locked
cargo build --workspace --release --locked
RUSTDOCFLAGS='-D warnings' cargo doc \
  -p wild_buzzard_stylo_atoms \
  -p wild_buzzard_web_atoms \
  -p wild_buzzard_style_platform \
  -p wild_buzzard_style_prefs \
  --no-deps --locked
RUSTDOCFLAGS='-D warnings \
  -A rustdoc::broken-intra-doc-links \
  -A rustdoc::bare-urls \
  -A rustdoc::invalid-html-tags \
  -A rustdoc::invalid-rust-codeblocks' \
  cargo doc --workspace --no-deps --locked
```

All commands above pass. The four explicit full-workspace rustdoc allowances are imported snapshot
debt; the new Wild Buzzard crates pass strict rustdoc without them. Similarly, strict Clippy is
applied to the new shims with `--no-deps`. A direct strict-Clippy attempt selecting only the three
new `style` integration-test targets still causes Cargo to lint the imported `style` library and
reports its pre-existing snapshot debt (2,625 errors on this toolchain); that command is not claimed
as passing or treated as a reason to mechanically reformat the import. The normal all-target check
and full test/release builds still compile every admitted workspace crate.

Formatting was checked with direct `rustfmt --check --config skip_children=true` on `build.rs` and
the new Wild Buzzard test/shim Rust sources. The imported snapshot, including the focused adapted
files, predates current rustfmt output and a workspace formatter would create a large mechanical
diff. The byte-identical `malloc_size_of_derive` import is intentionally not reformatted. No
root/workspace-wide formatter was run.

Those are deliberately default-feature gates. For `style`, the exact default is
`wild_buzzard -> servo + wild_buzzard_style_platform + wild_buzzard_style_prefs`; no Gecko feature
is enabled. `--all-features` is therefore an asserted negative gate, not a positive build mode:

```sh
if cargo check -p style --all-targets --all-features --locked \
  > /home/user/Documents/wildbuzzardbuilds/w2-a3t-stylo-servo/gecko-negative-gate.log 2>&1
then
  exit 1
fi
rg -q 'Gecko property generation is prohibited' \
  /home/user/Documents/wildbuzzardbuilds/w2-a3t-stylo-servo/gecko-negative-gate.log
```

This negative gate passes: selecting all features necessarily selects prohibited Gecko generation,
and the build script rejects it before compiling `build_gecko.rs` or resolving Gecko build inputs.

## Embedding defaults that block parity-grade layout

The active Servo profile still contains fallback values that are buildable but are not accepted
browser behavior. They must be replaced and validated before this path supplies parity-grade
layout or pixel evidence:

- `components/style/device/servo.rs::calc_line_height` computes `line-height: normal` as `0px`.
  Agent 4's font-metrics work and the Agent 3 device adapter must provide and consume real used
  metrics before text layout.
- `Device::scrollbar_inline_size` returns `0px`. Agent 1 must expose Linux theme/widget metrics and
  Agent 3 must wire them into Stylo before scrollbar/environment-dependent layout.
- `Device::is_dark_color_scheme` always returns `false`, so system colours use the light branch
  even when the device preference says dark. Agent 1 must provide the effective Linux colour
  scheme and Agent 3 must test device updates/invalidation.
- Servo `get_content_preferred_color_scheme` hardcodes `light`, while its `lnf_int!` fallback makes
  every GTK/overlay look-and-feel environment value `0`. Agent 1 owns the provider; Agent 3 owns
  typed propagation, CSS environment exposure, and invalidation.
- `driver.rs::should_report_statistics` is always false and Servo `report_statistics` is an
  `unreachable!` path. Agent 3 must either connect a local diagnostics sink or keep statistics
  reporting explicitly unavailable and prove no product option reaches that path.

The first four are style/layout correctness blockers, not supported defaults. The statistics item
is a traversal-diagnostics gap rather than a computed-style semantic, but it must be resolved before
exposing product diagnostics.

## Known gaps and fail-closed behaviour

- The immutable `DocumentSnapshot` adapter implements the Stylo DOM traits and exercises the public
  element-aware resolver synchronously. Live mutation snapshots, parallel traversal, incremental
  invalidation, animation state, and shadow DOM are not yet integrated.
- The DOM snapshot does not yet carry document quirks mode, so this static adapter explicitly uses
  `QuirksMode::NoQuirks` for parsing, the device, and resolution. Quirks and limited-quirks documents
  require a typed snapshot field and focused compatibility tests; a quirks sheet alone is not enough.
- The adapter publishes primary element styles only. Pseudo-element computed styles and generated
  content are not projected into layout. Visited-link styling is disabled. Dynamic interaction and
  form state can be supplied through an exact-revision side snapshot; absent state remains false
  instead of being synthesized from markup. Shadow-only matching remains unavailable.
- The default parser now accepts `:has()` and `:nth-child(... of ...)`, and the generic matcher is
  covered on an immutable test tree. Live DOM mutation and relative-selector invalidation remain
  untested and are not a product-conformance claim.
- An owned, document- and revision-matched computed-style snapshot is connected to the current
  static layout boundary. It is not yet connected through the complete loader-to-renderer product
  pipeline. The projection carries non-intrinsic physical width/height/min/max sizes, box sizing,
  and writing mode; intrinsic/anchor sizing and complex calculations reject explicitly, and
  vertical writing mode returns a layout error rather than horizontal geometry.
- Preferences are typed compile-time release values. Runtime profile preferences, invalidation on
  change, policy ownership, and user configuration are absent.
- Profiler registration hooks are intentional local no-ops until a privacy-preserving diagnostics
  service exists.
- Cross-process stylesheet URL transfer returns a structured error. It must remain disabled until
  a versioned typed IPC representation preserves URL/base/principal semantics.
- The 173-string static atom inventory has no automated upstream-sync/audit tool yet.
- Production font metrics, system fonts/colours, viewport/device updates, a complete UA sheet, user
  stylesheets, quirks sheets, and browser chrome style policy are not integrated. The current
  temporary UA sheet is parsed and cascaded by Stylo but covers only the early static-layout slice.
- The generated property universe is the pinned Servo-side configuration of current Stylo, not the
  complete Gecko product configuration. Full WPT, CSS, reftest, fuzz, and differential suites are
  required before any compatibility claim.
- Inactive Gecko source remains for provenance and comparison but cannot be selected. Removing it
  can be a later mechanically reviewed source-pruning change after the Wild Buzzard contracts are
  stable.
- AppImage, Wayland/X11, headless rendering, and product integration are owned by later vertical
  slices; this workspace neither builds nor claims those artifacts.
