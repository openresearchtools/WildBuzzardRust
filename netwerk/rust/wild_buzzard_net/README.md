# Wild Buzzard network nucleus

`wild_buzzard_net` provides two separate Rust-native HTTP/1.1 capabilities. `HttpClient` preserves
the original numeric-loopback-only transport. `GeneralWebClient` adds bounded Linux system DNS,
IPv4/IPv6 address attempts, cleartext HTTP when explicitly requested, and authenticated HTTPS using
TLS 1.2 or TLS 1.3. Both parse one bounded response and expose a cancellation-aware bounded body.
This remains a narrow transport slice, not a claim of Firefox networking parity.

The crate has no first-party `unsafe`, native wrapper, telemetry, provider endpoint, runtime
dependency on the `firefox/` reference checkout, or connection pool. Its URL parser is the imported
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
- `hickory-resolver` 0.26.1 uses Linux system resolver configuration through a private bounded Tokio
  1.53.1 current-thread runtime. DNS futures, cache entries, active requests, candidates, attempts,
  deadlines, and total lookup time are bounded.
- The direct `mio` 1.2.2 declaration disables defaults and requests only `net` and `os-poll`, so
  each sequential address attempt retains one nonblocking socket while polling readiness,
  cancellation, its absolute deadline, and its aggregate connect timeout. Cargo's resolved feature
  union also contains `os-ext`, selected transitively by Tokio; this crate does not call that API.
  A readiness event is accepted only after both `take_error` and the exact peer address succeed;
  the connected descriptor is then returned to blocking mode for bounded HTTP/TLS I/O.
- `rustls` 0.23.43 uses its explicit `ring` provider with TLS 1.2 and TLS 1.3 only. The client offers
  only `http/1.1` through ALPN, enables hostname verification and SNI for DNS names, disables early
  data, and has no public custom-verifier or invalid-certificate escape hatch. `webpki-roots` 1.0.9
  supplies the pinned bundled roots; valid DER roots may only be added, not substituted for an
  unauthenticated verifier. `rcgen` 0.14.8 is test-only.
- The network code is new MPL-2.0 Rust. No Firefox implementation source was copied and no
  first-party C, C++, native FFI, or `unsafe` boundary was added. The selected third-party `ring`
  backend does contain audited upstream native/assembly and unsafe code, which remains a dependency
  provenance and AppImage-closure concern.

## Contract

The transport boundary consists of:

- `LoopbackTarget`, which accepts only credential-free, fragment-free `http` URLs with a numeric
  IPv4 loopback address or `::1`. Names such as `localhost` are refused so this wave cannot invoke
  DNS. `Origin` and the origin-form `RequestTarget` remain explicit.
- `GeneralWebTarget`, `GeneralWebRequest`, and `GeneralWebClient`, which form a distinct capability.
  They accept bounded WHATWG `http` and `https` URLs, reject credentials and fragments, serialize
  only origin-form targets, resolve normalized DNS names through the private resolver, deduplicate
  and cap A/AAAA results, and cap sequential connection attempts. Each attempt polls one retained
  nonblocking socket instead of restarting TCP handshakes at the cancellation interval. Numeric
  targets bypass DNS.
- `TrustStore` always begins with the bundled Web PKI roots. Its only extension operation parses and
  adds a DER trust anchor. The TLS configuration has no certificate-verification bypass, insecure
  fallback, TLS downgrade below 1.2, client certificate, early data, or non-HTTP/1.1 ALPN offer.
- `GeneralWebResponse` records whether the caller explicitly selected cleartext HTTP or an
  authenticated TLS connection, plus the negotiated TLS version and HTTP/1.1 ALPN outcome. A server
  that selects an unoffered ALPN fails closed; absence of ALPN permits HTTP/1.1 as required for
  compatible TLS origins.
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
  delivery. General-web requests extend that same deadline across DNS and TLS. Blocking I/O polls
  cancellation at a bounded interval; DNS and TLS also have total time and byte/candidate limits.
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

This remains intentionally split by capability and policy:

- DOM/Fetch owns URL-to-request decisions, Fetch modes, CORS, CSP, referrer policy, redirect method
  rewriting, redirect-loop limits, origin checks, response filtering, and exposure to script.
- Networking owns validated wire serialization, connection policy, transport timeouts, cancellation,
  byte streaming, message framing, and peer-input limits.
- The loopback proof remains part of `LoopbackTarget`; `HttpClient` cannot consume a
  `GeneralWebTarget`. The new general-web capability deliberately does not make Fetch, CORS, CSP,
  permission, proxy, or local-network-access decisions. Those higher layers must decide when they
  are allowed to construct and execute a `GeneralWebRequest`.

The current API is synchronous. Agent 3 should not block a DOM or UI event loop; the future IPC/async
adapter should preserve `Request`, response metadata, structured errors, cancellation, total bounds,
and backpressure rather than bypassing them.

## Explicit gaps and future boundary

This wave does **not** implement Happy Eyeballs racing, DNS-over-HTTPS, DNSSEC validation, Firefox's
NSS trust service, OS/enterprise roots and constraints, revocation fetching, proxies, cookies,
cache, HTTP authentication, downloads, compression decoding, CORS, CSP, referrer policy, HSTS or
HTTPS upgrades, HTTP/2, HTTP/3, QUIC, socket pooling, persistent storage, process isolation, or
browser-facing permission/private-network policy. Bundled-root refresh and release dependency
closure remain explicit maintenance gates. The only public-network test is ignored and additionally
requires `WILD_BUZZARD_PUBLIC_NETWORK=1`; normal test runs are entirely local.

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
CARGO_TARGET_DIR=../../../../wildbuzzardbuilds/w8-a5-general-web \
  cargo fmt --package wild_buzzard_net -- --check
CARGO_TARGET_DIR=../../../../wildbuzzardbuilds/w8-a5-general-web \
  cargo check --all-targets --locked
CARGO_TARGET_DIR=../../../../wildbuzzardbuilds/w8-a5-general-web \
  cargo clippy --all-targets --all-features --locked -- -D warnings
CARGO_TARGET_DIR=../../../../wildbuzzardbuilds/w8-a5-general-web \
  cargo test --locked
CARGO_TARGET_DIR=../../../../wildbuzzardbuilds/w8-a5-general-web \
  cargo build --release --locked
RUSTDOCFLAGS="-D warnings" \
  CARGO_TARGET_DIR=../../../../wildbuzzardbuilds/w8-a5-general-web \
  cargo doc --no-deps --locked
```

The default suite uses only ephemeral numeric loopback listeners, a `localhost` system-resolver
check, and deterministic in-process peers. It never contacts the external network.
