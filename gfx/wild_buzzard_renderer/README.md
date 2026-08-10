# Wild Buzzard renderer boundary (W2-A4)

`wild_buzzard_renderer` is the first safe Rust boundary from immutable
`wild_buzzard_layout::LayoutOutput` to graphics. It validates one exact typed document version
(document identity plus local revision), creates an immutable renderer-owned scene, and serializes
the supported primitives with the real imported `webrender_api::DisplayListBuilder`.

This is a bounded integration slice, not a rendering-parity claim. It does not submit a scene to a
GPU renderer or produce pixels yet.

## Contract

The initial compile entry point is:

```text
&LayoutOutput + expected DocumentVersion + PipelineKey
    -> validated renderer-owned Scene
    -> (PipelineId, webrender_api::BuiltDisplayList)
```

Compilation borrows layout and copies only app-unit geometry, computed colors, border widths, text
metadata, and integer diagnostic box IDs. The resulting scene contains no DOM node, layout-box
reference, pointer, callback, or mutable view into layout. Its vectors and strings are private and
exposed only through shared references. `CompiledScene::into_webrender` consumes the boundary when
a later renderer owner is ready to submit it.

Text composition is a second checked phase:

```text
complete canonical SceneTextDescriptor inventory
    -> exact document/text/metric validation
    -> opaque ValidatedTextMap
    -> actual renderer namespace + namespace-checked font keys + line-local shaped glyphs
    -> immutable ResolvedTextSet
    -> one rebuilt Scene and BuiltDisplayList in original paint order
```

There is no unchecked `ResolvedTextSet` constructor. Resolution is explicitly bound to an
`IdNamespace` and rejects every `FontInstanceKey` from another namespace. A composed scene retains
that namespace, and the headless owner checks it against the actual `RenderApi` before submission.
This proves namespace consistency, not that an arbitrary same-namespace scalar key is a member of a
live font registry; the text registry remains the authority that generates and owns keys. Only the
freshly compiled pending scene is renderer-neutral.

Each successful compilation also receives a private, process-local, non-reusing resolution
identity from a checked `AtomicU64` allocator. That identity is carried opaquely through
`ValidatedTextMap`, `TextResolutionBuilder`, and `ResolvedTextSet`; composition checks it before
examining or changing scene items. Consequently a resolution cannot be rebound even to a separately
compiled scene with the same `DocumentVersion`, pipeline, item IDs, geometry, and namespace.
Identity exhaustion fails compilation explicitly and the counter never wraps or reuses a value.

The compiler preserves:

- the exact DOM-owned `document_version`, viewport, and content size;
- deterministic parent-before-child preorder and each box's fragment order;
- stable sequential scene-item, source-box, and pending-text IDs;
- non-transparent block/inline/flex-container backgrounds;
- top/right/bottom/left border geometry, provisionally as solid `currentColor` borders because the
  current layout contract has widths but no computed border style, per-side color, or radius;
- exact UTF-8 text, bounds, baseline, computed color, font size, and line height at a typed
  `PendingTextRun` boundary.

Anonymous layout blocks, text boxes, and line-break boxes do not paint box decorations. Every
supported background and border becomes an actual WebRender `Rectangle` or `Border` item. Pending
text deliberately does not become a WebRender `Text` item during initial compilation. After exact
shaped allocations are matched, every pending item is replaced in place by its real font instance
and glyph runs. Glyph Y is line-local and already contains Parley's `first_baseline`; composition
adds only the fragment top. Font ascent is never substituted as a placement coordinate.

## Validation and allocation bounds

Input is rejected with `SceneBuildError` before it can cross the graphics boundary when it has:

- a mismatched document identity/revision or invalid pipeline sentinel;
- a missing/misidentified root or child, multiple parents, a cycle, unreachable boxes, or children
  on leaf-only boxes;
- negative dimensions/edges, overflowing rectangle endpoints, geometry beyond the configured
  range, invalid font metrics/baselines, or a non-finite WebRender conversion;
- text on a non-text box or text without a baseline;
- excessive boxes, child references, fragments, scene items, tree depth, individual/aggregate text
  bytes, or WebRender bytes.
- wrong-version, missing, duplicate, unknown, or out-of-order shaped text; text/metric mismatch;
  cross-scene resolution rebinding; non-finite or overflowing glyph placement; or excessive
  aggregate glyph runs/glyphs.

Validation and box traversal are iterative. Exact validated scene-item and pending-text counts are
carried into construction. First-party vectors and each copied text string use `try_reserve_exact`
and return a structured allocation error rather than growing opportunistically.

Before `DisplayListBuilder` is created, a checked conservative upper bound is computed from
`peek_poke::Poke::max_size()` for the viewport `RectClip`, clip chain and its auxiliary `ClipId`
array/red zone, every rectangle/border item, the final display-item red zone, and the spatial-tree
red zone. The configured WebRender-byte limit must admit that bound. The exact serialized size is
checked again as a postcondition. WebRender's public builder does not expose a caller-supplied
fallible allocator; this preflight prevents accepted input from logically exceeding the configured
serialization budget, while process-level allocation failure inside the adopted builder remains an
upstream boundary to harden later.

Integer app units are inherently finite. Conversion still checks the resulting `f32`; that error is
defensive against a future geometry-source change.

## Root spatial and clip semantics

The scene owns one scene-local spatial root and viewport clip. The WebRender list uses
`SpaceAndClipInfo::root_scroll(pipeline_id)`, defines a rectangular viewport clip in that same local
root space, and defines an explicit clip chain containing it. Every primitive has both:

- `CommonItemProperties::clip_rect` equal to the local viewport; and
- the explicit viewport `clip_chain_id` and root `spatial_id`.

Primitive bounds are not destructively intersected during compilation. Negative and
out-of-viewport bounds remain intact, while WebRender receives both local clip mechanisms and can
perform its normal flattening/culling. The integration tests decode the built list and verify this
for a primitive extending past all viewport sides. This follows the local-space contract documented
on `CommonItemProperties` and exercised by `define_clip_rect`/`define_clip_chain` in the imported
API.

## Explicit gaps

- No font discovery, fallback, bidi, shaping, glyph cache, rasterization, or WebRender font-resource
  registration inside this crate. Those remain owned by the text adapter; unresolved initial
  scenes keep typed pending work.
- No WebRender renderer/device creation, GL surface, compositor, GPU-process protocol, frame
  submission, screenshot, or pixel/reftest output.
- No stacking contexts, transforms, opacity, scrolling nodes beyond the root, hit-test tags,
  rounded/complex clips, images, gradients, shadows, filters, Canvas, WebGL, or WebGPU.
- No border styles/colors/radii beyond the provisional solid `currentColor` mapping described
  above.
- No retained display-list diffing, invalidation, partial scene building, animation properties, or
  resource-update transaction.
- The current layout output is a bounded block/inline/flex slice. Its production path consumes
  Stylo-projected values, but neither layout nor painting is full CSS parity.

## Dependency, native-code, privacy, and platform audit

- `wild_buzzard_layout` is the live first-party immutable layout contract.
- `webrender_api` is the exact admitted Rust source under `gfx/wr/webrender_api`; this crate does not
  use `gfx/webrender_bindings`, Gecko, Wrench, SWGL, examples, or the WebRender renderer crate.
- The imported `webrender_api` crate contains upstream unsafe surfaces in `tile_pool.rs`, an
  `ExternalEvent` send marker, and an unsafe callback type. This boundary uses none of those APIs;
  it uses only the safe display-list, color, ID, and geometry surface. They remain part of the
  adopted crate's audit debt rather than new Wild Buzzard unsafe code.
- `peek-poke` is the exact admitted WebRender support crate and is used only for safe maximum-size
  queries directly. `DisplayListBuilder` also invokes its imported unsafe serializer internally;
  the derives' maximum-size contract is the relevant upstream safety invariant. This crate adds no
  unsafe block and does not widen that boundary.
- `DisplayListBuilder` records build timestamps through imported `zeitstempel`; on the supported
  Linux target it reaches the operating system's monotonic clock through `libc::clock_gettime`.
  That is a narrow adopted Linux OS-FFI boundary, not a third-party native library or new
  first-party FFI. Other platform modules in that dependency are inactive source and are not
  supported or tested.
- Direct code has no `unsafe`, FFI, build script, C/C++, native library, telemetry, service endpoint,
  filesystem lookup, or runtime network access.
- Runtime code has no platform branch. The supported build and tested target is only
  `x86_64-unknown-linux-gnu`.
- The crate is a root-workspace member and uses the repository lockfile for all locked owner gates;
  it has no nested lockfile or independent workspace.

## Firefox ESR153 reference evidence

The ignored Firefox checkout was read only and is not a dependency. The pinned reference paths
inspected were:

- `gfx/wr/webrender_api/src/display_list.rs`: builder lifecycle, clip definitions, primitive pushes,
  auxiliary-array encoding, iteration, and built-list size/round-trip APIs;
- `gfx/wr/webrender_api/src/display_item.rs`: common local clip/spatial properties and rectangle,
  border, and text item contracts;
- `gfx/wr/webrender_api/src/units.rs`: typed layout coordinate spaces and app-unit conversion;
- `layout/painting/nsDisplayList.cpp` and `layout/painting/nsDisplayList.h`: Gecko painting order,
  bounds, backgrounds, borders, text, and retained-list responsibilities (behavioral reference
  only);
- `gfx/layers/wr/ClipManager.cpp`, `gfx/layers/wr/ClipManager.h`, and
  `gfx/layers/wr/WebRenderCommandBuilder.cpp`: mapping display items to WebRender spatial/clip
  contracts (behavioral reference only);
- `gfx/webrender_bindings/WebRenderAPI.cpp` and `gfx/webrender_bindings/WebRenderAPI.h`: the Gecko
  adapter that Wild Buzzard intentionally replaces with a native Rust boundary.

Relevant reference tests inspected:

- `gfx/wr/wrench/src/rawtest.rs` and `gfx/wr/wrench/src/rawtests/snapping.rs` for direct built-list,
  rectangle, border, text, and coordinate behavior (Wrench itself remains excluded);
- `layout/reftests/display-list/reftest.list`, `retained-dl-zindex-1.html`,
  `retained-dl-zindex-1-ref.html`, `retained-dl-style-change-1.html`, and
  `retained-dl-style-change-1-ref.html` for ordering/invalidation expectations not yet claimed;
- `layout/reftests/reftest-sanity/test-displayport-bg.html` and
  `layout/reftests/reftest-sanity/test-displayport-ref.html` for viewport/background clipping
  expectations not yet elevated to pixel parity.

Meaningful history inspected with the full checkout:

- `845b81ddb8c73251bc74d77967af795b3b2c480b` — moved WebRender to `gfx/wr`, establishing the
  current source boundary;
- `73cc1ec2fe986092be99efe3dbb7bb990d443f0b` — added the rectangle-clip display item used for the
  explicit viewport clip;
- `231ad3350827e3d2ef5a00e094ac64982d76941b` — removed the display-list item cache, clarifying the
  current builder contract;
- `8591930ae4f84458aa1d1f68698fdf330ce2f60b` — unified active-scrolled-root to spatial-ID mapping;
- `437a2a9b2e7ba70f1fae1c4654208872447f5cda` — normalized external scroll offsets in the display-list
  builder, reinforcing that item/clip coordinates must share their declared spatial space;
- `78b87ae8be52d4c837d503999af0d83b31cf7f7c` — changed border/background painting behavior and was
  reviewed alongside current border construction.

These references informed contracts and test cases; no Gecko adapter, test asset, or C++ source was
copied.

## Owner gates

All artifacts are written below the external `../wildbuzzardbuilds/` tree. The crate has 23 focused
renderer integration tests plus two unit tests (and zero doc tests), including exact mapping,
transactional retry, paint order, first-baseline placement, Flex decoration painting, overflow, and
aggregate bounds. Representative commands are:

```sh
CARGO_TARGET_DIR=../wildbuzzardbuilds/agent-4-graphics-wave2 \
  cargo fmt --manifest-path gfx/wild_buzzard_renderer/Cargo.toml \
  --package wild_buzzard_renderer -- --check

CARGO_TARGET_DIR=../wildbuzzardbuilds/agent-4-graphics-wave2 \
  cargo check --manifest-path gfx/wild_buzzard_renderer/Cargo.toml \
  --workspace --all-targets --target x86_64-unknown-linux-gnu --locked

CARGO_TARGET_DIR=../wildbuzzardbuilds/agent-4-graphics-wave2 \
  cargo clippy --manifest-path gfx/wild_buzzard_renderer/Cargo.toml \
  --workspace --all-targets --all-features --target x86_64-unknown-linux-gnu --locked \
  -- -D warnings

CARGO_TARGET_DIR=../wildbuzzardbuilds/agent-4-graphics-wave2 \
  cargo test --manifest-path gfx/wild_buzzard_renderer/Cargo.toml \
  --workspace --target x86_64-unknown-linux-gnu --locked

CARGO_TARGET_DIR=../wildbuzzardbuilds/agent-4-graphics-wave2 \
  cargo build --manifest-path gfx/wild_buzzard_renderer/Cargo.toml \
  --workspace --release --target x86_64-unknown-linux-gnu --locked

CARGO_TARGET_DIR=../wildbuzzardbuilds/agent-4-graphics-wave2 \
RUSTDOCFLAGS="-D warnings" \
  cargo doc --manifest-path gfx/wild_buzzard_renderer/Cargo.toml \
  --workspace --no-deps --target x86_64-unknown-linux-gnu --locked
```
