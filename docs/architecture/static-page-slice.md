# Static-page vertical-slice contract

This document fixes the ownership and data flow for milestone M1. It is an integration contract,
not evidence that the end-to-end slice already runs. A separately tested graphics boundary now
renders real pixels, but URL loading, Stylo, layout, shaping, and that frame owner are not yet one
pipeline.

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

## Owner contracts

### Browser product and engine facade

Agent 6 owns a typed navigation command containing a navigation identity, URL text, viewport,
top-level browsing-context identity, cancellation handle, and explicit privacy/session context.
The facade publishes typed provisional-start, response, committed-document, frame-ready, failure,
and cancelled events. Events carry stable identities and owned summaries; they do not carry
process-local pointers or component-private objects.

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

The HTML parser owns one mutable `Document` during loading and produces a committed document plus
parse diagnostics. Layout and style consume an immutable `DocumentSnapshot` carrying the exact
document revision. A navigation is not committed merely because an HTTP head arrived or some DOM
nodes were parsed.

### Stylo and layout

Agent 3 owns the adapter from Wild Buzzard DOM/style inputs to the admitted Stylo parser, selectors,
cascade, computed values, and invalidation contracts. The existing `InitialStyleResolver` is a
foundation test double and cannot satisfy M1. The live slice must use the imported Stylo algorithms
through a documented Wild Buzzard platform feature, without Gecko bindings or a replacement toy CSS
engine.

Computed style and DOM revisions must match the layout request. Layout produces owned immutable box
and fragment data plus explicit limits and warnings. Font metrics used before real shaping must be
marked provisional; they cannot become pixel-parity evidence.

### Graphics and presentation

`wild_buzzard_renderer::SceneCompiler` is the accepted layout-to-graphics boundary. It validates the
document revision, graph, geometry, resources, and WebRender serialization budget. Text remains a
typed pending resource until font discovery, fallback, bidi, shaping, glyph registration, and
rasterization are implemented; glyph IDs must never be fabricated.

`wild_buzzard_headless::HeadlessRenderer` is the accepted Linux x86_64 device/frame boundary. It
owns an exact RGB8/A8 zero-sample EGL pbuffer, imported WebRender construction and transaction
submission, revision/epoch checks, bounded RGBA8 readback, context restoration, and explicit
teardown. Its deterministic background/border screenshots prove the scene-to-pixel seam only;
pending text is not painted and it is not connected to the loader or Stylo adapter. Agent 1 owns
Linux window/surface and input primitives. M1 still requires the full fixture, including shaped
text, before the same frame contract is presented through Wayland/X11.

## Navigation state and cancellation

Each stage receives the same navigation identity and a descendant of the navigation cancellation
token. A newer navigation cancels the older pipeline. Components must stop producing externally
visible results after cancellation; a stale document revision or stale frame is rejected rather
than presented.

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
small explicit UTF-8 HTML document and CSS supported by the admitted Stylo adapter. Acceptance
requires all of the following:

- one browser/engine navigation command reaches the loopback transport without DNS or unsolicited
  external access;
- response bytes parse into a DOM whose committed revision is carried through style, layout, scene,
  and frame metadata;
- actual Stylo parsing/cascade supplies computed styles;
- backgrounds, borders, and shaped text produce a deterministic headless Linux screenshot;
- cancellation and one malformed response fail without committing or presenting a stale frame;
- the test runs from a clean external build directory with `firefox/` absent as a build input;
- the handoff records exact Firefox/WPT/reftest references and every unsupported behavior.

JavaScript, normal internet access, browser chrome, and YouTube are later gates. This fixture must
not be described as browser, engine, UI, CSS, or rendering parity.
