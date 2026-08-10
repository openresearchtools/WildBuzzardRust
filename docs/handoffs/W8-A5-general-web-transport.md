# W8-A5 general-web transport handoff

## Decision and boundary

W8-A5 admits one bounded, synchronous Rust transport for ordinary `http` and authenticated
`https` origins on Linux x86-64. It is a transport slice, not a Fetch implementation and not a
claim of page loading, HTML/CSS/JavaScript execution, rendering, normal-site compatibility, or
Firefox networking parity.

The existing `HttpClient`/`LoopbackTarget` capability remains numeric-loopback-only. The new
`GeneralWebClient` cannot be reached by weakening or converting that type: callers must explicitly
construct a `GeneralWebTarget` and `GeneralWebRequest`. Firefox remains read-only reference material
at detached ESR153 commit `c19b7e89270787889495688244ec6ee8e79288a1`; it is not a build, test, or
runtime input.

All new first-party implementation is safe MPL-2.0 Rust. The crate continues to deny first-party
`unsafe`. It adds no telemetry, provider endpoint, certificate-verification bypass, insecure TLS
fallback, client credential, first-party native code, or dependency on `firefox/`.

## Public API and capability split

The public general-web surface consists of:

- `GeneralWebTarget`, `WebOrigin`, `WebHost`, and `WebScheme` for an exact normalized origin and
  origin-form path/query;
- `GeneralWebRequest`, which owns an explicit `RedirectPolicy`, method, validated fields, bounded
  body, shared cancellation token, and optional absolute deadline;
- `GeneralWebConfig`, which combines the existing strict HTTP limits with DNS, candidate, attempt,
  TLS-time, and TLS-byte limits;
- `TrustStore`, whose only public starting point is the bundled Web PKI root set and whose only
  mutation is additive parsing of a DER trust anchor;
- `GeneralWebClient`, which performs validation, DNS when needed, bounded sequential connection
  attempts, TLS authentication for `https`, one HTTP/1.1 exchange, and strict streaming response
  parsing; and
- `GeneralWebResponse` plus `ConnectionSecurity`, `TlsVersion`, and `AlpnOutcome`, which distinguish
  explicitly selected cleartext from authenticated TLS and report only TLS 1.2/1.3 and HTTP/1.1 or
  absent ALPN.

`GeneralWebResponse` is distinct from the loopback response entry point. There is no raw resolver,
socket, TLS configuration, certificate verifier, or peer-selected authority in the public API.

## URL, DNS, and address policy

`GeneralWebTarget` uses the canonical local WHATWG `url` 2.5.7 source. It:

- accepts only `http` and `https`;
- bounds both input and normalized serialization to 2 MiB;
- rejects URL usernames, passwords, and fragments;
- requires a host and a nonzero effective port;
- retains WHATWG IDNA, percent-encoding, path, query, IPv4, and IPv6 normalization;
- serializes only an origin-form request target and a correctly bracketed/default-port-elided Host
  authority; and
- never treats a `Location` response field as a new transport request.

Numeric IPv4/IPv6 targets bypass DNS and are not governed by the DNS-candidate limit. Domain names
are parsed as Hickory `Name` values and resolved from Linux system resolver configuration by a
dedicated named owner thread. That thread owns its Tokio current-thread runtime and Hickory resolver
for their complete lifetimes, so construction, `block_on`, and runtime drop never occur inside an
arbitrary caller's Tokio context.

Default DNS policy is a 5-second aggregate caller-visible timeout, a 32-command bounded queue,
256-entry resolver cache, 32 active requests, two concurrent upstream queries, two attempts,
`Ipv6AndIpv4`, TCP fallback on resolver error, no preserved CNAME intermediates, and at most 32
unique A/AAAA candidates. Admission and response waiting poll cancellation and the request deadline.
Startup failure, worker panic, channel loss, invalid name, no records, and lookup failure have typed
fail-closed classifications. Final client drop disconnects and joins the resolver owner.

## TCP connection contract and the public-smoke defect

Address candidates are tried sequentially, never raced. The default cap is 16 attempts and the
default aggregate timeout is two seconds per candidate, further shortened by the absolute request
deadline. Every attempt creates one nonblocking Mio socket, registers writable interest, and polls
that same socket at the cancellation interval. Readiness is not success until `take_error()` returns
none and `peer_addr()` equals the exact requested `SocketAddr`. The descriptor is deregistered and
returned to blocking mode only after those checks.

The first opt-in `example.com` run exposed a real bug in the inherited loopback connector. It called
`TcpStream::connect_timeout` with a 10 ms polling slice, dropped that socket on timeout, and started
a new TCP handshake. At 2026-08-10T04:37:55+01:00 the smoke failed after four candidates with
`ConnectAttemptsExhausted { attempted: 4, last_kind: Some(TimedOut) }`.

Read-only diagnosis was limited to the authorized target:

- no `HTTPS_PROXY`, `https_proxy`, `ALL_PROXY`, or `all_proxy` variable was set;
- `curl --http1.1` reached `172.66.147.243` and returned status 200;
- system resolution exposed two IPv6 and two IPv4 candidates; and
- `strace` proved immediate `ENETUNREACH` for IPv6 followed by hundreds of new IPv4 `connect(2)`
  calls returning `EINPROGRESS` at roughly 10 ms intervals.

The Mio connector fixes that failure without relaxing a timeout, deadline, cancellation, address,
or TLS check. A deterministic scripted regression proves one factory invocation survives three poll
iterations, then separately proves aggregate connect timeout and post-poll cancellation. Existing
local IPv6-failure-to-IPv4 fallback coverage exercises candidate retry. This remains sequential
fallback, not Firefox-like Happy Eyeballs.

## TLS authentication and protocol policy

The TLS client uses rustls 0.23.43 with an explicit `ring` provider and only TLS 1.3 and TLS 1.2.
It always starts from the pinned `webpki-roots` 1.0.9 set. An administrator may add a valid DER root,
but cannot replace verification, install a custom verifier, accept an invalid certificate, or create
an empty unauthenticated trust policy through the public API.

DNS names are verified as `ServerName` values and send SNI. Numeric targets are verified against an
IP subject alternative name and send no DNS SNI. The client offers only `http/1.1`, asks rustls to
reject an unoffered selected ALPN, accepts absent ALPN as HTTP/1.1 compatibility, disables early
data, sends no client certificate, limits rustls buffering to 64 KiB, and verifies the final protocol
version and ALPN again before exposing a response.

Each candidate has a 10-second aggregate TLS handshake timeout, bounded by the request deadline,
and a 1 MiB aggregate handshake-wire-byte cap across reads and writes. Socket operations poll at
10 ms. TLS, certificate, ALPN, protocol-version, cancellation, timeout, byte-limit, and I/O failures
remain typed. There is no HTTPS-to-HTTP fallback. HTTP request bytes are not written until candidate
selection and TLS authentication succeed, so TCP/TLS retries cannot replay the body.

Deterministic TLS evidence covers:

- trusted DNS-name certificate, SNI, and HTTP/1.1 ALPN;
- numeric IP SAN verification with no SNI;
- TLS 1.2 and TLS 1.3 as the only admitted versions;
- absent ALPN compatibility and fatal rejection of an HTTP/2-only server;
- wrong name, untrusted issuer, and expired certificate classification;
- a real root-to-intermediate-to-leaf chain, plus exact `UnknownIssuer` failure when the server
  omits the required intermediate;
- additive roots and malformed-DER rejection;
- forced handshake cancellation, aggregate timeout, and handshake-byte limit; and
- preservation of the strict HTTP body limit over authenticated TLS.

## HTTP/1.1 and redirect behavior

W8-A5 refactors the existing transport into private `WireRequest`/`WireStream`, request preparation,
and execution seams. Both capabilities therefore use the same bounded serializer and strict parser;
the 37 original loopback regressions remain green.

The transport owns Host, connection, proxy authorization, framing, upgrade, expectation, and
content-coding request fields. `Authorization` remains caller data for a future higher-layer policy,
but URL credentials are forbidden and `Proxy-Authorization` cannot be sent to an origin. Serialized
request-head bytes and field counts are checked before opening a socket. The client always sends
`Connection: close` and implements no pool or automatic replay.

Default HTTP bounds are 64 KiB aggregate response metadata, 256 response fields, 8 MiB decoded body,
64 KiB fully serialized request head, 128 caller request fields, 1 MiB request body, 8 KiB chunk
line, and eight informational responses. Connect/read/write inactivity defaults are 2/5/5 seconds.
The parser keeps its strict CRLF, status, header, body-framing, chunk, trailer, EOF, poison, and
unsupported-coding behavior.

`RedirectPolicy::Manual` returns the exact 3xx response. `RedirectPolicy::Reject` returns a typed
error. Neither mode parses or follows `Location`, so credentials and bodies cannot leak through a
transport-level redirect.

## Typed failures and cancellation

The public error vocabulary now includes stable DNS, certificate, TLS, trust-store, URL-size,
DNS-candidate, connection-attempt, and TLS-handshake-byte classifications. Fatal peer alerts for
`NoApplicationProtocol` and `ProtocolVersion` map to their specific TLS failures rather than a
generic protocol bucket.

One shared foundation `CancellationToken` and one optional absolute deadline span DNS admission and
lookup, every connection attempt, TLS authentication, request write, response head, and body reads.
DNS, connection, TLS, and blocking HTTP I/O poll at bounded intervals; aggregate phase timeouts are
not reset by an individual poll.

## Dependency and lock evidence

Direct dependency selection is exact:

| Package | Features/source | License | crates.io SHA-256 |
| --- | --- | --- | --- |
| `hickory-resolver` 0.26.1 | defaults off; `system-config,tokio` | MIT OR Apache-2.0 | `f0d58d28879ceecde6607729660c2667a081ccdc082e082675042793960f178c` |
| `mio` 1.2.2 | direct: defaults off, `net,os-poll`; resolved union also has Tokio-selected `os-ext` | MIT | `30d65c71f1ce40ab09135ce117d742b9f8a19ff91a41a8b57ed50bc2de59c427` |
| `rustls` 0.23.43 | defaults off; `std,tls12,ring` | Apache-2.0 OR ISC OR MIT | `0283386ce02abc0151e1761d08802dfe86c173b0b494af5cbc086574e453da06` |
| `tokio` 1.53.1 | defaults off; `rt,net,time` | MIT | `202caea871b69668250d242070849eb495be178ed697a3e98aebce5bc81a0bed` |
| `webpki-roots` 1.0.9 | compiled root set | CDLA-Permissive-2.0 | `7dcd9d09a39985f5344844e66b0c530a33843579125f23e21e9f0f220850f22a` |
| `rcgen` 0.14.8 | test-only; defaults off; `crypto,ring` | MIT OR Apache-2.0 | `57f6d249aad744e274e682777a50283a225a32705394ee6d5fcc01efa25e4055` |

The existing local path dependencies remain WHATWG `url` 2.5.7 (`MIT OR Apache-2.0`) and
MPL-2.0 `wild_buzzard_runtime` 0.1.0. The selected transitive `ring` 0.17.14 archive checksum is
`a4689e6c2294d81e88dc6261c768b63bc4fcdb852be6d1352498b114f61383b7`
and its license is Apache-2.0 AND ISC. `ring` includes upstream native/assembly and unsafe code;
Mio and its OS backend also contain audited upstream unsafe/syscall boundaries. W8-A5 adds no
first-party unsafe or FFI.

For `x86_64-unknown-linux-gnu`, `cargo tree` reports 98 unique packages for normal edges and 109 for
normal/build/dev edges using the recorded sort-and-deduplicate command. The dependency set is not an
AppImage-closure or product-wide license acceptance claim.

Mio 1.2.2 was already present and checksummed in all shared locks. The orchestrator explicitly
authorized adding it to the `wild_buzzard_net` dependency arrays in root, engine, UI, and shell
locks. No package stanza changed. `cargo metadata --locked --no-deps --format-version 1` succeeds in
all four workspaces.

Official API references consulted were the versioned rustls, webpki-roots, Hickory Resolver, Tokio,
Mio, and rcgen rustdoc pages. In particular, Mio's connection documentation requires writable
registration, `take_error`, and `peer_addr` checks; rustls's normal client builder retains Web PKI
verification and permits explicit safe protocol versions.

## Firefox ESR153 reference evidence

Focused read-only implementation inspection included:

- `netwerk/protocol/http/TlsHandshaker.cpp` for Firefox's HTTP/1.1-first ALPN offer;
- `netwerk/protocol/http/DnsAndConnectSocket.cpp`, `HappyEyeballsConnectionAttempt.*`,
  `HappyEyeballsTransaction.*`, and `happy_eyeballs_glue/HappyEyeballs.*` for address preference,
  racing, cancellation, and the 250 ms backup path;
- `netwerk/dns/nsIDNSService.idl` for asynchronous cancellation and blocking synchronous lookup;
  and
- `security/manager/ssl/SSLServerCertVerification.cpp` for hostname and background verification
  boundaries.

Focused tests included `netwerk/test/unit/test_dns_cancel.js`,
`test_cert_verification_failure.js`, the `test_happy_eyeballs_*` family,
`test_https_rr_sorted_alpn.js`, and security-manager tests for self-signed certificates, trust,
session resumption, expiry, and name constraints.

History inspection included `0288bf04fe9c` (introducing `HappyEyeballsConnectionAttempt`),
`9456bdc8e2d5` (reworking `HappyEyeballsTransaction`), and `ad0ee4135867` (pausing racers for a client
certificate). W8-A5 deliberately does not claim those racing or client-certificate behaviors. No
Firefox source was copied.

## Hostile review and defect response

An independent ultra-effort security review found and verified fixes for:

1. caller-context Tokio runtime construction/drop panic risk, fixed by the dedicated resolver owner;
2. fatal ALPN/version alert misclassification;
3. insufficient certificate-chain evidence, fixed with a real constrained intermediate chain;
4. numeric IPs being incorrectly gated by the DNS-candidate limit; and
5. `Proxy-Authorization` not being transport-reserved.

After those fixes the reviewer reported no unresolved high-, medium-, or low-severity issue and
confirmed the capability split, authenticated-before-write rule, redirect non-following, bounded
resources, and all loopback regressions.

The later Mio same-socket connector delta received a separate hostile review with GO and no high-
or medium-severity finding. The review verified that each candidate keeps one persistent socket;
cancellation and aggregate deadlines surround every bounded poll; success requires both a clear
`SO_ERROR` and the exact peer address; descriptor ownership transfers once and otherwise closes by
RAII; and IPv6 failure can fall back to IPv4 without weakening TLS. Its sole low-severity finding
was documentation that described only the directly requested Mio features while Cargo's resolved
feature union also contains Tokio-selected `os-ext`. The dependency table and crate README now
distinguish those facts; Wild Buzzard does not call Mio's `os-ext` API.

## Verification evidence

Toolchain: Cargo 1.96.0 and rustc 1.96.0, target `x86_64-unknown-linux-gnu`. Recorded final artifacts
are under `/home/user/Documents/wildbuzzardbuilds/w8-a5-general-web/`.

Final gates:

```sh
cargo fmt --package wild_buzzard_net -- --check

CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w8-a5-general-web/fix-check \
  cargo check -p wild_buzzard_net --all-targets --locked \
  --target x86_64-unknown-linux-gnu

CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w8-a5-general-web/final-clippy \
  cargo clippy -p wild_buzzard_net --all-targets --locked \
  --target x86_64-unknown-linux-gnu -- -D warnings

CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w8-a5-general-web/final-test \
  cargo test -p wild_buzzard_net --locked --target x86_64-unknown-linux-gnu

CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w8-a5-general-web/final-doc \
  RUSTDOCFLAGS=-Dwarnings cargo doc -p wild_buzzard_net --no-deps --locked \
  --target x86_64-unknown-linux-gnu

CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w8-a5-general-web/final-release \
  cargo build -p wild_buzzard_net --release --locked \
  --target x86_64-unknown-linux-gnu

git diff --check
```

All pass. The default deterministic matrix reports 24 general-web tests passed, 37 loopback tests
passed, zero failures, and one explicitly ignored public-network test: 61 passed total. Rustdoc has
zero doctests. A source audit finds no `unsafe`, custom-verifier, invalid-certificate, or telemetry
escape in first-party source.

One early pre-fix diagnostic `cargo check` was inadvertently invoked without an external
`CARGO_TARGET_DIR`. It was not used as acceptance evidence; no cleanup was attempted because the
ignored workspace target may contain other agents' artifacts. Every frozen acceptance build above
uses the task-owned external tree.

## Explicit public-network smoke

The final opt-in smoke was run only after deterministic gates and the connector fix:

```sh
CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w8-a5-general-web/public-smoke-fixed \
  WILD_BUZZARD_PUBLIC_NETWORK=1 cargo test -p wild_buzzard_net --locked \
  --target x86_64-unknown-linux-gnu \
  --lib general::tests::public_example_dot_com_is_explicitly_opt_in \
  -- --ignored --exact --nocapture
```

At `2026-08-10T04:47:20+01:00`, `https://example.com/` returned status 200 over authenticated TLS
1.3 with HTTP/1.1 ALPN. The URL contained no credentials. The response body was 559 bytes under the
8,388,608-byte cap. One test passed in 0.23 seconds. This proves only URL-to-bounded-HTTP transport;
it proves no redirect following, cookie behavior, content interpretation, page load, script, layout,
paint, visible presentation, or site compatibility.

The optional DuckDuckGo, Google, and YouTube transport probes were not run before this handoff so
the connector security-review slot could be freed promptly. No result for those targets is implied.

## Frozen file hashes

SHA-256 at the handoff freeze:

| File | SHA-256 |
| --- | --- |
| `netwerk/rust/wild_buzzard_net/Cargo.toml` | `4c9071054406f6ef7e356355b1e7219269331672d699a552ea21f0119f60db55` |
| `netwerk/rust/wild_buzzard_net/README.md` | `92074590068b6e09881cc41508be8f5e5f637b4e2f05ea437ebafaf2e5d67eb2` |
| `netwerk/rust/wild_buzzard_net/src/client.rs` | `ae8a79b067d2bb8776b1aebaa4abcbc3b6e28bf03e52148b9d7866f6cdfcafe8` |
| `netwerk/rust/wild_buzzard_net/src/error.rs` | `ec14f47231ee715960ff5887545fcd647061fbd61ad1624f28dd742a3d668bf0` |
| `netwerk/rust/wild_buzzard_net/src/general.rs` | `ecb4a33ff5ffd9fda6531444b4232c7a9d291bf5ae5d87b41061030f1eb48c7d` |
| `netwerk/rust/wild_buzzard_net/src/general/tests.rs` | `1e80badab2070fb616bf84dc9d1108ae3f14140b44fed5d6c51d007b3769428d` |
| `netwerk/rust/wild_buzzard_net/src/lib.rs` | `50de503cf2fd2ac58e10b68abd7eb8c8cb736137bdd1055962d14fe3b5eacd9b` |
| `netwerk/rust/wild_buzzard_net/src/message.rs` | `515515b6d3c566b635b7c146a0aa14ee7c4b0a9b3c7b912ca4556b5a8c716ec8` |
| `netwerk/rust/wild_buzzard_net/src/target.rs` | `64bcdbcf028668f835ee77f4b64f4d9582a119b24475ad06694e2f266b07f5fc` |
| root `Cargo.lock` | `49e8bf1e664002983f3387776a05916318f37487fd5d015798d9fd1f2218a9f6` |
| engine `Cargo.lock` | `d69eb94e9034b6af877b2adb653eba7d4d9d74d3355fd16546c280825ddacd8c` |
| UI `Cargo.lock` | `e027e79848f9e16de870f4697904b806f25adcf0b912353d8e50927e255bb997` |
| shell `Cargo.lock` | `4b2761e2fed232418aaba52d2c57fbf748dccab539a25bf21ea9071df10be3e1` |

Shared-lock hashes can legitimately move when another authorized lane changes a different package;
the W8-A5 invariant is the exact `mio` package/checksum and `wild_buzzard_net` dependency entry.

## Explicit residual gaps

- Address attempts are sequential. There is no RFC 8305/Firefox Happy Eyeballs racing, connection
  coalescing, family delay, network-change recovery, or speculative connection manager.
- DNS uses the Linux system resolver configuration but implements no Firefox TRR/DoH policy,
  DNSSEC validation, HTTPS/SVCB routing, proxy DNS, local-network-access permission, partitioning,
  or browser process isolation.
- Trust is a pinned compiled Web PKI set plus additive DER roots. There is no NSS trust database,
  OS/enterprise root policy, user distrust, CT, OCSP/CRLite/revocation integration, client
  certificate selection, root-update service, or certificate UI.
- There is no proxy/PAC/WPAD/SOCKS/CONNECT support. Proxy environment variables are deliberately
  not consumed implicitly.
- There is no HSTS/HTTPS-Only upgrade, mixed-content policy, CORS, CSP, referrer policy, Fetch
  metadata, service worker, cookie/authentication jar, cache, compression decoder, download policy,
  content sniffing, MIME policy, or redirect rewriting/loop handling.
- There is no HTTP/2, HTTP/3, QUIC, WebSocket, WebTransport, connection pool, reuse, prioritization,
  backpressure-aware IPC, network process, sandbox, or crash isolation.
- The API is synchronous and must not run on DOM/UI event loops. Browser loader and IPC adoption
  remain separate gates.
- Registry-source vendoring, complete license/source admission, native dependency review, AppImage
  dynamic closure, packaged networking, and prohibited-endpoint audits remain orchestrator work.
- No general-site, Google, DuckDuckGo, YouTube, rendering, Web Platform Test, or Firefox parity claim
  follows from this slice.
