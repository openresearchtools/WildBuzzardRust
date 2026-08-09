# Static-page vertical-slice contract

This document fixes the ownership and data flow for milestone M1 and records the bounded W2-A6
integration proof. The independently locked `browser/wild_buzzard_engine` crate now runs numeric
loopback HTTP, UTF-8 HTML parsing, immutable DOM, imported Stylo, layout with Rust-shaped metrics,
scene compilation, and real Linux EGL/WebRender readback in one synchronous operation. It returns
page decorations and one shaped-glyph proof as separate frames. W2-A6N wraps that current executor
in a typed bounded worker/event/lease facade with generation-based stale-publication suppression;
it is not a window, UI, or complete product-navigation contract. Separately, W2-A4D's graphics path
can produce one composed zero-pending-text frame with an independently accepted exact-scene token,
but the engine does not call it. M1 therefore remains in progress.

The supported product target is only `x86_64-unknown-linux-gnu`. Tests use numeric loopback
addresses and all build, screenshot, and AppImage output belongs under `../wildbuzzardbuilds/`.

## One-way component flow

```text
browser navigation command
  -> engine facade
  -> web-platform document loader and policy
  -> bounded network request/response stream
  -> encoding decoder and incremental HTML parser
  -> owned DOM and immutable revisioned snapshot
  -> Stylo adapter and computed-style snapshot
  -> immutable layout output
  -> validated renderer scene and WebRender built display list
  -> Linux headless/window renderer and presented frame
  -> engine navigation/frame event
  -> browser UI
```

The browser product must not import private DOM, parser, layout, network, Stylo, or WebRender
implementation modules. The renderer must never retain DOM nodes or mutable layout data. The
JavaScript runtime joins later through rooted host bindings and document-task queues, not by giving
the UI a scripting back door.

W2-A6 is deliberately a narrow in-process integration seam rather than the product facade shown
above. It calls only public component contracts, owns no duplicate parser/style/layout/renderer
logic, and publishes an owned `RenderedStaticPage` result. W2-A6N subsequently puts that executor
behind typed navigation identities, bounded commands/events, generation checks, and opaque frame
leases without changing the synchronous pipeline result. Browser UI code must not consume its
component internals directly.

## Owner contracts

### Browser product and engine facade

The eventual product contract needs a typed navigation command containing a navigation identity,
URL text, viewport, top-level browsing-context identity, cancellation handle, and explicit
privacy/session context. It must publish typed provisional-start, response, committed-document,
frame-ready, failure, and cancelled events. Events carry stable identities and owned summaries;
they do not carry process-local pointers or component-private objects.

W2-A6N is the accepted bounded in-process subset around the current executor. It provides opaque
nonzero top-level-context IDs, monotonic context-local generations, bounded URL/command/event/context
and retained-frame resources, `Navigate`/`Cancel`/`Shutdown` commands, sequenced lifecycle events,
and one-shot generation-tagged frame leases. A newer admitted generation cancels its predecessor,
and successful publication rechecks the current generation while atomically reserving the
`NavigationCommitted`/`FrameReady` pair and replacing the retained frame. The worker constructs,
uses, shuts down, and explicitly destroys its non-`Send` executor on one thread before publishing
terminal status. It does not yet carry viewport or privacy/session context in each command, define a
serialized process protocol, or provide window, tab, input, history, or browser-UI behavior.

The initial in-process implementation must preserve the eventual process boundary. It may use
direct Rust calls, but all variable-sized values remain bounded and no public contract assumes a
shared address space. Protocol IDs and message kinds are assigned in `docs/wire-registry.toml` only
when the concrete serialized contract is ready.

### Document loading and web policy

Agent 3 owns URL-to-document and Fetch semantics above transport: navigation method selection,
redirect policy, origin/CORS/CSP/referrer decisions, MIME and encoding decisions, response
filtering, parser lifecycle, and document commit. Agent 5 owns bytes on the wire, framing,
timeouts, cancellation polling, and transport limits.

For W2, the only minted transport capability is `wild_buzzard_net::LoopbackTarget`; it accepts a
numeric loopback IP and cleartext HTTP. The document loader must not work around that type with DNS,
`localhost`, a raw socket, or a second HTTP implementation. General network access remains blocked
until resolver, TLS, certificate, proxy, address-policy, and isolation contracts exist.

The current synchronous transport must run away from DOM and UI event loops. Its next adapter must
provide bounded asynchronous chunks, cancellation, backpressure, and a terminal completion/error
state without weakening the existing fail-closed body parser.

### Bytes, character decoding, and HTML

Response bytes and decoded character input have separate limits. Chunk boundaries are not assumed
to be UTF-8 boundaries. Until a standards-compatible incremental encoding detector/decoder is
admitted, the first integration may buffer only the already bounded loopback body and accept an
explicit UTF-8 test response; every unsupported or invalid encoding must fail visibly. It must not
use lossy conversion as an unrecorded compatibility shortcut.

The HTML parser owns one mutable `Document` during loading and produces a document plus parse
diagnostics. Layout and style consume an immutable `DocumentSnapshot` carrying a typed
`DocumentVersion`: document identity paired with its document-local revision. Bare revision values
must not be compared across documents. A product navigation is not committed merely because an
HTTP head arrived or some DOM nodes were parsed; W2-A6 returns a result only after its requested
frame work succeeds.

### Stylo and layout

Agent 3 owns the adapter from Wild Buzzard DOM/style inputs to the admitted Stylo parser, selectors,
cascade, computed values, and invalidation contracts. The existing `InitialStyleResolver` is a
foundation test double and cannot satisfy M1. The live slice must use the imported Stylo algorithms
through a documented Wild Buzzard platform feature, without Gecko bindings or a replacement toy CSS
engine.

Computed style and DOM versions must match the layout request. Layout produces owned immutable box
and fragment data plus explicit limits and warnings. W2-A6 uses the Rust text shaper for layout
metrics and reshapes finalized pending runs after layout. Because the current `TextMeasurer`
contract returns metrics rather than the finalized shaped allocation, this does not prove exact
`Arc<ShapedText>` identity or complete CSS text semantics.

### Graphics and presentation

`wild_buzzard_renderer::SceneCompiler` is the accepted layout-to-graphics boundary. It validates the
typed document version, graph, geometry, resources, and WebRender serialization budget. Text
remains a typed pending resource in the page display list; glyph IDs must never be fabricated.
W2-A6 shapes every pending run with the Rust text system and sends one paintable run through the
real glyph adapter as an independent proof frame.

W2-A4D separately adds a graphics composition path for resolving a complete supplied shaped-text
inventory into the scene's pending text slots and submitting page primitives, positioned glyphs,
font resources, epoch, and frame generation together. Its successful proof has zero pending text,
and a private non-reusing identity rejects resolution prepared for any other compilation before
mutation. This graphics contract is independently accepted, but it is not part of W2-A6 until the
engine retains the exact shaped objects and calls the composed renderer path.

`wild_buzzard_headless::HeadlessRenderer` is the accepted Linux x86_64 device/frame boundary. It
owns an exact RGB8/A8 zero-sample EGL pbuffer, imported WebRender construction and transaction
submission, revision/epoch checks, bounded RGBA8 readback, context restoration, and explicit
teardown. Its deterministic background/border screenshots prove the scene-to-pixel seam only;
W2-A6 now connects it to the loader and Stylo adapter, but pending text is not painted into that
same page frame. `CompositionStatus` distinguishes no-text, whitespace-only, and separate-glyph
proof results so a caller cannot silently treat the latter two as composed output. Agent 1 owns
Linux window/surface and input primitives. M1 still requires the engine to use W2-A4D's checked
one-display-list/one-transaction path for all positioned shaped runs and page primitives before the
same frame contract is presented through Wayland/X11.

## Navigation state and cancellation

The product facade must give each stage the same navigation identity and a descendant of the
navigation cancellation token. W2-A6N establishes that identity at its current executor boundary:
a newer admitted generation cancels the older token, and completion is checked under the
publication lock so stale output can emit only cancellation and cannot replace or remove the newer
frame. Components must stop producing externally visible results after cancellation; a stale
document version or stale frame is rejected rather than presented.

W2-A6 currently accepts a cancellation token and absolute deadline and checks them between bounded
synchronous stages. It has exclusive `&mut` access, so it does not permit concurrent navigations or
out-of-order publication. Its renderer epoch advances only when a render is attempted; failures
before page submission do not consume an epoch. A cancellation or deadline observed after a
successful page submission can still return an error after pixels were internally published, and
the separate glyph proof is a second transaction. W2-A6N serializes these operations on one worker
and adds the monotonic navigation generation and atomic external publication decision;
`DocumentVersion` remains document identity rather than a navigation token. It does not make the
transport or pipeline stages asynchronous and does not turn internal renderer submission into a
window presentation protocol.

The minimum state progression is:

```text
created -> fetching -> parsing -> styling -> laying-out -> building-frame -> committed
   |          |          |          |             |              |
   +----------+----------+----------+-------------+--------------+-> failed/cancelled
```

Failure is structured by stage and preserves the safe component error without exposing credentials,
raw pointers, or unbounded peer input. Terminal network framing errors, parser limit failures,
invalid encodings, stale revisions, and renderer validation errors must remain failures on retry;
retry starts a new navigation identity.

## First acceptance fixture

The first end-to-end fixture is a deterministic in-process numeric-loopback server returning a
small explicit UTF-8 HTML document and CSS supported by the admitted Stylo adapter. W2-A6 now
proves:

- one engine operation reaches only a numeric-loopback capability, without DNS or unsolicited
  external access;
- bounded response bytes parse into a DOM whose exact `DocumentVersion` flows through style,
  layout, scene, page frame, glyph frame, and evidence;
- actual imported Stylo parsing, selector matching, cascade, and computed values supply layout;
- backgrounds and borders produce exact checked pixels in a real headless Linux frame;
- every pending text run is shaped, and one non-whitespace run produces changed pixels through a
  separate real WebRender glyph frame;
- no-text, whitespace-only, 404, invalid UTF-8, pre-cancelled, and expired-deadline behavior is
  explicit, and multiple sequential documents retain distinct identities;
- focused check, strict Clippy, tests, release, and warning-denied rustdoc run from external build
  directories with locked dependencies.

M1 acceptance still requires:

- retain each finalized run's exact `Arc<ShapedText>` rather than reshaping and discarding it;
- project above-baseline as `first_baseline` and below-baseline as
  `height - first_baseline` for the composed-text contract;
- call `render_composed` once for backgrounds, borders, and all positioned shaped runs, then publish
  that zero-pending frame through W2-A6N's generation-tagged lease boundary;
- add the deterministic URL-to-composed-frame fixture plus its Firefox/WPT/reftest mapping and
  broader malformed-input evidence.

JavaScript, normal internet access, browser chrome, and YouTube are later gates. This fixture must
not be described as browser, engine, UI, CSS, or rendering parity.
