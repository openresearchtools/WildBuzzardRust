# W4-A6E dynamic navigation handoff

- Task: connect exact-version DOM mutation and full recomposition to the bounded
  multi-context navigation worker without exposing stale generations or
  unbounded variable results.
- Owner: Agent 6, browser shell and integration; root integration and acceptance
  remain with the main orchestrator.
- Status: GO for this bounded worker seam after hostile-review repairs and the
  fresh locked gate recorded below. It remains NO-GO for script/event-loop
  integration, incremental rendering, browser UI, untrusted pages, or parity.
- Wild Buzzard paths changed:
  `browser/wild_buzzard_engine/{src/{dynamic,lib,navigation,pipeline}.rs,tests/{dynamic_document,navigation_facade}.rs,README.md}`
  and this handoff. No root manifest, lockfile, imported Stylo/WebRender source,
  or product-dispatch path changed in this slice.

## Admitted scope

W4-A6E connects the W3-A3S exact-version DOM transaction and W3-A6D full
live-document recomposition to the existing bounded `NavigationEngine` worker.
It does not add JavaScript execution, DOM event dispatch, a task/microtask loop,
incremental Stylo invalidation, browser chrome, or a platform window.

The worker accepts typed mutation and rerender commands only for one exact
`TopLevelContextId`, retained document `NavigationId`, and `DocumentVersion`.
The mutation payload is rechecked against the immutable DOM hard caps before it
can enter the command queue. One document operation may be outstanding per
context. Every admitted mutation or rerender receives a never-reused
`DocumentOperationId`: a private process-global engine-owner incarnation plus a
per-engine monotonic sequence. Owner exhaustion prevents engine construction;
sequence exhaustion rejects further document work without wrapping or changing
queue, document, cancellation, or reservation state.

## Per-context ownership

`StaticPageEngine` still owns one Linux EGL/WebRender renderer on its creator
thread. Its active `LiveDocumentPage` slot is exchanged only through a
crate-private method. `StaticPipelineExecutor` leaves that slot empty between
commands and retains opaque pages in a `TopLevelContextId` map. It activates
only the page named by a mutation/rerender command.

A navigation renders with the prior page held outside the engine. The new page
and old page remain in a private pending transaction until the shared worker
acknowledges whether its generation-checked frame publication succeeded. A
published load installs the replacement; a failed, stale, cancelled, or
resource-rejected publication restores the prior page. Thus context B can never
silently replace context A's mutation target, and a suppressed load cannot
become a hidden retained document.

The prior page is also restored on the otherwise-impossible invariant failure
where a successful static load returns no detachable live page. Renderer poison
is terminal even when cancellation or supersession suppresses the ordinary
operation event; stale publication cannot accidentally keep an unusable
renderer alive.

## L/F and cancellation state machine

For each retained context the shared worker records:

- `L`: exact live DOM `DocumentVersion`;
- `F`: exact revision represented by the last frame published behind a worker
  lease;
- the navigation generation which owns that document; and
- the exact active document-operation identity and cancellation source, when
  present; and
- a conservative retained-node charge.

Mutation admission reserves created-node live-state and result capacity. A
precommit rejection or cancellation releases both reservations and changes
neither `L` nor `F`. A committed downstream failure consumes them, advances
`L`, leaves `F`, and publishes the complete dense created-node mapping. A
successful mutation advances both; a successful rerender keeps `L` and makes
`F = L` without fetch, parse, mutation, token creation, or revision increment.

`Cancel(NavigationId)` is navigation-only. Mutation and rerender cancellation
requires the exact active
`CancelDocumentOperation(NavigationId, DocumentOperationId)` tuple. The
operation identity appears in its admission receipt and all five dynamic event
variants; a mutation result lease repeats the same identity. A precommit exact
cancel changes neither `L` nor `F`. If it is observed after the irreversible
DOM commit, the worker instead publishes
`DocumentMutationCommittedWithoutFrame` and retains `L` plus the result map. In
contrast, a newer navigation or `CloseContext` permanently supersedes the old
generation and invalidates its active operation identity. No old-generation
event or frame may publish; if the executor says the hidden page changed, both
executor and shared copies are discarded before that page can resurface.

A failed newer navigation may restore an older retained document even though
the context's latest admitted generation remains newer. Dynamic admission and
document-operation cancellation bind to that retained document's exact
navigation plus operation identity; navigation cancellation and `CloseContext`
remain latest-generation controls. Thus restored-document work remains
cancellable without allowing a delayed navigation control to cross generations.
Completed, stale sequential, wrong-navigation, wrong-context, foreign-engine,
closed, and superseded operation identities are rejected. Receiver drop and
terminal shutdown cancel any active document operation and clear queued work
and all pending document/result/payload reservations.

`DocumentOperationId` is only an in-process worker control identity. It is not
a persisted session identifier, an IPC authorization token, a security
principal, a cross-process wire format, or evidence of asynchronous script/event
loop integration. This slice still executes one synchronous document operation
at a time per context.

Navigation `Cancel` and `CloseContext` both name the exact current
`NavigationId`, not only a numeric context. Closing permanently retires that
`TopLevelContextId`; a new
context identity must be greater than the worker's monotonic admitted-identity
high watermark. The worker therefore needs only bounded O(1) retirement state,
and a delayed control or explicit navigation cannot cross a close/reopen ABA
boundary.

Every successful replacement navigation retires the prior typed document,
node charge, frame, and mutation-result leases for that context. This includes
a custom executor result with no `DocumentVersion`: such a navigation is an
intentional transition to navigation-only state, not permission for the old
typed document to remain reachable.

Successful custom-executor mutation outcomes are not trusted merely because
their Rust enum variant says `Rendered` or `CommittedWithoutFrame`. Before
publication the outcome must carry an opaque `DocumentMutationCommit` converted
from the DOM layer's private `ScriptMutationCommit`; safe custom executors can
no longer supply an arbitrary `Box<[NodeId]>`. The worker independently checks
that create tokens in the submitted batch are dense and in command order,
every `Created` operand refers only to an earlier create, every `Existing`
operand belongs to the named document, and the proven allocation map has the
exact version, document identity, reserved cardinality, and pairwise-distinct
nodes. A violation invalidates the executor and shared page and stops the worker
with `ExecutorContractViolation`; it cannot publish a pre-existing node as a
new allocation or a misleading replacement frame.

## Publication and resource transaction

Before entering a dynamic executor call the sole producer reserves:

- the operation's already-admitted exact `DocumentOperationId` binding;
- one ordinary event slot and event sequence;
- one frame-lease identity;
- aggregate retained capacity assuming the configured maximum frame size; and
- for mutation, one result-lease identity plus exact created-node reservations.
  An empty created-node map still consumes one retained result unit. Normalized
  queued command/string bytes have a separate aggregate budget.

The resulting event remains fixed-size. Variable created-node maps live in a
bounded store behind one-shot `MutationResultLeaseId`; frame pixels remain
behind the independent `FrameLeaseId`. Success can therefore publish frame and
mapping atomically without discovering event backpressure after the pipeline
has advanced its internal returned-frame version. A renderer-unusable rejection
or committed failure is published first, then the worker stops with
`WorkerStopReason::RendererUnavailable` and destroys the executor on its owner
thread.

Loaded documents charge the connected node count in their exact successful
snapshot. Every committed create command adds to the charge even if the new
node is detached. Navigation publication counts all other contexts' pending
creation reservations; dynamic entry revalidates retained plus pending charge,
and commit converts its exact pending charge to retained state under the same
lock. Thus one context cannot publish a load over capacity reserved for an
already-admitted mutation in another context. The DOM layer does not yet expose
exact total arena bytes or the count of parser-created nodes already detached
before the initial snapshot; string/vector capacities are also not charged.
Response-body limits, `max_contexts`, mutation hard caps, aggregate node
reservations, and result lease limits are defense in depth, not final per-site
RSS accounting.

## Firefox ESR153 reference inspected

Reference checkout: `c19b7e89270787889495688244ec6ee8e79288a1`.

- `docshell/base/nsDocShell.h`: current-document-viewer ownership and `Stop`
  contract, especially the `mDocumentViewer` ownership notes.
- `docshell/base/nsDocShell.cpp`: `nsDocShell::Stop`,
  `nsDocShell::SetupNewViewer`, old/new viewer handoff, delayed showing of the
  replacement viewer, and page-load completion paths.
- `docshell/test/browser/browser_onbeforeunload_navigation.js`: replacement
  navigation and stop ordering.
- `docshell/test/browser/browser_onunload_stop.js`: stop during unload.
- `docshell/test/unit/test_subframe_stop_after_parent_error.js`: asynchronous
  cancellation and terminal stop after failed parent loads.

The reference behavior informed ownership and cancellation invariants; no C++
architecture or provider-specific service was copied.

## Regression evidence

`tests/navigation_facade.rs` covers:

- an exact-ID postcommit mutation cancel which retains advanced `L`, unchanged
  `F`, and a transferable result map carrying the same identity, followed by an
  exact-ID repair rerender;
- atomic precommit mutation cancellation with navigation-cancel isolation;
- gated rerender cancellation with receipt/event identity, navigation-cancel
  isolation, later stale-ID rejection, and successful cleanup/retry;
- a failed newer navigation which restores the older retained document, then
  admits and exactly cancels work on that older document while latest-generation
  navigation/close controls remain isolated;
- two sequential operations under one navigation, proving a delayed first ID
  cannot cancel the second, plus wrong-generation and wrong-context rejection;
- two live engine incarnations with equal local sequences, proving a foreign
  owner ID cannot cancel the other engine's work;
- a superseding navigation which suppresses all stale mutation events and
  discards changed hidden state while invalidating its operation ID;
- close during an in-flight committed mutation, same-thread invalidation, an
  exact-navigation close event, operation-ID invalidation, and permanent
  numeric-context retirement;
- stale cancel, close, and explicit-navigation controls rejected across two
  accepted generations;
- a typed-document to navigation-only replacement which retires node charge and
  result leases, followed by successful typed admission in another context;
- a pending mutation in context A which makes context B's typed navigation fail
  at the document budget, then converts atomically and publishes in A;
- a real loopback load in two contexts followed by mutation of the first and
  exact rerender of the second, proving independent retained pages behind one
  renderer/executor;
- a deterministically gated, undrained full event queue which stops the worker
  before queued dynamic executor entry and still publishes terminal status in
  the reserved slot;
- an executor which applies a valid transaction while the submitted batch has
  an out-of-order create token; and
- a one-create proof whose allocation identity is shown distinct from a
  pre-existing node in the same document.

The invalid-topology success claim proves terminal contract failure and
same-thread document invalidation without a dynamic success event. The
pre-existing-node adversary is prevented structurally: only the DOM commit's
private allocation map can enter `DocumentMutationCommit`.

Private unit regressions additionally force per-engine operation-sequence and
process-global owner-incarnation exhaustion without wrap, and inspect receiver
drop to prove that an active document cancellation source, queued work, and all
pending reservation counters are cleared.

All Cargo outputs, Python environments, and logs must remain under
`/home/user/Documents/wildbuzzardbuilds/`. W4-A6E used:

- rustc `1.96.0 (ac68faa20 2026-05-25)`;
- cargo `1.96.0 (30a34c682 2026-05-25)`;
- Python `/home/user/Documents/wildbuzzardbuilds/w2-a6-static-pipeline/python/bin/python`;
- Cargo target `/home/user/Documents/wildbuzzardbuilds/w4-a6e/cargo`; and
- temporary files `/home/user/Documents/wildbuzzardbuilds/w4-a6e/tmp`.

The exact gates were:

```sh
export TMPDIR=/home/user/Documents/wildbuzzardbuilds/w4-a6e/tmp
export PYTHON3=/home/user/Documents/wildbuzzardbuilds/w2-a6-static-pipeline/python/bin/python
export CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w4-a6e/cargo
export CARGO_INCREMENTAL=0
export CARGO_PROFILE_DEV_DEBUG=0
export CARGO_PROFILE_TEST_DEBUG=0

cargo fmt --manifest-path browser/wild_buzzard_engine/Cargo.toml \
  --package wild_buzzard_engine -- --check

cargo check --manifest-path browser/wild_buzzard_engine/Cargo.toml \
  --workspace --all-targets --locked --target x86_64-unknown-linux-gnu

cargo clippy --manifest-path browser/wild_buzzard_engine/Cargo.toml \
  --workspace --all-targets --locked --target x86_64-unknown-linux-gnu \
  --no-deps -- -D warnings -W clippy::all -W clippy::pedantic

cargo test --manifest-path browser/wild_buzzard_engine/Cargo.toml \
  --locked --target x86_64-unknown-linux-gnu --test navigation_facade

cargo test --manifest-path browser/wild_buzzard_engine/Cargo.toml \
  --workspace --locked --target x86_64-unknown-linux-gnu

RUSTDOCFLAGS='-D warnings' \
cargo doc --manifest-path browser/wild_buzzard_engine/Cargo.toml \
  --workspace --locked --target x86_64-unknown-linux-gnu --no-deps

cargo build --manifest-path browser/wild_buzzard_engine/Cargo.toml \
  --workspace --release --locked --target x86_64-unknown-linux-gnu
```

Results: package formatting passed; all-target workspace check passed; strict
Clippy passed; warning-denied rustdoc passed; the focused navigation facade
passed 33 tests; and the full nested workspace passed 54 tests (10 library, 8
dynamic-document, 33 navigation-facade, and 3 static-pipeline tests) plus zero
doc tests. The optimized release build passed. It emitted one inherited warning
from imported WebRender: `RenderTaskGraph::frame_id` is never read in
`gfx/wr/webrender/src/render_task_graph.rs`. W4-A6E did not modify that upstream
file or suppress the warning.

An independent final hostile read-only rereview found no Critical, High,
Medium, or Low defect. It verified the engine-incarnation and per-engine
operation identity scheme, the distinct navigation/document cancellation
paths, exact `(NavigationId, DocumentOperationId)` matching, exhaustion without
wrap or mutation, and the completed, stale, restored-page, cross-engine,
supersession, close, shutdown, and receiver-drop cases. The reviewer also
confirmed that the original accounting, context-retirement, DOM-issued commit,
and navigation-only cleanup repairs remain sound and that the 33 focused / 54
full test counts match the source.

## Remaining work

- Exact arena/string/vector/RSS accounting and a content-process memory-pressure
  policy.
- Journaled/incremental DOM mutation, style invalidation, and frame scheduling.
- Rooted Brimstone host objects, task/microtask/event ordering, exceptions, and
  navigation teardown semantics.
- Origin/site-isolation ownership, child browsing contexts, history/BFCache,
  redirects, asynchronous fetch, and process crash recovery.
- Presentation of worker frame leases through the Linux window/compositor and
  Rust browser UI.
