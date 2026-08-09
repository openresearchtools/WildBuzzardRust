# Wild Buzzard static-page engine seam

This crate is an independently testable Linux x86-64 integration boundary. It
currently proves this concrete path:

```text
numeric-loopback HTTP
  -> bounded UTF-8 HTML parser
  -> Rust DOM snapshot
  -> imported Stylo selector matching/cascade/computed values
  -> Rust layout using metrics from the Rust text shaper
  -> validated WebRender display list
  -> real Linux EGL/WebRender RGBA8 readback
```

It also shapes every pending page text run through `wild_buzzard_text` and sends
one exact non-whitespace shaped result through the real WebRender glyph adapter.
The graphics crates separately provide a checked complete-inventory composition
path; this engine has not connected its finalized shaped runs to that path yet.

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

## Deliberate composition gap

The initial `wild_buzzard_renderer` display-list compiler represents layout text
as `PendingTextRun` and deliberately omits it from its first WebRender list.
W2-A4D adds an exact-scene-bound `wild_buzzard_headless::render_composed`
operation which can replace every pending slot with a complete supplied shaped
inventory in one display list and transaction. This synchronous engine does not
call that operation yet. Consequently `RenderedStaticPage` still returns:

- `page_frame`: real WebRender output for page backgrounds and borders, with a
  nonzero pending-text count;
- `glyph_proof_frame`: a separate real WebRender frame for one exact shaped run;
- `composition`: an enum that states that these are not yet one complete frame.

The next integration must retain the exact shaped allocations for every
finalized fragment, project `first_baseline` through the accepted graphics
contract, call `render_composed` once, and replace the separate proof without
reaching into renderer internals.

`PendingTextRun` currently preserves the UTF-8 text, computed font size, used
line height, color, measured rectangle, and a provisional baseline offset. It
does not carry CSS font family, weight, style, letter spacing, word spacing,
OpenType features, language, or direction. The separate proof therefore shapes
with initial family/weight/style/spacing settings and does not establish page
placement. W2-A4D's reusable contract requires exact quantized metrics and adds
fragment top to Parley's already-baselined glyph Y exactly once; this engine
must still supply `first_baseline` rather than its older ascent projection.
Neither CSS text-style fidelity nor engine-level exact baseline positioning is
claimed yet.

Layout's `TextMeasurer` contract returns only `TextMetrics`. The layout engine
also measures speculative wrapping candidates, sometimes several prefixes for
one final fragment, and does not signal which shaped allocation became the
published fragment. Retaining every candidate `Arc<ShapedText>` here would
bypass the text system's bounded cache and can grow quadratically for long
wrapping input. The next cross-crate integration must put a bounded shaped
handle on the finalized layout fragment (or provide an equivalent finalization
callback), then pass that exact inventory to W2-A4D. Until then a post-layout
cache hit may reuse the same allocation, but this crate does not claim or
depend on `Arc` identity and reshapes safely after eviction.

DOM revisions are local mutation counters and can decrease when navigation
creates a smaller new document. This seam carries one typed `DocumentVersion`
(document identity plus local revision) unchanged through Stylo, layout, scene
compilation, page rendering, the glyph proof, and `PipelineEvidence`. The
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
