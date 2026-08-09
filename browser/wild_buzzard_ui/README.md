# Wild Buzzard browser session controller

`wild_buzzard_ui` is the first bounded browser-product controller above the
existing `NavigationEngine`. It owns window, tab, browsing-context, history,
address-editing, engine-event, and shutdown state without importing DOM,
layout, networking, renderer, EGL, winit, or native-handle internals.

This workspace intentionally has no executable. W5-A4Q is independently
accepted GO as a bounded same-process native WebRender-window presentation
prerequisite; a later integration must connect this controller and its
`EnginePort` leases to that presenter and to a real browser-chrome scene. This
crate does not claim that connection or visible browser UI already exists.

The controller currently provides:

- bounded multi-window and multi-tab ownership with never-reused nonzero IDs;
- exactly one active tab in every live window;
- per-tab bounded history, typed navigation generations, and retained address
  drafts/UTF-8 selections across tab switches;
- new, close, activate, address navigation, back, forward, reload, and exact
  stop commands;
- a narrow `EnginePort`, including a concrete adapter which owns the public
  `NavigationEngine` and `EngineEventReceiver` pair;
- generation-checked frame and mutation-result lease transfer and safe stale
  draining;
- a session-wide 4,096-entry navigation phase ledger, with exact per-generation
  Requested/Started/Committed/Ready/Cancelled/Failed ordering;
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

It is not browser chrome, session persistence, BFCache, process isolation,
WebDriver, accessibility, page input routing, error-page UI, or Firefox parity.
