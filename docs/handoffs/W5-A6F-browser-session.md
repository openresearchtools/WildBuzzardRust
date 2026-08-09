# W5-A6F browser session controller handoff

## Outcome

W5-A6F adds the independently locked `browser/wild_buzzard_ui` Rust library. It is a bounded,
renderer-neutral browser-product controller above the existing `NavigationEngine` and Linux event
shell. The corrective freeze replaces the rejected single-latest-navigation model with an exact
per-navigation phase ledger, retains the prior live page until successful replacement publication,
and makes stale pixel leases semantically usable through immutable document metadata without ever
relabeling old pixels.

An R4 hostile rereview found a late-frame rollback defect after the previously recorded matrix: a
still-Committed older navigation could replace a newer Ready page through a generic `EnginePort`.
The session-side monotonic publication correction and generic-port regressions are source-frozen
and statically accepted by R4. The replacement engine/UI matrix passed from the final formatted
tree under a new serialized external build lease; exact evidence and hashes are recorded below.

This is not a Firefox-parity claim. The crate deliberately contains no executable and does not
claim visible browser chrome. W5-A4Q is independently accepted GO as a bounded same-process native
WebRender-window presentation prerequisite. A later integration must still connect this controller
and its `EnginePort` leases to that presenter and build a real chrome/page WebRender scene.

## Added source

- `browser/wild_buzzard_ui/Cargo.toml` and its UI-owned `Cargo.lock`
- `browser/wild_buzzard_ui/src/address.rs`
- `browser/wild_buzzard_ui/src/engine.rs`
- `browser/wild_buzzard_ui/src/input.rs`
- `browser/wild_buzzard_ui/src/lib.rs`
- `browser/wild_buzzard_ui/src/session.rs`
- `browser/wild_buzzard_ui/tests/browser_session.rs`
- `browser/wild_buzzard_ui/tests/navigation_engine_port.rs`
- `browser/wild_buzzard_ui/README.md`
- the minimal first-party metadata/split-shutdown correction in
  `browser/wild_buzzard_engine/src/navigation.rs`

The root workspace manifest and lock were not edited by this lane. Root-workspace admission remains
an orchestrator action.

## Product-state contract

`BrowserSession<E: EnginePort>` owns:

- process-local, nonzero, never-reused window, tab, and top-level-context identities;
- one active tab per live window and independent address/content focus per tab;
- bounded URL-only history with exact current index, forward truncation, and a Firefox-shaped
  default maximum of 50 entries;
- distinct latest-admitted, loading-owner, and retained-live navigation identities per tab;
- an exact per-navigation `Requested -> Started -> Committed -> Ready` phase ledger, with
  `Cancelled` accepted only from Requested/Started and `Failed` only from Started;
- one frame lease, mutation-result lease, independent engine live/frame document revisions, and
  document failure for the retained live page;
- bounded close tombstones until the exact `ContextClosed` generation arrives;
- aggregate retained history and frame-byte accounting plus a session-wide hard cap of 4,096
  fixed-size navigation-ledger entries; and
- one-way `Running -> Closed` or `Running -> Failed` lifecycle with repeatable shutdown reporting.

Default limits are 16 windows, 256 tabs per window, 1,024 total live-or-closing tabs, 1,024 close
tombstones, 50 history entries per tab, 64 MiB aggregate retained history strings, 256 MiB aggregate
retained UI frames, the engine URL byte limit per address editor, and 256 events per pump. Public
construction enforces nonzero hard ceilings: 64 windows, 1,024 tabs per window, 4,096 total tabs or
tombstones, 4,096 history entries, 256 MiB history strings, 1 GiB UI frames, and 4,096 events per
pump.

Address, history, and ledger budgets preflight before engine admission. Ledger admission computes
`global entries - all prunable terminal nonlive entries + 1`; only after the engine returns the
exact expected generation does it prune those entries and insert Requested. Nonterminal entries
are never pruned, and each retained live Ready entry remains present. An event for an absent or
pruned generation in a live context is therefore an after-terminal contract failure. Suppression
is limited to closing or already-retired allocated contexts; a never-allocated foreign identity
fails closed.

Admission alone does not clear the retained live page. Its frame, document revisions, and mutation
result remain routable under the old navigation while a replacement is Requested, Started, or
Committed. Only a valid replacement `FrameReady` promotes the new page and retires the prior Ready
ledger entry. Thus A may remain live, B may finish and queue its frame, and C may be admitted before
the UI pumps B: B still promotes while C owns loading, and C cancellation/failure clears only C's
loading token and leaves B's page state intact. Successful live publication is nevertheless
strictly context-generation-monotone: every candidate must be newer than the retained Ready
navigation. If C publishes first, a later B frame is a terminal rollback fault checked before
lease transfer. Back, forward, and reload use the same rule.

Tab close preflights ownership, accounting, and tombstone capacity before asking the engine to
close. The active adjacent successor is the next tab at the removed index, otherwise the preceding
tab. Closing the final tab closes its window; closing the final window shuts down the session.
Window close preflights all needed tombstones before its first engine side effect. A later rejection
after partial admission is terminal, preventing a half-owned hidden window.

The adjacent close rule is intentionally narrower than Firefox's complete successor/opener policy.
Firefox first considers explicit successor and visible owner metadata before adjacent tabs; this
gate does not yet model those relationships.

## Engine boundary

`EnginePort` is the narrow browser-owned capability for navigation, cancellation, context close,
sequenced fixed-size events, exact frame/result transfer, stale draining, and deterministic
shutdown. `NavigationEnginePort` is the concrete implementation around one inseparably spawned
public `NavigationEngine`/`EngineEventReceiver` pair. There is no public constructor from
independently supplied parts; a compile-fail API proof closes cross-incarnation receiver pairing.

Adapter invariants:

- UI-visible sequence and lease identities are nonzero values and contain no pointers or native
  handles.
- Frame and mutation bindings are independently capped at 4,096 entries; individual UI frame
  descriptors are exact RGBA8 and capped at 256 MiB.
- Native lease high-water marks reject reuse/reordering even after an active binding is retired.
- A composite mutation-result/frame event preflights both registries before committing either.
- `NavigationStarted` validates order but retains older live-page bindings. Only a valid newer frame
  publication retires older bindings for that exact context.
- Frame transfer validates the navigation, public lease, native lease, and descriptor. Mutation
  result transfer validates its exact navigation, public/native lease binding, operation, committed
  live document revision, and created-node count against the event. A binding is removed only after
  successful native transfer, except when the native receiver definitively reports that exact token
  `Stale`. `Unknown` and receiver faults do not silently remove it.
- Draining an old public token can never substitute for or revoke a differently keyed newer lease.
- Context close and terminal shutdown clear the applicable active registries without resetting the
  never-reuse high-water marks.
- Shutdown takes the engine out of the port, contains request, receiver drop, join, and final owner
  drop separately, and retains no engine owner after return even when a contained step panics.
  Receiver destruction precedes the potentially unbounded join and authoritatively clears shared
  queued events, frame/result leases, retained-document metadata, cancellations, and resource
  accounting. The executor-owned live page remains on the worker until executor finalization
  during that join.

The session independently validates a gap-free engine-event sequence and the exact phase of the
event's own navigation, including known older nonterminal entries. Duplicate, out-of-order,
after-terminal, missing-live-ledger, wrong-current-page document, and foreign-context events are
terminal. For `ContextClosed` specifically, only the exact tombstone generation acknowledges a
close; a repeated close for a context identity this session allocated and already retired is
suppressed, while a live context, wrong tombstone generation, or never-allocated foreign context is
terminal. `FrameReady` additionally checks that the retained live identity still owns its exact
Ready ledger entry and that the candidate generation is strictly newer. This preserves B-before-C
publication while preventing B-after-C rollback independently of the concrete adapter's ordering.
After sequence acceptance, every unexpected apply/take/discard error or panic is also terminal and
performs complete cleanup; a partially transferred composite publication is never left in a
running session.

Each first-party frame event now carries its optional `DocumentVersion` in immutable
`FrameMetadata`, and a transferred lease must repeat that exact value. Product `FrameReady`
requires a present, nonzero document identity. This lets an already-superseded initial lease still
promote the exact navigation and establish `live == frame == event version` while suppressing all
pixels. A valid transferred frame must match the same version exactly.

Rendered mutations must continue the exact retained `(live, frame)` pair and advance the same
nonzero document by exactly one revision; their result lease must independently match navigation,
lease, operation, new live version, and created-node count. A valid result plus an exact stale frame
still commits the result and semantic `frame = live` state but clears pixels. A stale result means a
later navigation/invalidation revoked the compound outcome: the paired frame is drained and neither
half applies. Committed-without-frame mutations advance live by exactly one while preserving the
prior frame revision. Rerender success preserves live and sets frame to live. Rejected mutation and
rerender events must repeat the exact unchanged nonzero pair. Same-revision mutation, skipped,
rollback, zero-document, foreign-document, ahead-frame, and colluding payload variants fail closed.

The session's aggregate frame budget is checked before installing transferred pixels. An
over-budget or semantically stale publication drops any prior pixels so they cannot be mislabeled
with the new frame revision. For a committed `DocumentMutationRendered`, a valid result is still
retained, `ResourceLimit` is recorded only for policy suppression, and
`MutationAppliedFrameSuppressed` reports that typed partial outcome. Exact native `Stale` is the
only semantic supersession; `Unknown`, mismatches, receiver faults, generic errors, and panics are
terminal.

Real-worker regressions buffer raw native events without mapping them early, proving both the
A/B/C promotion race and a stale initial `FrameReady` whose document identity survives only in
event metadata. Further real paths cover a queued old-live document mutation during a pending
failed replacement, mutation rejection, rerender success/rejection, stale rerender, and
request/drop-receiver/join resource release. Hostile tables cover every result field, rendered,
resource-suppressed, no-frame, rejected, rerendered, and rerender-rejected document versions, plus
exact stale/unknown/panic composite transfer and discard behavior. A generic-port pair now proves
that B may promote while newer C is merely Committed, while the reverse C-then-B frame order fails
terminal before B's lease is transferred and preserves exact pre-fault ledger accounting.

## Address and Linux input contract

Each tab keeps bounded committed address text, directional byte-indexed selection, dirty state, and
separate bounded IME preedit. All endpoints must be UTF-8 boundaries. Shift movement extends or
contracts from a stable anchor across multibyte scalars; an ordinary arrow collapses a selection to
the edge in its travel direction without stepping past it. Address focus selects all, successful
submission moves focus to content, and Escape reverts a dirty/preedit value before it can act as
Stop.

Linux mapping recognizes the scoped Firefox-shaped physical shortcuts used by this gate: new/close
tab, address focus, back/forward, reload/stop, next/previous tab, and Alt+1 through Alt+9 tab
selection. Layout text comes only from `InputEvent::Text`; physical key events never synthesize
characters. Input sequences are strictly increasing per window but may contain gaps, as coalesced
pointer events do. Unhandled content and chrome input is returned with the exact owned event,
window, tab, and native/synthetic origin for a future downstream router.

Native window lifecycle is one-way and cross-checked through both window-to-surface and
surface-to-window maps. `Ready` cannot reuse a live surface, duplicate readiness is terminal,
`Suspended`/`Resumed` accept only their exact predecessor states, and `Destroyed` removes the exact
reverse mapping. Every later nonterminal event, including duplicate `Destroyed`, is rejected.

## Firefox ESR153 reference evidence

Reference checkout: detached `c19b7e89270787889495688244ec6ee8e79288a1`. No file under
`firefox/` was edited or used as a build/runtime dependency.

Implementation and preference paths inspected:

- `browser/base/content/browser-sets.inc.xhtml` and `browser/base/content/browser-sets.js` for
  browser/tab key bindings;
- `browser/components/tabbrowser/content/tabbrowser.js`, especially `removeTab` and
  `_findTabToBlurTo`, for last-tab close and successor/owner/next/previous selection;
- `browser/app/profile/firefox.js` for `browser.tabs.closeWindowWithLastTab = true`,
  `browser.tabs.selectOwnerOnClose = true`, and `browser.sessionhistory.max_entries = 50`;
- `docshell/shistory/nsSHistory.cpp` and `SessionHistoryEntry.{h,cpp}` for history ownership,
  maximum-entry handling, and truncation; and
- the current URL bar input/tab-switch implementation reached from tabbrowser and URLbar tests.

Tests inspected:

- `browser/components/urlbar/tests/browser/browser_revert.js`;
- `browser/components/urlbar/tests/browser/browser_urlbar_selection.js`;
- `browser/components/urlbar/tests/browser/browser_keepStateAcrossTabSwitches.js`;
- `browser/components/urlbar/tests/browser/browser_focusContentDocumentEsc.js`;
- `browser/components/urlbar/tests/browser/browser_stop.js` and `browser_stop_pending.js`;
- `browser/components/tabbrowser/test/browser/tabs/browser_tabs_owner.js`;
- `browser/components/tabbrowser/test/browser/tabs/browser_tabswitch_select.js`; and
- the tab close/multiselect close tests under
  `browser/components/tabbrowser/test/browser/tabs/`.

History inspected included:

- `c1ce24fd5a9b4683a12c70487eeb094536ad70d1`, “Bug 1836051: Restore selection range when tab
  switching”;
- `1f42ad0209ba0145e54c892a21ee57424aefc65d`, “Bug 1836051: Make the urlbar to revert its
  previous focus state when switching tabs”;
- blame/history for the successor -> owner -> next -> previous close priority in tabbrowser; and
- `a2ba24131be89c861f5ab69d7bc30dcaaf640cbb`, “Bug 1728375 - Notify session history listeners
  when entries are being removed via purging or truncation.”

These references informed observable state transitions only. No Mozilla source was copied.

## Verification

Verification of the R4 rollback correction passed from the final formatted tree with Rust/Cargo
1.96.0. The R4 reviewer statically accepted the generation-monotone guard and paired generic-port
regressions: B may publish while C is pending, but B after C is Ready fails terminal before
transfer. The reviewer also confirmed the corrected shutdown-ownership wording. Every Cargo
resolution/build command ran offline and every generated artifact remained outside the repository:

```text
CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w5-a6f
TMPDIR=/home/user/Documents/wildbuzzardbuilds/w5-a6f/tmp
CARGO_NET_OFFLINE=true                    # Cargo resolution/build commands
```

The repository `.cargo/config.toml` fixed the build target to `x86_64-unknown-linux-gnu`.

The replacement matrix passed:

```sh
cargo fmt --manifest-path browser/wild_buzzard_engine/Cargo.toml -- --check
cargo fmt --manifest-path browser/wild_buzzard_ui/Cargo.toml -- --check
cargo check --manifest-path browser/wild_buzzard_engine/Cargo.toml --locked --all-targets
cargo check --manifest-path browser/wild_buzzard_ui/Cargo.toml --locked --all-targets
cargo test --manifest-path browser/wild_buzzard_engine/Cargo.toml --locked --all-targets
cargo test --manifest-path browser/wild_buzzard_ui/Cargo.toml --locked --all-targets
cargo clippy --manifest-path browser/wild_buzzard_engine/Cargo.toml --locked --all-targets -- -D warnings
cargo clippy --manifest-path browser/wild_buzzard_ui/Cargo.toml --locked --all-targets -- -D warnings
RUSTDOCFLAGS='-D warnings' cargo doc --manifest-path browser/wild_buzzard_engine/Cargo.toml --locked --no-deps
RUSTDOCFLAGS='-D warnings' cargo doc --manifest-path browser/wild_buzzard_ui/Cargo.toml --locked --no-deps
RUSTDOCFLAGS='-D warnings' cargo test --manifest-path browser/wild_buzzard_engine/Cargo.toml --locked --doc
RUSTDOCFLAGS='-D warnings' cargo test --manifest-path browser/wild_buzzard_ui/Cargo.toml --locked --doc
```

Results from that exact tree:

- engine and UI formatting checks passed;
- engine and UI locked all-target checks passed without warnings;
- engine tests passed 55/55: 11 library, 8 dynamic-document, 33 navigation-facade, and 3 static
  pipeline tests;
- UI tests passed 54/54: 23 library, 30 browser-session integration, and 1 real navigation-port
  integration test;
- engine and UI all-target Clippy passed with `-D warnings`;
- engine and UI no-dependency rustdoc passed with `-D warnings`; and
- engine doctests passed 0/0 while the UI compile-fail API proof passed 1/1.

The first strict UI lint after the R4 edit reported only that `apply_frame_event` crossed the
pedantic 100-line threshold. The already accepted phase/live monotonic preflight was extracted into
a focused helper without changing its pre-transfer order. UI formatting, check, all 54 tests, and
both engine/UI strict lint gates were rerun before the final documentation gates above.

Frozen source SHA-256 values:

| Path | SHA-256 |
| --- | --- |
| `browser/wild_buzzard_engine/src/navigation.rs` | `3318b42d93d21189e89f232925b0182005f5faf2491a16f212545e9ee72556e2` |
| `browser/wild_buzzard_ui/Cargo.toml` | `d105fd8b092281b1615811d30900ca261e567691d84a7494ba967310a5f5d50e` |
| `browser/wild_buzzard_ui/Cargo.lock` | `94dccfe598213c0cded133aaf9ed834e01ede7556369c683eac15a79fddf4452` |
| `browser/wild_buzzard_ui/README.md` | `c15844faf8839edc463dc5d24e76cc9080512b66c3d9c69689cebf918a32b177` |
| `browser/wild_buzzard_ui/src/address.rs` | `ef2751fcd66bb17f0e23645d2f694aedb14bdec3c3471704594a7746ef5b98b7` |
| `browser/wild_buzzard_ui/src/engine.rs` | `ca4ed1537f701771776830ff664072a0e84c2f09b4f00166992a07845fbf4949` |
| `browser/wild_buzzard_ui/src/input.rs` | `2e82ae4e049d5b420d4ac126f4f45b398ed9800401094d096e5200243deeca3c` |
| `browser/wild_buzzard_ui/src/lib.rs` | `f073d49b25dc1b95fc9f8979ef6fe55142127d6219bc1cb500662fffb84188c6` |
| `browser/wild_buzzard_ui/src/session.rs` | `116e86ac305f2d9a4f537fbd658d4523a37a412f8e7d6d2292c1c9cb357ea6cc` |
| `browser/wild_buzzard_ui/tests/browser_session.rs` | `5af6bfcf02d3b6cc1cc781e6511135d1e1a6c38b15d7d56cd4c8bd21614413a4` |
| `browser/wild_buzzard_ui/tests/navigation_engine_port.rs` | `292cbe72a4359677ffa4db153b7e20da33676d82ff370ede40cbc6026f75eacb` |

The earlier 52-test UI evidence and hashes are superseded by this table. The lane did not edit the
root workspace manifest or root lock; the independently locked UI workspace resolved unchanged
under `--locked` and offline mode.

## Explicit non-claims and next work

This gate does not provide browser chrome rendering, a runnable shell, a W5-A4Q presenter
connection, page-input dispatch, navigation URL parsing/search fixup, persisted sessions,
same-document history, BFCache, scroll/form restoration, opener/successor tab metadata, private
windows, prompts, downloads, permissions, accessibility, process isolation, WebDriver, devtools,
or Firefox parity. The current browser-facing port also has no public document-mutation admission
API and therefore no `DocumentInvalidated` producer for silently revoked in-flight dynamic work;
future exposure must add that explicit signal before claiming end-to-end dynamic-page routing.
Shutdown has no deadline and can block forever when an executor ignores cancellation.

With W5-A4Q independently accepted, the next browser-shell integration must retain the separation
of authority: this controller owns product state; `NavigationEnginePort` owns engine event/lease
transfer; the presenter owns native surface/WebRender presentation; and a new chrome scene/input
router must explicitly connect them without exposing native handles or engine internals through
this crate.

## Provenance

All new code is first-party Rust under MPL-2.0. The gate adds no `unsafe`, C/C++, FFI, native
library, endpoint, credential, telemetry, Mozilla artwork, branding, or imported source.
