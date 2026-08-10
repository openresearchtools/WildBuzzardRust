# W9-A6I general-web top-level navigation handoff

- Task: W9-A6I
- Owner: Agent 6 integration lane
- Status: Complete for the bounded general-web-to-frame vertical described here; not general-site or Firefox navigation parity
- Product target: Linux x86-64 only
- Firefox reference: ESR153 detached full-history checkout at `c19b7e89270787889495688244ec6ee8e79288a1`
- Build root: `/home/user/Documents/wildbuzzardbuilds/w9-a6i-general-navigation/`

## Observable result

An explicit `NavigationRequest::general_web` now enters the existing
`NavigationEngine` worker, uses the reviewed `GeneralWebClient`, and delivers
the bounded response body into the existing HTML -> DOM snapshot -> Stylo ->
layout/text -> WebRender scene -> headless RGBA8 or presentation-scene path.
The caller/UI thread performs none of URL validation, DNS, TCP, TLS, HTTP body
I/O, parsing, style, layout, or rendering. The client and its resolver are
constructed, used, and destroyed from the worker-owned executor factory.

This is a distinct authority, not a widening of the numeric-loopback client:

- `NavigationRequest::new` means `NavigationNetworkCapability::NumericLoopback`.
- `NavigationRequest::general_web` means
  `NavigationNetworkCapability::GeneralWeb`.
- `NavigationEngine::spawn` and `spawn_for_presentation` construct executors
  which accept only numeric-loopback requests.
- `NavigationEngine::spawn_general_web` and
  `spawn_general_web_for_presentation` require explicit `GeneralWebConfig` and
  `TrustStore` and accept only general-web requests.
- `StaticPageEngine` has matching direct constructors and load entry points;
  crossing those capabilities fails before network access.

No manifest or lockfile change was required. `wild_buzzard_engine` already had
a direct path dependency on `wild_buzzard_net`, whose public W8-A5 types provide
all required transport contracts.

## Control and publication invariants

One absolute operation deadline and one cancellation token now span DNS,
connection, TLS, response-head/body I/O, and all later page stages. A transport
error is projected to `PipelineError::Cancelled { Fetch }` when cancellation is
authoritative, or `PipelineError::DeadlineExceeded { Fetch }` when the absolute
deadline elapsed. Per-operation socket inactivity timeouts which occur before
the absolute deadline remain ordinary network failures.

The existing worker state machine remains authoritative after synchronous
execution. It rechecks the context-local generation under the publication lock,
so a superseded general-web load emits `NavigationCancelled` and can never emit
`NavigationCommitted` or `FrameReady`. The executor's existing old/new live-DOM
transaction restores the previous context document whenever a result is stale
or rejected.

Successful general-web bytes use the exact same parser, DOM, style, layout,
scene, font, frame, retained-node, retained-frame, cancellation, and deadline
bounds as the loopback path. This gate adds no JavaScript execution, DOM event
loop, cookie/cache state, external resource loading, image/media decode, or
provider endpoint.

The transport authenticates HTTPS and internally returns exact TLS/ALPN
security metadata, but the current engine event/frame/session contract does not
carry that metadata. Consequently this slice is engine-vertical evidence, not
permission for the product UI to display a lock/security identity yet.

## Redirect decision

Redirect following is deliberately not claimed in W9-A6I. The reviewed
transport can return a manual 301/302/303/307/308 response, its bounded
`Location` header, and a validated WHATWG URL. The current browser-session event
and site-identity contract, however, publishes only requested-navigation
identity and HTTP status; it cannot publish the final redirect URL, redirect
chain, or final connection-security identity. Following while the address and
security UI remain bound to the requested URL would be false success.

The general path therefore uses `RedirectPolicy::Manual` and returns the public
typed blocker `PipelineError::RedirectBlocked { status }` before parsing any
redirect response body. The worker maps it to fixed-size `Rejected` at
`NavigationStage::Fetch`. A later redirect slice must atomically add final URL,
security metadata, redirect count/loop state, session history semantics, and
site-identity publication before enabling following.

Firefox ESR153 material inspected read-only:

- `netwerk/protocol/http/HttpBaseChannel.cpp`, especially
  `HttpBaseChannel::CheckRedirectLimit`;
- `netwerk/protocol/http/nsHttpHandler.h`, whose default ordinary redirect
  limit is 10;
- `netwerk/test/unit/test_redirect_loop.js`, covering absolute, relative, and
  empty-Location loops and expecting `NS_ERROR_REDIRECT_LOOP`.

No file inside `firefox/` was modified or used as a build input.

## Deterministic acceptance evidence

`tests/general_navigation.rs` covers:

1. `http://localhost:<ephemeral>/search/index.html?q=rust` through Linux system
   DNS and the general-web client at 1366×768. A 200 body reaches a composed
   frame with exact viewport/stride/length, author masthead and search-panel
   colors, shaped text, a nonempty document version, and no fake clear frame.
2. `https://localhost:<ephemeral>/page.html` at 1920×1080. A test-only OpenSSL
   TLS 1.2/1.3 server uses a fresh localhost SAN certificate; the exact DER
   certificate is added to the otherwise bundled-root `TrustStore`. The same
   response produces a visible full-HD frame. Certificate verification is not
   disabled or replaced. Temporary certificate/private-key/page files are
   created under `CARGO_TARGET_DIR` and deleted by the fixture.
3. A response-head stall superseded by a newer generation. The stale operation
   reports cancellation, publishes no commit/frame, and only the replacement
   generation returns a visible 1366×768 frame.
4. A response-head stall beyond the caller's absolute 120 ms operation
   deadline, which remains exactly `DeadlineExceeded` at `Fetch` rather than
   becoming a generic network error.
5. Direct capability mismatch and a 302 response, which return typed failures
   without socket widening, HTML parsing, live-DOM replacement, or fake
   navigation success.

The OpenSSL process exists only as deterministic local TLS test infrastructure;
it is not linked into the browser, used for browser verification, or added as a
runtime dependency.

## Public-site probe and exact downstream blocker

An ignored opt-in assertion requests `https://example.com/` through the same
worker and requires a visible 1366×768 frame. It was run once on 2026-08-10.
Authenticated DNS/TCP/TLS/HTTP, bounded body delivery, HTML parsing, and DOM
snapshot creation succeeded. The navigation then failed honestly at Style.

A direct diagnostic run exposed the exact source:

```text
Style(UnsupportedComputedValue {
  value: AutomaticMargin("margin-right"),
  ...
})
```

The current example.com stylesheet contains `body { width:60vw; margin:15vh
auto; ... }`. W9-A6I does not own `layout/` or the Stylo adapter and did not
strip that declaration, substitute a site-specific page, or fabricate a frame.
The ignored test remains a visible-frame assertion so the blocker becomes a
real pass when automatic-margin layout is implemented.

No Google, DuckDuckGo, or YouTube render claim is made. They additionally need
redirect/final-URL publication, external stylesheets/resources, broader layout,
images, browser JS/DOM bindings, media, and other open platform work.

## Validation

All deterministic commands used the pinned engine lock and external build
root. The accepted matrix is:

- `cargo fmt --check`: passed.
- `git diff --check` for owned paths: passed.
- workspace all-target `cargo check --locked --target
  x86_64-unknown-linux-gnu`: passed.
- strict workspace all-target Clippy, `--no-deps -- -D warnings -W
  clippy::all -W clippy::pedantic`: passed.
- full deterministic workspace tests, serial execution: 61 passed, 0 failed,
  1 ignored.
- focused general-navigation suite: 5 passed, 0 failed, 1 ignored.
- ignored public example.com test: reached Style, then failed on the exact
  automatic-margin blocker above; it is not counted as a passing gate.
- release workspace build for `x86_64-unknown-linux-gnu`: passed. The imported
  WebRender dependency retains its pre-existing `frame_id` dead-code warning;
  the engine emitted no warning and strict no-dependency Clippy remained clean.
- warning-denied workspace rustdoc with `--no-deps`: passed.

## Owned files

- `browser/wild_buzzard_engine/src/error.rs`
- `browser/wild_buzzard_engine/src/lib.rs`
- `browser/wild_buzzard_engine/src/navigation.rs`
- `browser/wild_buzzard_engine/src/pipeline.rs`
- `browser/wild_buzzard_engine/tests/general_navigation.rs`
- `browser/wild_buzzard_engine/README.md`
- `docs/handoffs/W9-A6I-general-navigation.md`

No manifest, lockfile, network crate, layout, Stylo, graphics, shell/UI,
program-status, or JavaScript file was edited by this task.

## Downstream work required

1. Implement automatic physical margins and center resolution in the layout
   and Stylo-adapter lane; rerun the ignored example.com assertion at 1366×768.
2. Add typed final-URL, redirect-chain/count, connection-security, history, and
   address/site-identity publication; then implement a bounded redirect
   algorithm (Firefox ESR default ordinary limit: 10) with absolute, relative,
   empty-Location, cross-origin, downgrade, cancellation, deadline, and loop
   regressions.
3. Wire `NavigationRequest::general_web` and a general-web presentation engine
   into the browser session/shell together with authenticated connection
   metadata. Until that explicit product change, the existing shell remains
   numeric-loopback-only and must not claim HTTPS site identity.
4. Add external stylesheet/resource fetch ownership, MIME/encoding handling,
   images, forms and input events, Brimstone browser host bindings/event loop,
   media, storage, cache/cookies, proxy, HTTP/2/3, process isolation, and
   security/site-identity propagation.
5. Compare Wild Buzzard and Firefox ESR at 1366×768 and 1920×1080 using actual
   screenshot/layout evidence. This slice has deterministic browser-shaped
   fixtures, not visual parity evidence for a public site.
6. Define the browser's address/origin policy above the raw general transport,
   including private/local-network decisions and DNS-rebinding-aware process
   isolation. W9-A6I accepts only an explicit top-level request and sends no
   credentials, but it is not that complete policy boundary.

## Frozen source hashes

| File | SHA-256 |
| --- | --- |
| `browser/wild_buzzard_engine/src/error.rs` | `99b496b793bd19e634486ff1a87ce8509ad7fb0a315c503cd367dd3bfd703fce` |
| `browser/wild_buzzard_engine/src/lib.rs` | `3d479f0ef60c56bddbce54250b2d0c00c6c628f3c0e74763bf4def3c0e58a9c3` |
| `browser/wild_buzzard_engine/src/navigation.rs` | `1125055ce7ca311ffc8cb2e3926451bac7f19bb903837f303b61ce174d264d13` |
| `browser/wild_buzzard_engine/src/pipeline.rs` | `f7bd5902fa1e6ec80b8cdbe94667ab0807acea4dfcc164f9cfa37c06c528d710` |
| `browser/wild_buzzard_engine/tests/general_navigation.rs` | `45fbc9271fb7f6058fa4a887fa2eee233f53051c43cd05494bd16125b7833b42` |
| `browser/wild_buzzard_engine/README.md` | `df29e154a8d267b72c884688f33f02842d1a9a910fcf64b2b5421c848e258a22` |

The handoff omits its own self-referential hash. `sha256sum` was rerun after
the final source, test, and README freeze.
