# W3-A6D bounded live-document recomposition handoff

## Accepted scope

W3-A6D adds one direct synchronous live-document owner to
`browser/wild_buzzard_engine`. A successful load retains exactly one opaque
mutable `Document`; callers can inspect identity and lookup data but can mutate
it only through W3-A3S's exact-version bounded `ScriptMutationBatch`.

This is a contained recomposition proof. It is not exposed by
`NavigationEngine`, does not execute Brimstone or transitional JavaScript,
does not dispatch tasks, events, microtasks, or mutation observers, and does
not implement incremental Stylo invalidation. It is not permission to process
untrusted page script or a browser/parity result.

## State and failure contract

The engine tracks two exact `DocumentVersion`s:

- `L` is the retained live DOM identity/revision.
- `F` is the revision represented by the last owned frame successfully
  returned from this API.

A precommit version, token, command, DOM, or per-batch resource rejection is
atomic: the private working arena is discarded and the tree, node-slot
allocation, `L`, and `F` remain unchanged. A successful DOM transaction
irreversibly advances `L` exactly once and returns its exact immutable snapshot
and created-node map. If later style, layout, text, scene, cancellation,
deadline, or renderer work fails, `DocumentUpdateError::Committed` reports the
advanced `L`, unchanged `F`, created-node map, and exact downstream source.

Success fully recomputes that snapshot through imported Stylo, layout,
canonical text shaping, scene compilation, and one composed WebRender
readback. No fallible checkpoint follows a successful `render_composed` return;
that frame return is commit-wins and then `F` becomes `L`.

`rerender_live` requires the exact current `L`. It performs no fetch, parse,
DOM mutation, created-node mapping, or revision increment. A successful
rerender returns a new attempt epoch for the same revision and brings `F` to
`L`. Renderer epochs are monotone attempt identifiers, not success counters;
a failed attempt after epoch reservation may leave a gap.

`F` describes only frames successfully returned to the caller. It is not proof
of internal backend-surface rollback after a post-send error.
`renderer_is_usable() == false` is terminal for the engine: callers must tear
it down and create/load a replacement. `true` permits another attempt but does
not predict success. A pre-send validation/resource failure can therefore be
repairable while a post-send poisoned renderer is not.

## Cross-owner change

`gfx/wild_buzzard_headless::HeadlessRenderer::is_usable` exposes only the
minimal health bit needed by the engine. `false` is terminal; `true` supplies
no recovery, backend-state, or presentation guarantee. The existing real
post-send timeout regression now asserts the transition from usable to
unusable. No renderer protocol, FFI, dependency, or native capability was
otherwise added.

## Verification evidence

All generated output remained under `/home/user/Documents/wildbuzzardbuilds`.
The owner matrix under `w3-a6d-dynamic-document-corrected` passed:

- exact formatting and diff checks;
- locked Linux all-target workspace check;
- 8 focused dynamic tests;
- 33 complete nested-workspace tests (7 unit, 8 dynamic, 15 navigation, and 3
  static);
- strict no-dependency Clippy across all targets/features with
  `clippy::all`, `clippy::pedantic`, and `-D warnings`;
- release build; and
- warning-denied no-dependency rustdoc.

An independent frozen-source review returned GO for this contained scope with
no high- or medium-severity finding. Its fresh external target under
`w3-a6d-independent-review` repeated all 8 dynamic tests, strict all-target
pedantic Clippy, the unusable-renderer preflight unit, and the real headless
post-send poison/teardown regression. The reviewer verified the `L`/`F`
transitions, exact preflight order, irreversible commit, commit-wins return,
rerender behavior, failed replacement retention, attempt epochs, and honest
renderer-state boundary.

After that review, the orchestrator applied only its recommended wording fix:
`Rejected` now says the call committed no DOM mutation, which is exact for both
precommit mutation rejection and a no-mutation rerender failure. The engine
README title and `true` health wording were clarified at the same time. No
executable state-machine behavior changed.

Final SHA-256 identities:

| Path | SHA-256 |
| --- | --- |
| `browser/wild_buzzard_engine/README.md` | `61da0771d689f4ad389dab4b6faf7aea6baaf2c3d26cf4d1c949672641f770d8` |
| `browser/wild_buzzard_engine/src/dynamic.rs` | `d02bb3ffad8a7a86130ffaa726c69a5f55efee5f7abf68d6353911d336c8acd1` |
| `browser/wild_buzzard_engine/src/lib.rs` | `0c3fecff19015a155b52347dbb4b9271b8b45b2f4b1afb6f76cfc7254902a41b` |
| `browser/wild_buzzard_engine/src/pipeline.rs` | `ca99ce8fd3acf506424b23e5d0cb597b4a11d6426e438a5e8bbe18122b66a39f` |
| `browser/wild_buzzard_engine/tests/dynamic_document.rs` | `926de5cf1b84740125eadc6c6b2e34fb9d2ba65840bced2e42c311531e956120` |
| `browser/wild_buzzard_engine/tests/static_pipeline.rs` | `4026716b9e96b4d1cda07cc2e933eff6a5ffd5aba7474e1821c0b3e6a9764fb0` |
| `gfx/wild_buzzard_headless/src/headless.rs` | `957487f4d52827d9f8dbd2f2b7cf03414ecd0aabf4419fa4582250ba77f3136c` |
| `gfx/wild_buzzard_headless/tests/composed_scene.rs` | `9edec0f9d9c2f6942b9cacd46ece877c8d960886badc63fbf213dab92b298509` |

## Coverage and parity limits

Focused tests cover no-live preflight, exact successful mutation and dense
token mapping, stale-version and later-command atomic rollback, semantic
no-op revision advance, exact/stale rerender, committed style failure and
repair, pre-send renderer-resource rejection plus epoch gap, failed replacement
load retention, and terminal renderer health. Post-send poisoning plus later
engine rejection is proven compositionally by a real headless poison test and
the engine preflight unit rather than a timing-dependent combined test. Failed
replacement retention is exercised with HTTP 404; source ownership places
replacement only after a fully successful frame for every later failure too.

No Firefox mochitest, Web Platform Test, scripted-page behavior, or live-style
invalidation parity is claimed. Mutation limits are per submitted batch;
cumulative document/detached-node growth, scalable journaled mutation,
navigation-generation publication, multi-document ownership, rooted host
bindings, event scheduling, and process isolation remain required before this
boundary can serve normal page script.
