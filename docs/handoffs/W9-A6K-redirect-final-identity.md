# W9-A6K: redirect and final-navigation identity handoff

## Outcome

W9-A6K closes the first bounded top-level HTTP redirect and final-identity
vertical from the general-web transport through browser history/address state.
It targets ordinary Linux desktop navigation at 1366×768 and 1920×1080; no
tiny-window behavior or public-site parity claim is part of this gate.

The implementation:

- follows 301, 302, 303, 307, and 308 iteratively under the exported
  `MAX_TOP_LEVEL_REDIRECTS` value of 10;
- reuses one cancellation token and one absolute operation deadline across the
  entire chain;
- does not read intermediate response bodies;
- rejects loops, excess hops, unsupported 3xx statuses, missing/multiple/non-UTF-8
  `Location`, credentials, non-HTTP(S) targets, malformed URLs, and oversized
  final identities with typed failures;
- retains WHATWG-normalized fragments in navigation identity, including initial
  fragments and Fetch-style inheritance when `Location` omits a fragment, but
  derives an otherwise exact fragment-free transport target for every request;
- records the normalized final URL, redirect count, final cleartext or exact
  TLS-version/ALPN evidence, and a sticky HTTPS-to-HTTP downgrade bit; and
- publishes that variable-sized record through an exact-navigation one-shot
  transfer installed before the existing fixed-size `NavigationCommitted`
  event, preserving the engine event ABI.

The concrete UI engine port consumes the engine transfer before returning the
commit event and binds it to the exact `NavigationId`. The browser session then
updates every exact matching history slot, including a noncurrent slot, while
changing visible address text only when that exact slot remains current and its
editor has neither a dirty draft nor an active IME preedit. Escape later reverts
either preserved edit state to the newly committed final history URL.
Back/forward and reload therefore request the committed final URL. A commit
event outside HTTP 200–299 is terminal before phase or history success. Missing,
foreign, duplicate, or stale concrete general-web commitments fail closed.
Legacy deterministic numeric-loopback ports may omit the extension; only that
narrow capability can synthesize cleartext, zero-redirect metadata, and it can
never synthesize TLS.

`HistoryCommitState`, `history_entry_commit`, and `TabSnapshot::history_commit`
expose exact retained connection/redirect facts. Chrome classification no
longer derives security from URL text. Cleartext HTTP is local/insecure as
appropriate. `AuthenticatedTls` remains conservatively `Unverified` because
the current writable primary-UI enum has no secure state; W9-A6K therefore does
not display or claim a lock.

## Architecture and invariants

- `GeneralWebTarget::parse_navigation` and `from_navigation_url` are the narrow
  boundary between browser URL identity and HTTP request identity. The former
  retains the fragment; the latter is fragment-free and remains the only type
  consumed by `GeneralWebClient`.
- Redirect loop keys and URL bounds use the normalized browser identity,
  including fragments. The ten-hop cap independently prevents fragment churn
  from bypassing a resource bound.
- Authenticated HTTPS evidence comes only from the network response security
  object and is checked against the exact request scheme. URL spelling never
  creates TLS evidence.
- Product general-web session admission calls the engine-owned
  `NavigationCommitMetadata::validate_general_web` before changing history or
  address state. It rejects invalid, credentialed, non-HTTP(S), noncanonical,
  over-limit, unverified, and scheme/security-incoherent records. This proves
  structural coherence, not authenticity: `NavigationEnginePort` is the trusted
  engine-to-UI ownership seam, while privileged custom ports can fabricate a
  coherent record.
- A `NavigationCommitted` event whose status is outside HTTP 200–299 is an
  engine-contract failure before commitment transfer, navigation-phase advance,
  history mutation, or subsequent frame admission.
- The downgrade bit is sticky: once a successfully authenticated HTTPS
  response redirects to HTTP, later hops cannot erase that fact.
- A commit record is inserted under the worker publication lock before the
  commit/frame event pair. An exact transfer removes only its own key. A
  foreign transfer cannot consume it, and a second transfer is stale/unknown.
- UI retained-history byte accounting is recomputed before replacing a
  requested URL with a differently sized final URL. A final identity which the
  configured session cannot retain is terminal rather than silently truncated.
- An exact current-slot commitment does not overwrite address text, selection,
  dirty state, or IME preedit while the editor owns a draft or composition. The
  history URL and typed commitment still advance, so Escape has the correct new
  baseline.
- The engine and UI share the exported redirect limit; there is no duplicated
  numeric policy that can drift across the boundary.

## Independent review corrections

The first independent review returned **NO-GO**. This rereview freeze corrects
all six findings rather than carrying the first-ready claim forward:

- dirty drafts and active preedits now survive an exact current-slot commit,
  including selection and dirty state, with Escape reverting to the final URL;
- hostile 302 and 404 commit events fail before `Committed`/`Ready` success;
- component documentation names `NavigationEnginePort` as the authenticity seam
  and does not claim structural validation authenticates arbitrary ports;
- deterministic integration executes 301 and 303 together with 302/307/308,
  checks every exact wire target remains GET, and covers every requested typed
  redirect rejection without document publication;
- the obsolete pipeline reference to a redirect blocker is removed; and
- the frozen inventory includes the orchestrator-owned
  `tests/general_navigation.rs` integration adjustment and all 17 scoped paths.

## Firefox ESR reference

The read-only `firefox/` checkout remained at the pinned ESR153 commit
`c19b7e89270787889495688244ec6ee8e79288a1`. Relevant implementation and test
paths inspected were:

- `netwerk/protocol/http/HttpBaseChannel.cpp` (`CheckRedirectLimit`) and
  `nsHttpHandler.h` (default limit 10);
- `netwerk/protocol/http/nsHttpChannel.cpp` redirect processing and replacement
  channel setup;
- `netwerk/ipc/DocumentLoadListener.cpp` redirect-chain publication;
- `docshell/base/nsDocShell.cpp` final-channel URI publication before current
  URI/session-history updates;
- `netwerk/test/unit/test_redirect_loop.js` for absolute, relative, and empty
  redirect loops;
- `docshell/test/navigation/test_session_history_on_redirect.html` for final URL
  history traversal; and
- `security/manager/ssl/nsNSSCallbacks.cpp` plus site-identity tests for the rule
  that security state comes from the authenticated channel, not URL text.

Firefox source/history was reference-only and is not a build input.

## Deterministic evidence

All build products, generated Stylo Python state, TLS fixture files, and Cargo
outputs remain under:

```text
/home/user/Documents/wildbuzzardbuilds/w9-a6k-redirect-identity/
```

The frozen matrix uses locked, offline dependencies and
`x86_64-unknown-linux-gnu`. Explicit-file `rustfmt --check` and the Git
whitespace-error check pass. Package/workspace check, no-dependency clippy with
warnings denied, full tests, doctests, and rustdoc with warnings denied produced:

- `wild_buzzard_net`: 61 passed, 1 deliberately ignored public-network test;
- isolated `wild_buzzard_engine` workspace: 67 passed, 1 deliberately ignored
  public comparison test;
- isolated `wild_buzzard_ui` workspace: 82 passed, including 2 compile-fail
  doctests.

The network package itself is clippy-clean under `clippy::all` and
`clippy::pedantic`. A diagnostic root-workspace clippy attempt also reached
this package cleanly but is not an accepted gate: it encounters hundreds of
existing denied warnings in imported `qcms`, the vendored URL crate, and
libpref. No
files in those unrelated components were changed to mask that debt.

The engine suite proves:

- 301/302/303/307/308 following through exact GET wire targets, an initial
  fragment, relative fragmentless inheritance, absolute fragment replacement,
  later fragmentless inheritance, and exact final identity at 1366×768;
- typed missing, multiple, non-UTF-8, credentialed, non-HTTP(S), unsupported
  3xx, and bounded overlong-location failures with no static live document,
  navigation commitment, or frame event;
- foreign and duplicate one-shot transfer behavior;
- self-loop rejection and failure before requesting hop 11;
- cancellation and the original absolute deadline while stalled after a hop;
- exact TLS 1.3/HTTP-1.1 ALPN evidence at 1920×1080; and
- sticky authenticated-HTTPS-to-cleartext downgrade evidence through a local
  TLS redirect into a deterministic HTTP final page; and
- structural rejection of invalid, credentialed, non-HTTP(S), noncanonical,
  scheme/security-incoherent, unverified, and excess-hop metadata.

The UI suite proves exact current and noncurrent history replacement, visible
address isolation, dirty-draft and active-preedit preservation across a history
commit, Escape against the new final URL, retained-byte accounting, reload of
the final URL, typed history evidence, missing general commitment failure,
hostile 302/404 rejection before phase/history/frame success, concrete port
one-shot binding, conservative TLS chrome, and insecure downgrade projection.

## Frozen implementation inventory

The lane was frozen against worktree `HEAD`
`b10c4e47f773e4f627fc17d26b598df903b7dbb9`. The following SHA-256 values name
the exact W9-A6K implementation, test, and component-document files handed to
the orchestrator. The handoff file itself is intentionally excluded from its
own recursive inventory; its hash accompanies the orchestrator message.

The tracked lane diff is 1,672 insertions and 157 deletions across 15 existing
files. The new redirect integration test and this handoff are 905 and
223 lines, respectively. Together those 15 tracked files and two
new files are the complete 17-path review scope.

| File | SHA-256 |
| --- | --- |
| `browser/wild_buzzard_engine/README.md` | `3ce68184b2b99002622a533697317b3ed4f06fc5fb245895251137769d58b0e0` |
| `browser/wild_buzzard_engine/src/error.rs` | `b5a2e25371608761a82276c997942331fdb5bdf52e36dea92f79fc71400949c1` |
| `browser/wild_buzzard_engine/src/lib.rs` | `31d874bfaf4b86d1e5a6776a9591797869b7667f31802bfae80442cde836e4f0` |
| `browser/wild_buzzard_engine/src/navigation.rs` | `c53db14e8033f41f568c16e90c5cce7fc25095c0effdc08070c0f656987f2e49` |
| `browser/wild_buzzard_engine/src/pipeline.rs` | `539917dfb5ad0f6065086c28338af6420bfa3d64f5f63c2db838155da37efe94` |
| `browser/wild_buzzard_engine/tests/general_navigation.rs` | `f53f91beb2277b103bc32f07c9b52978b68f75a14288c87d009ccabd673a99cd` |
| `browser/wild_buzzard_engine/tests/redirect_navigation.rs` | `7a2ee5afce9445ce6947b0e1d612dd1aa3b998bdb8326748fc9b27dc2c332593` |
| `browser/wild_buzzard_ui/README.md` | `dea8e85773bf4e30c06f6a2a95216b360a201e69aa9a426a09cfe3820a1ce24b` |
| `browser/wild_buzzard_ui/src/engine.rs` | `8362fedf27d17a0efa79bfa26b193af82928b270292e3041904f68f29faf1e23` |
| `browser/wild_buzzard_ui/src/lib.rs` | `3fd4e5919cafef5011ed00a5aa848a8579cd264795098dd4924d19fe95c3c72c` |
| `browser/wild_buzzard_ui/src/session.rs` | `bbc43bc4f255f2cab90e15639a3ac1638568a6f12f0fac56d1e4202f86b080ca` |
| `browser/wild_buzzard_ui/src/session/primary_ui_controller.rs` | `e626e9b00e5e6037d3810971b093e4fd8587d29065e0f3a0fa4ebfdafb35d93f` |
| `browser/wild_buzzard_ui/tests/browser_session.rs` | `f283140fca22e4d86fb9daa28ba914146bd11e3353d6a087bc626fedc7902e16` |
| `browser/wild_buzzard_ui/tests/navigation_engine_port.rs` | `0e6d69d65c53519446b9738d1f1c591c1da0a3c2d61d0834740d18ce66450e55` |
| `netwerk/rust/wild_buzzard_net/src/target.rs` | `872c1c9a96604321ebd1c2ee740725e4a142713cb061c67a17c68cffd2d20e2c` |
| `netwerk/rust/wild_buzzard_net/src/general/tests.rs` | `1f7084ba5629f35ea3dc7324927f33bd4d832f5b1085a201dee80cce543858fd` |

## Deliberate remaining work

- Redirect method/body rewriting is currently harmless only because this gate
  admits top-level GET. POST/form navigation needs Fetch-compatible method and
  body rules before activation.
- Redirect response CSP, referrer, credentials mode, cookie processing, mixed
  content, HSTS/upgrades, service workers, cache, and process/origin policy are
  not implemented here.
- The current UI can retain authenticated TLS facts but cannot present a secure
  identity state. Adding one requires a separately reviewed primary-UI API and
  certificate/site-information model; URL scheme must not be used as a proxy.
- Same-document fragment traversal without a network response, scroll-to-fragment,
  and BFCache/session persistence remain separate browser-navigation work.
- This deterministic fixture evidence is not Google, DuckDuckGo, YouTube, or
  Firefox layout parity. Those comparisons belong to broader rendering and
  browser-product gates.
