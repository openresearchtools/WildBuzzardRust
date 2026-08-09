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

## Deliberate composition gap

The current `wild_buzzard_renderer` display-list compiler represents layout text
as `PendingTextRun` and deliberately omits it from its WebRender list.
`wild_buzzard_headless` can render a compiled page scene or one
`ShapedTextFrame`, but it does not expose a typed operation that merges the two
into one display list. Consequently `RenderedStaticPage` returns:

- `page_frame`: real WebRender output for page backgrounds and borders, with a
  nonzero pending-text count;
- `glyph_proof_frame`: a separate real WebRender frame for one exact shaped run;
- `composition`: an enum that states that these are not yet one complete frame.

The next cross-owner contract must accept multiple positioned shaped runs and
page primitives in one transaction/display list. This crate must then replace
the separate proof without reaching into renderer internals.

`PendingTextRun` currently preserves the UTF-8 text, computed font size, used
line height, color, measured rectangle, and a provisional baseline offset. It
does not carry CSS font family, weight, style, letter spacing, word spacing,
OpenType features, language, or direction. The separate proof therefore shapes
with initial family/weight/style/spacing settings. It places glyphs at the run
rectangle origin because the text-only renderer has no contract for applying
the pending run's provisional baseline to shaped glyph coordinates. Neither
CSS text-style fidelity nor exact baseline positioning is claimed yet.

Layout's `TextMeasurer` contract returns only `TextMetrics`. The layout engine
also measures speculative wrapping candidates, sometimes several prefixes for
one final fragment, and does not signal which shaped allocation became the
published fragment. Retaining every candidate `Arc<ShapedText>` here would
bypass the text system's bounded cache and can grow quadratically for long
wrapping input. A later cross-crate contract must put a bounded shaped handle on
the finalized layout fragment (or provide an equivalent finalization callback).
Until then a post-layout cache hit may reuse the same allocation, but this crate
does not claim or depend on `Arc` identity and reshapes safely after eviction.

DOM revisions are local mutation counters and can decrease when navigation
creates a smaller new document. This seam carries one typed `DocumentVersion`
(document identity plus local revision) unchanged through Stylo, layout, scene
compilation, page rendering, the glyph proof, and `PipelineEvidence`. The
headless renderer rejects an exact-version mismatch and only applies monotonic
revision checks when the immediately preceding submission belongs to the same
document; navigation to a distinct document never requires synthetic revision
rebasing. The synchronous `&mut StaticPageEngine` does not expose retained
compiled scenes or concurrent loads. A future asynchronous navigation owner
must add a monotonic navigation-generation/capability token: `DocumentVersion`
alone cannot reject an older document after another document intervened.

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
