# Wild Buzzard page engine seam

This crate is an independently testable Linux x86-64 integration boundary. It
currently proves this concrete path:

```text
explicit network capability
  -> numeric-loopback HTTP, or bounded DNS + HTTP/authenticated HTTPS
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

W9-A6I adds the first general-web top-level vertical without weakening the
numeric-loopback proof. `NavigationRequest::new` retains the old
`NumericLoopback` authority; `NavigationRequest::general_web` selects a distinct
`GeneralWeb` authority, and a real pipeline executor accepts only the authority
with which it was constructed. `NavigationEngine::spawn_general_web` and
`spawn_general_web_for_presentation` construct the reviewed `GeneralWebClient`
inside the existing dedicated worker, so URL validation, system DNS, TCP, TLS,
HTTP parsing/body delivery, HTML, style, layout, shaping, scene construction,
and headless or presentation output all remain off the caller/UI thread.

One absolute operation deadline and one cancellation token cover transport and
all later synchronous stages. A transport cancellation or an elapsed absolute
deadline is projected back to the existing fixed-size `Cancelled` or
`DeadlineExceeded` navigation outcome at `Fetch`; an ordinary socket inactivity
timeout remains a network failure. The worker's existing generation check still
prevents a superseded general-web result from committing or publishing a frame.

General-web response bytes use exactly the same configured body bound and
UTF-8-to-frame path as loopback bytes. W9-A6K adds an iterative, manual
top-level redirect walker with one exported ten-hop bound, one absolute
deadline, and one cancellation token. It admits 301, 302, 303, 307, and 308;
rejects missing/ambiguous/malformed or prohibited locations, loops, unsupported
3xx semantics, and excess hops with typed failures; and never reads a redirect
body as a document. WHATWG-normalized fragments remain in browser navigation
identity, inherit across a fragmentless `Location`, and are stripped only from
the exact HTTP transport request target.

Successful publication atomically installs a bounded one-shot commitment keyed
by the exact `NavigationId` before the unchanged fixed-size
`NavigationCommitted` event. It carries the normalized final URL, redirect
count, sticky authenticated-HTTPS-to-cleartext downgrade bit, and exact final
connection evidence (`Cleartext` or authenticated TLS version and ALPN).
Foreign, missing, duplicate, stale, and detached transfers fail closed without
consuming a different commitment. `NavigationCommitMetadata::validate_general_web`
additionally rejects an embedding's invalid, credentialed,
noncanonical, over-limit, unverified, or scheme/security-incoherent record
before product history or chrome can consume it. That method validates structural
coherence; it cannot authenticate a custom embedding which fabricates coherent
metadata. Authenticity comes from retaining the concrete engine-to-UI ownership
seam. Even authentic transport evidence is not permission for browser chrome to
invent a lock or other assurance its current UI model cannot represent.

## Captured final-response policy inputs

W9-A5L retains one bounded `CapturedDocumentResponseMetadata` inside the same
`LiveDocumentPage` owner as the parsed DOM. It is bound to the exact initial
`DocumentVersion` and the exact final `NavigationCommitMetadata`; replacing or
moving a live page therefore moves the response inputs with it. Dynamic DOM
revisions retain that original response binding rather than relabelling the
headers as if they came from a mutation. A failed later navigation leaves the
prior page and its envelope unchanged.

Only the final successful top-level response is captured. Redirect response
policy fields and cookie values are discarded with those response objects.
Duplicate enforcing CSP, report-only CSP, and Content-Type field lines remain
separate and ordered. Referrer-Policy comma tokens are inspected in field order
and only the currently recognized typed inputs are retained, together with an
ignored-token count. Content-Type fields retain a typed media type and ordered
charset inputs, or a non-sensitive malformed classification. Set-Cookie retains
only presence, field count, and aggregate field-value bytes; its values never
enter the live document. CSP bytes remain available only because the next gate
needs a dedicated parser, and their count, individual size, and aggregate size
are hard-bounded. Debug output redacts CSP and cookie values.

This envelope is observation, not admission or enforcement. No CSP directive,
referrer policy, Content-Type encoding/MIME decision, or cookie mutation is
applied by W9-A5L, and no external stylesheet is fetched. A later loader must
parse the captured policies, resolve the final response/base URL, apply CSP and
mixed-content checks before issuing a request, and own cookie/referrer behavior
in their dedicated subsystems. Deterministic tests prove that merely adding
these observed headers leaves the exact visible frame unchanged at 1366×768 and
1920×1080.

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

W4-A6E adds typed `MutateDocument`, `RerenderDocument`, and `CloseContext`
commands without turning this worker into a script event loop. One renderer and
executor remain thread-affine. Between calls the executor detaches the opaque
`LiveDocumentPage` into a map keyed by `TopLevelContextId`, then activates only
the page named by the command. A navigation result is held as a private
old/new-page transaction until the worker acknowledges publication; stale or
resource-rejected results restore the prior context page. This prevents a load
in context B from becoming the implicit mutation target for context A.

Dynamic admission requires the exact retained document navigation and
`DocumentVersion`, permits only one outstanding document operation per context,
and applies the same bounded command queue. Every admitted mutation or rerender
receives a never-reused `DocumentOperationId` containing a private
process-global engine incarnation and a per-engine monotonic sequence. The ID
is returned in its admission receipt and repeated in every dynamic outcome;
mutation result leases repeat it as well. Incarnation or sequence exhaustion
fails closed rather than wrapping.

`Cancel(NavigationId)` is navigation-only. A document operation can be
cancelled only by `CancelDocumentOperation(NavigationId, DocumentOperationId)`
matching the exact active tuple. This remains true when a failed newer
navigation leaves an older retained document active: the older document's
operation is cancellable by its own tuple, while navigation control and close
continue to follow the latest admitted generation. Completed, superseded,
closed, wrong-generation, wrong-context, foreign-engine, and stale sequential
operation IDs cannot cancel current work. Explicit document-operation
cancellation observed after the DOM commit publishes
`DocumentMutationCommittedWithoutFrame`, retains the advanced live revision,
and exposes the dense created-node map. A newer navigation or `CloseContext`
instead cancels and invalidates the operation identity and makes the old
generation permanently unpublishable; if hidden executor state changed, that
old page is discarded so it cannot later resurface. Receiver drop and worker
shutdown cancel active document work and clear its queued reservations.

Created-node maps do not make `EngineEvent` variable-sized. They are held under
an aggregate result-unit budget behind independent one-shot
`MutationResultLeaseId` values; even an empty map consumes one unit, so draining
events without consuming leases cannot grow the map store without bound.
Normalized queued command/string bytes have a separate aggregate budget.
Successful mutation/rerender execution reserves one event sequence, one
frame lease, worst-case retained-frame capacity, and (for mutation) its result
lease before entering the executor. The worker therefore has no ordinary
event-backpressure failure point after a successful dynamic render advances its
internal frame version. Renderer-unusable failure publishes its exact dynamic
outcome and then terminally stops the worker.

Successful custom-executor mutation outcomes must carry an opaque
`DocumentMutationCommit` converted from the DOM layer's private
`ScriptMutationCommit`; callers cannot construct a created-node mapping from
raw `NodeId` values. The worker additionally checks the submitted batch's
token topology and the proof's exact version, document identity, cardinality,
and uniqueness before publication.

Retained live-state accounting charges every connected node reported by a
successful load and every node created by a committed mutation, including a
created node later detached by that batch. Pending creation reservations count
against navigation publication in every context and are revalidated immediately
before executor entry and atomically converted to retained charge at commit. A
successful navigation, including a navigation-only custom result, retires the
prior typed document, its node charge, and all of that context's
mutation-result leases.

The current DOM API does not expose an exact arena allocation/owned-byte
counter, so parser-created nodes already detached before the first snapshot and
string/vector allocation capacity remain an explicitly uncharged residual.
`max_contexts`, bounded response bodies, per-batch string/creation caps, and the
aggregate node charge still prevent an unbounded worker-owned context or
mutation-result population, but this is not final browser memory accounting.

Context close names the exact current `NavigationId`, destroys its page on the
executor owner thread, and permanently retires that numeric context identity.
New raw `TopLevelContextId` values must be greater than every identity this
worker has previously admitted; this bounded high-watermark rule prevents
delayed `Cancel`, `CloseContext`, or explicit `Navigate` commands from crossing
a close/reopen ABA boundary. This is still not a tab/window lifecycle, browser
UI, platform event loop, or asynchronous networking implementation.

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
Brimstone or any transitional JavaScript runtime, dispatch DOM events, run an
event loop, process microtasks, or implement live Stylo invalidation. Each
worker update still does a complete recomputation. W4-A6E supplies
navigation-generation publication and bounded multi-context ownership, but
cumulative DOM strings/arena bytes, journaled mutation, rooted script tasks,
origin/process policy, and untrusted-script resource enforcement remain open.

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
sniffing, external stylesheets, images/media, script,
cookies/cache/proxy/HTTP2/HTTP3, and complete normal-page layout. Those are
rejected or absent; they are not simulated. The opt-in public
`https://example.com/` assertion now reaches authenticated transport, HTML,
DOM, Stylo, CSS2 automatic-margin resolution, layout, shaping, and visible
WebRender frames at both 1366×768 and 1920×1080. Its centered viewport-relative
body geometry matches the same-size Firefox ESR captures. The comparison still
shows generic canvas-background propagation and font-family/weight gaps, so it
is evidence of pipeline progress rather than page or Firefox parity. No CSS is
stripped and no per-site workaround exists.

## External build and test

Stylo's generated properties require the Python packages pinned in
`servo/style-build-requirements.txt`. Keep both that environment and Cargo
output outside the repository:

```sh
task_root=/home/user/Documents/wildbuzzardbuilds/w9-a6k-redirect-identity
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

The deterministic matrix includes a system-DNS HTTP fixture at 1366×768, an
authenticated local-TLS fixture at 1920×1080, absolute-deadline and
stale-generation regressions, capability mismatch, relative/absolute and
inherited fragment redirects, exact GET wire targets across 301/302/303/307/308,
typed malformed/prohibited redirect failures without document publication,
loop/ten-hop bounds, cross-hop cancellation and deadline, exact TLS/ALPN
evidence, sticky downgrade evidence, one-shot commitment transfer, bounded
final-response policy capture, duplicate/mixed-case policy fields, cookie-value
redaction, malformed Content-Type classification, and exact metadata retention
across mutation/rerender.
The local TLS server is test-only OpenSSL process infrastructure; it is not a
runtime dependency or trust-verifier substitute. To rerun the deliberately
ignored public assertion at both desktop viewports:

```sh
PYTHON3="$task_root/python/bin/python" \
CARGO_TARGET_DIR="$task_root/cargo" \
cargo test --manifest-path browser/wild_buzzard_engine/Cargo.toml \
  --locked --target x86_64-unknown-linux-gnu \
  --test general_navigation public_example_https_reaches_a_visible_desktop_frame \
  -- --ignored --exact --test-threads=1
```

To record bounded raw RGB comparison artifacts, first create an external
directory and set `WILDBUZZARD_PUBLIC_CAPTURE_DIR` for that same command. The
test writes one PPM per viewport with fixed names and never writes a screenshot
without that opt-in environment variable. Firefox ESR reference screenshots
must use the exact same URL, viewport, date, machine, and scale.

Use the same `PYTHON3`, target directory, manifest, lock, and Linux target for
release and rustdoc gates. Do not create a virtual environment, `target/`,
screenshot, or log inside the live source tree.
