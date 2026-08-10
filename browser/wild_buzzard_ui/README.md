# Wild Buzzard browser session controller

`wild_buzzard_ui` is the first bounded browser-product controller above the
existing `NavigationEngine`. It owns window, tab, browsing-context, history,
address-editing, engine-event, and shutdown state without importing DOM,
layout, networking, renderer, EGL, winit, or native-handle internals.

This workspace intentionally has no executable. The separate
`browser/wild_buzzard_shell` crate connects this controller's `EnginePort`
leases to the Linux WebRender presenter and functional Rust browser chrome.
This controller still does not own native-window, renderer, or compositor
internals.

The controller currently provides:

- bounded multi-window and multi-tab ownership with never-reused nonzero IDs;
- exactly one active tab in every live window;
- per-tab bounded history, typed navigation generations, and retained address
  drafts/UTF-8 selections across tab switches;
- new, close, activate, address navigation, back, forward, reload, and exact
  stop commands;
- a narrow `EnginePort`, including a concrete adapter which owns the public
  `NavigationEngine` and `EngineEventReceiver` pair;
- exact one-shot final-navigation commitment transfer keyed by
  `NavigationId`; the current and any matching noncurrent history slot receive
  the normalized final URL, redirect count, downgrade bit, and typed connection
  evidence before frame publication, while a foreign/duplicate/missing
  general-web commitment fails closed; engine-owned canonical HTTP(S),
  credential, redirect-bound, and scheme/security validation rejects
  structurally incoherent records but does not authenticate an arbitrary
  `EnginePort` implementation;
- `NavigationEnginePort` is the trusted authenticity seam for final URL and
  transport evidence. A custom port can fabricate internally coherent metadata
  and must therefore be treated as privileged embedding code, not untrusted
  page input;
- history traversal and reload use the committed final URL, and only the exact
  current history identity may replace visible address text. A dirty address
  draft or active IME preedit survives that history update until Escape reverts
  to the new final history URL. The current chrome conservatively projects
  authenticated TLS as `Unverified` because its public identity enum has no
  secure state, while cleartext and downgrade results can never display as
  secure;
- an explicit session-wide network authority: the deterministic numeric
  loopback mode remains the default for existing tests, while the product can
  select general HTTP/authenticated HTTPS consistently for address entry,
  history traversal, and reload; the concrete engine rejects a mismatched
  request/worker capability;
- generation-checked frame and mutation-result lease transfer and safe stale
  draining;
- a session-wide 4,096-entry navigation phase ledger, with exact per-generation
  Requested/Started/Committed/Ready/Cancelled/Failed ordering; a custom
  `NavigationCommitted` event carrying any status outside 200–299 is terminal
  before commitment transfer, phase success, history mutation, or frame
  publication;
- per-tab retained-live navigation plus exact engine document/live/frame
  revision tracking while a newer replacement remains pending;
- aggregate retained-frame accounting, including a typed outcome which keeps a
  committed mutation result when the corresponding UI frame exceeds policy;
- successful replacement publication, rather than admission, atomically
  retires the prior live page; cancellation/failure leaves its frame,
  document, and mutation-result capabilities intact;
- successful live publication is strictly generation-monotone per context: B
  may publish over A while newer C is pending, but after C publishes a late B
  frame is terminal before its lease can transfer;
- semantic document publication from fixed event metadata even when an exact
  one-shot pixel lease has already become stale, while stale pixels are never
  relabeled as the newer document;
- independent per-tab address/content focus, Firefox-shaped physical-key
  shortcuts, directional UTF-8 selections, IME preedit, and Escape revert;
- typed Linux window/input shortcut and IME routing; and
- exact ownership return for content input and unmapped chrome input until a
  downstream content/chrome router exists; and
- panic-contained, idempotent engine shutdown on explicit exit, last-window
  close, failure, and drop. Receiver destruction releases shared event queues,
  frame/result leases, retained-document metadata, and resource accounting
  before join; an executor-owned live page remains on the worker until
  executor finalization during that join.

Shutdown has no deadline; an executor which ignores cancellation can still
block the worker join indefinitely.

It is not session persistence, BFCache, process isolation, WebDriver,
accessibility, page input routing, error-page UI, or Firefox parity.
