# Wild Buzzard network nucleus

`wild_buzzard_net` is the Rust-native transport nucleus for Wild Buzzard's first headless vertical
slice. It sends one HTTP/1.1 request to a numeric Linux loopback address, parses one bounded response,
and exposes its body as a cancellation-aware bounded reader. It is deliberately a small transport,
not a claim of general web access or Firefox networking parity.

The crate has no `unsafe`, native wrapper, telemetry, provider endpoint, runtime dependency on the
`firefox/` reference checkout, DNS lookup, or connection pool. Its only URL parser is the imported
WHATWG `url` crate at `third_party/rust/url`; the dependency is a relative path and is locked. Its
cancellation contract is the shared foundation
`wild_buzzard_runtime::{CancellationSource, CancellationToken}` rather than a network-local type.
The supported product/build target is only `x86_64-unknown-linux-gnu`; no other operating-system or
architecture adapter is implemented or tested here.

## Dependency provenance

- `url` 2.5.7 is the imported rust-url/WHATWG URL component at `third_party/rust/url`, licensed
  `MIT OR Apache-2.0`. This crate uses that exact local source through a path dependency and does not
  modify its normalized vendor manifest. Its locked transitive Rust dependencies come from the
  crate-local Cargo lockfile; a product-wide dependency audit remains an orchestrator gate.
- `wild_buzzard_runtime` 0.1.0 is Wild Buzzard's first-party foundation crate at
  `mozglue/rust/wild_buzzard_runtime`, licensed MPL-2.0. The network crate directly accepts and
  re-exports its cancellation source/token types; there is no duplicate cancellation state machine.
- The network nucleus itself is new MPL-2.0 Rust code. No Firefox implementation source was copied,
  and there is no C, C++, native FFI, or `unsafe` boundary.

## Contract

The transport boundary consists of:

- `LoopbackTarget`, which accepts only credential-free, fragment-free `http` URLs with a numeric
  IPv4 loopback address or `::1`. Names such as `localhost` are refused so this wave cannot invoke
  DNS. `Origin` and the origin-form `RequestTarget` remain explicit.
- Validated `Method`, `HeaderName`, and `HeaderValue` types. The transport owns `Host`, connection,
  framing, upgrade, expectation, and content-coding request fields to prevent injection or ambiguous
  outgoing messages.
- `Request`, which requires an explicit `RedirectPolicy`. `Manual` exposes a redirect response to the
  web-platform caller; `Reject` fails it. This crate never follows a `Location` value.
- `ClientConfig`, which bounds the fully serialized outgoing request head, caller-supplied request
  field count, request body, aggregate response status/header/trailer bytes, response field count,
  decoded body bytes, chunk-line bytes, and informational response count. It also provides connect,
  read-inactivity, and write-inactivity timeouts. Request-head sizing uses checked arithmetic over the
  request line, Host and connection lines, every caller field and CRLF, the generated Content-Length,
  and the final empty line before reserving memory or opening a socket.
- `CancellationToken` and an optional absolute deadline spanning connect, response head, and body
  delivery. Blocking I/O polls cancellation at a bounded interval.
- `ResponseHead` and `Body`. The body supports exact Content-Length, strict chunked decoding with
  validated trailers, no-body semantics, and EOF-delimited connection-close responses. The client
  always sends `Connection: close` and never marks the socket reusable. A protocol, resource-limit,
  premature-EOF, or non-timeout I/O failure permanently poisons the body: later reads reproduce the
  same structured error without consuming or exposing later peer bytes, and partial trailers are
  discarded. Cancellation and timeout remain control-flow failures rather than parser poison: an
  inactivity timeout can be retried, while cancellation continues to follow its one-way token.
- Structured `Error` values for policy, timeout, cancellation, I/O, malformed syntax, premature EOF,
  framing conflicts, unsupported coding, outgoing request-head/count limits, and response limit
  failures. Peer input is not indexed or converted with an unchecked panic path.

Strict response parsing requires CRLF, an HTTP/1.0 or HTTP/1.1 status line, token field names, and
control-free values. It rejects obsolete line folding, Transfer-Encoding plus Content-Length,
conflicting Content-Length fields, invalid decimal lengths, transfer codings other than exactly one
`chunked`, non-identity Content-Encoding, malformed chunk extensions, forbidden framing trailers,
oversized input, and incomplete fixed or chunked bodies. Duplicate identical Content-Length values
are accepted, including equivalent comma-list members.

## Agent 3 adoption boundary

The DOM/Fetch owner can construct a validated request and run the synchronous client away from its
main event loop:

```rust
use wild_buzzard_net::{
    CancellationSource, HttpClient, LoopbackTarget, RedirectPolicy, Request,
};

let source = CancellationSource::new();
let target = LoopbackTarget::parse("http://127.0.0.1:8000/index.html")?;
let request = Request::get(target, RedirectPolicy::Manual)
    .with_cancellation(source.token());
let response = HttpClient::default().execute(&request)?;
let bytes = response.read_body_to_end()?;
# Ok::<(), wild_buzzard_net::Error>(())
```

This is intentionally a policy split:

- DOM/Fetch owns URL-to-request decisions, Fetch modes, CORS, CSP, referrer policy, redirect method
  rewriting, redirect-loop limits, origin checks, response filtering, and exposure to script.
- Networking owns validated wire serialization, connection policy, transport timeouts, cancellation,
  byte streaming, message framing, and peer-input limits.
- The loopback proof is part of the connectable target type, not a hostname string checked after a
  resolver runs. A later general-network policy layer must produce a separate approved target only
  after DNS/address policy, permission, private-network, proxy, and security checks.

The current API is synchronous. Agent 3 should not block a DOM or UI event loop; the future IPC/async
adapter should preserve `Request`, response metadata, structured errors, cancellation, total bounds,
and backpressure rather than bypassing them.

## Explicit gaps and future boundary

This wave does **not** implement TLS or certificate verification, DNS, proxies, cookies, cache,
authentication, downloads, compression decoding, CORS, CSP, referrer policy, HTTP/2, HTTP/3, QUIC,
socket pooling, production internet access, or persistent storage. It has no external-network test
and makes no claim about those behaviors.

Neqo remains the intended reusable QUIC/HTTP/3 core after its canonical editable workspace is
established. Neqo, an HTTP/2 implementation, and a future Rust-facing TLS/certificate service should
sit behind a transport interface that preserves the request/response streaming and cancellation
contract. Certificate verification, trust policy, resolver output validation, proxy policy, and
address permission checks belong to an explicit security/policy boundary before a non-loopback
connect capability is minted. They must not be hidden inside a URL parser or weakened to expand this
prototype.

## Firefox ESR153 reference evidence

Reference checkout: detached ESR153 commit `c19b7e89270787889495688244ec6ee8e79288a1`.
The checkout is research material only and is not a source or build input.

Implementation paths inspected:

- `netwerk/protocol/http/nsHttpRequestHead.cpp` and `.h` for request-head representation.
- `netwerk/protocol/http/nsHttpResponseHead.cpp` and `.h` for status, header, and Content-Length
  processing.
- `netwerk/protocol/http/nsHttpTransaction.cpp` for response-head acquisition, framing selection,
  partial-transfer detection, cancellation, and connection semantics.
- `netwerk/protocol/http/nsHttpChunkedDecoder.cpp` and `.h` for chunk parsing and trailers.
- `netwerk/base/nsNetUtil.cpp` and `netwerk/base/nsINetUtil.idl` were consulted only to understand the
  broader validation boundary; no XPCOM or C++ adapter was adopted.

Test paths inspected and mapped into `tests/loopback_http.rs`:

- `netwerk/test/gtest/TestHttpResponseHead.cpp`
- `netwerk/test/gtest/TestParseHeaders.cpp`
- `netwerk/test/unit/test_duplicate_headers.js`
- `netwerk/test/unit/test_content_length_underrun.js`
- `netwerk/test/unit/test_chunked_responses.js`
- `netwerk/test/unit/test_obs-fold.js`
- `netwerk/test/httpserver/test/test_headers.js`
- `netwerk/test/httpserver/test/test_request_line_split_in_two_packets.js`

Full history was inspected with `git log --follow`, `git log -S`, and targeted `git show`. Particularly
relevant changes include `fbc7db2fe9a8` (bug 655389, CRLF injection and header parsing),
`896e58f7b4b7` (bug 237623, detecting broken HTTP/1.1 transfers), `0c8aa78f3e5b` (soft framing checks),
and `6fd47aac93e5` (bug 1589609, obs-fold coverage). The ESR obs-fold test records compatibility
folding; this nucleus deliberately rejects obs-fold as the safer behavior and records that difference
rather than claiming exact parity.

## Local gates

All generated files must stay in the task-owned external target directory:

```sh
CARGO_TARGET_DIR=../../../../wildbuzzardbuilds/agent-5-network-wave2 \
  cargo fmt --package wild_buzzard_net -- --check
CARGO_TARGET_DIR=../../../../wildbuzzardbuilds/agent-5-network-wave2 \
  cargo check --all-targets --locked
CARGO_TARGET_DIR=../../../../wildbuzzardbuilds/agent-5-network-wave2 \
  cargo clippy --all-targets --all-features --locked -- -D warnings
CARGO_TARGET_DIR=../../../../wildbuzzardbuilds/agent-5-network-wave2 \
  cargo test --locked
CARGO_TARGET_DIR=../../../../wildbuzzardbuilds/agent-5-network-wave2 \
  cargo build --release --locked
RUSTDOCFLAGS="-D warnings" \
  CARGO_TARGET_DIR=../../../../wildbuzzardbuilds/agent-5-network-wave2 \
  cargo doc --no-deps --locked
```

The integration suite uses only ephemeral numeric loopback listeners and deterministic in-process
peers. It never contacts the external network.
