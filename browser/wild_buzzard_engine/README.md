# Wild Buzzard page engine seam

This crate is an independently testable Linux x86-64 integration boundary. It
currently proves this concrete path:

```text
numeric-loopback HTTP
  -> bounded UTF-8 HTML parser
  -> Rust DOM snapshot
  -> imported Stylo selector matching/cascade/computed values
  -> Rust layout using metrics from the Rust text shaper
  -> validated WebRender scene plus canonical finalized text inventory
  -> one composed Linux EGL/WebRender RGBA8 readback
```

After layout and scene compilation, the engine shapes only the canonical
finalized `PendingTextRun` inventory. It retains every exact bounded
`Arc<ShapedText>` through W2-A4D's checked `render_composed` path, including
whitespace-only entries, and returns one frame with zero pending text.

## Bounded navigation facade

`NavigationEngine` wraps that synchronous pipeline in one dedicated worker. The
worker constructs, uses, and shuts down its thread-affine EGL/WebRender executor
on the same thread. The existing `&mut StaticPageEngine` API remains available
for direct synchronous use.

The facade admits bounded `NavigationRequest` values through typed
`TopLevelContextId` and monotonic `NavigationGeneration` identities. Navigate
admission is transactional: a full queue, invalid generation, or context limit
does not advance state or cancel previously accepted work. Accepting a newer
generation cancels the old token, and the worker checks the generation again
under the publication lock after execution. A stale result can therefore emit
only `NavigationCancelled`; it cannot replace a frame or emit `FrameReady`.

Successful publication reserves `NavigationCommitted` and `FrameReady`
together, accounts the configured per-frame and aggregate retained-byte limits,
and replaces the context's frame behind an opaque one-shot lease in one locked
transaction. The minimum event capacity is three: one undrained
`NavigationStarted` plus that indivisible two-event publication. Event
backpressure stops the worker with an inspectable reason and
preserves the previously published frame. A reserved terminal-event slot keeps
shutdown observable even when the ordinary event queue is full. Executor
construction, execution, explicit destruction, cleanup, panic containment, and
the worker join have deterministic ownership and shutdown behavior.

This slice has only `Navigate`, `Cancel`, and `Shutdown` commands. Context
entries are lifetime-bounded by `max_contexts`, but there is not yet a
`CloseContext` command or context-slot reuse. It is not a tab/window lifecycle,
browser UI, platform event loop, or asynchronous networking implementation.

## Bounded live-document recomposition

After a successful synchronous load, `StaticPageEngine` retains exactly one
opaque mutable `LiveDocumentPage` alongside the thread-affine renderer. The
arena never leaves the engine and callers receive only read-only document
identity and lookup operations. `apply_and_render` accepts the frozen DOM
layer's exact-version `ScriptMutationBatch` under its configured per-batch hard
limits. It performs no fetch or HTML parse, then fully recomputes an immutable
snapshot through Stylo, layout, canonical text shaping, scene compilation, and
one composed WebRender frame. `DynamicRenderEvidence` deliberately omits HTTP
and parser counters so an update cannot be mistaken for another navigation.

The update boundary tracks two exact versions: `L`, the retained live DOM, and
`F`, the DOM revision represented by the last frame successfully returned to
the caller. Its state transitions are:

| Operation outcome | Live DOM (`L`) | Last returned frame (`F`) |
| --- | --- | --- |
| Mutation rejected before commit | unchanged | unchanged |
| Mutation committed, downstream work failed | advances once | unchanged |
| Mutation and frame return succeeded | advances once | becomes `L` |
| Exact-version rerender failed | unchanged | unchanged |
| Exact-version rerender succeeded | unchanged | becomes `L` |

Version, token, per-batch mutation-resource, or DOM-command rejection occurs
against a private working copy before commit. Once a batch commits, style,
layout, text, scene, cancellation, deadline, or renderer failure cannot roll
the DOM back. `DocumentUpdateError::Committed` therefore retains the exact
created-node map and reports the advanced `L` together with the unchanged `F`.
A repair batch must target `L`. `rerender_live` accepts exactly `L`, performs no
fetch, parse, mutation, created-node mapping, or revision increment, and can
bring `F` back to `L` after a recoverable downstream failure.

`F` describes only owned frames already returned by this API. It makes no claim
about a renderer's internal surface after a post-send error; that surface may
be indeterminate. The engine exposes `renderer_is_usable()`, and all load,
mutation, and rerender entry points reject an unusable renderer before further
work. An unusable renderer is terminal: tear down the engine and load the page
in a replacement. A `true` health result merely permits another attempt and
does not predict success. A usable pre-send failure may instead be repaired
against the advanced live version. Renderer epochs are monotonic attempt identifiers,
not success counters, so a failed attempt can leave an observable gap.

This is a synchronous pipeline proof, not script execution. It does not connect
Brimstone or any transitional JavaScript runtime, dispatch events, run an event
loop, process microtasks, implement live Stylo invalidation, or expose updates
through `NavigationEngine`. Each update does a complete recomputation. The
configured mutation caps bound only one submitted batch; cumulative
per-document mutation and detached-node accounting, navigation-generation
publication, and multi-document ownership remain required before this seam may
process untrusted page script.

## Composed text boundary

The initial `wild_buzzard_renderer` display-list compiler represents layout text
as `PendingTextRun` and deliberately omits it from its first WebRender list.
W2-A4D adds an exact-scene-bound `wild_buzzard_headless::render_composed`
operation which can replace every pending slot with a complete supplied shaped
inventory in one display list and transaction. This engine now passes the
original compiled scene and one `ShapedSceneText` per canonical pending ID to
that operation. Missing, duplicate, unknown, reordered, wrong-text, and
wrong-metric inventories fail before renderer mutation; a successful
`RenderedStaticPage::frame` has zero pending text.

`PipelineEvidence::pre_composition_display_list_bytes` measures the validated
pending-text scene list. The final glyph-containing display list is rebuilt and
submitted privately inside `render_composed`, so the engine does not misreport
the earlier byte count as final composed-list evidence.

`PendingTextRun` currently preserves the UTF-8 text, computed font size, used
line height, color, measured rectangle, and a provisional baseline offset. It
does not carry CSS font family, weight, style, letter spacing, word spacing,
OpenType features, language, or direction. Shaping therefore still uses initial
family/weight/style/spacing settings. The engine projects the shaped line extent
above the baseline as `first_baseline` and below it as
`height - first_baseline`. W2-A4D adds only the fragment top to Parley's
already-baselined glyph Y, so font ascent is never added a second time. This is
exact placement for the admitted contract, not complete CSS text-style parity.

Layout's `TextMeasurer` contract returns only `TextMetrics`. The layout engine
also measures speculative wrapping candidates, sometimes several prefixes for
one final fragment, and does not signal which shaped allocation became the
published fragment. Retaining every candidate `Arc<ShapedText>` outside the
text system would bypass its bounded cache and can grow quadratically for long
wrapping input. The engine therefore recovers shapes only after scene
compilation, from the bounded canonical pending inventory. Those exact
allocations remain alive through composition; speculative measurements remain
owned only by the bounded text cache.

DOM revisions are local mutation counters and can decrease when navigation
creates a smaller new document. This seam carries one typed `DocumentVersion`
(document identity plus local revision) unchanged through Stylo, layout, scene
compilation, composed rendering, and `PipelineEvidence`. The
headless renderer rejects an exact-version mismatch and only applies monotonic
revision checks when the immediately preceding submission belongs to the same
document; navigation to a distinct document never requires synthetic revision
rebasing. The synchronous `&mut StaticPageEngine` does not expose retained
compiled scenes or concurrent loads. The worker facade adds a context-local
navigation generation for stale-publication suppression; `DocumentVersion`
remains the exact identity inside one pipeline result.

Other intentionally visible gaps in this bounded slice are HTML encoding
sniffing, redirects, external stylesheets, images/media, script, and non-loopback
networking. Those are rejected or absent; they are not simulated.

## External build and test

Stylo's generated properties require the Python packages pinned in
`servo/style-build-requirements.txt`. Keep both that environment and Cargo
output outside the repository:

```sh
task_root=/home/user/Documents/wildbuzzardbuilds/w2-a6-static-pipeline
python3 -m venv "$task_root/python"
"$task_root/python/bin/python" -m pip install \
  -r servo/style-build-requirements.txt

PYTHON3="$task_root/python/bin/python" \
CARGO_TARGET_DIR="$task_root/cargo" \
cargo check --manifest-path browser/wild_buzzard_engine/Cargo.toml \
  --workspace --all-targets --locked --target x86_64-unknown-linux-gnu

PYTHON3="$task_root/python/bin/python" \
CARGO_TARGET_DIR="$task_root/cargo" \
cargo clippy --manifest-path browser/wild_buzzard_engine/Cargo.toml \
  --workspace --all-targets --locked --target x86_64-unknown-linux-gnu \
  --no-deps -- -D warnings -W clippy::all -W clippy::pedantic

PYTHON3="$task_root/python/bin/python" \
CARGO_TARGET_DIR="$task_root/cargo" \
cargo test --manifest-path browser/wild_buzzard_engine/Cargo.toml \
  --workspace --locked --target x86_64-unknown-linux-gnu
```

Use the same `PYTHON3`, target directory, manifest, lock, and Linux target for
release and rustdoc gates. Do not create a virtual environment, `target/`,
screenshot, or log inside the live source tree.
