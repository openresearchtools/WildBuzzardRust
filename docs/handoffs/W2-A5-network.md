# W2-A5 bounded network handoff

- Task: W2-A5 bounded URL and HTTP/1.1 transport nucleus
- Owner: Agent 5 — network, security, and storage; hardened, integrated, and independently reviewed by the main orchestrator
- Status: Complete for the Wave 2 numeric-loopback transport contract; this is not production internet access or Fetch/network parity
- Firefox commit and source paths: ESR153 `c19b7e89270787889495688244ec6ee8e79288a1`; request/response heads, HTTP transactions, chunked decoding, and validation paths are enumerated in `netwerk/rust/wild_buzzard_net/README.md`
- Firefox test paths: focused HTTP response-head, header, content-length underrun, chunked-response, obs-fold, and split-request fixtures are recorded in the crate README with relevant history revisions
- Wild Buzzard paths changed: `netwerk/rust/wild_buzzard_net`, root `Cargo.toml`, root `Cargo.lock`, and this status/handoff evidence
- Contract added or changed: numeric-loopback `LoopbackTarget`; validated methods and fields; bounded request serialization; strict response framing; streaming fixed, chunked, and close-delimited bodies; shared cancellation/deadline contract; structured transport errors
- Tests run and results: 37 deterministic loopback integration tests passed, 0 failed/ignored; root-integrated package formatting, all-target check, strict all-feature Clippy, locked tests, release build, and warning-denied rustdoc passed for `x86_64-unknown-linux-gnu` using the external build directory
- Parity evidence: strict bounded HTTP/1.1 wire behavior for one loopback connection only; terminal protocol, limit, EOF, and non-timeout I/O body errors latch permanently, discard partial trailers, and prevent retry exposure; inactivity timeout retry is covered
- Known behavioral differences: no DNS, TLS, certificate verification, proxies, cookies, cache, authentication, decompression, CORS, CSP, referrer policy, HTTP/2, HTTP/3, QUIC, connection pool, async adapter, or external address capability
- Unsafe or FFI introduced: None; the first-party crate forbids unsafe code and has no native wrapper
- Licenses and provenance: MPL-2.0 first-party code informed by the ignored pinned ESR reference; dependencies are the imported local `url` 2.5.7 Rust crate and the MPL-2.0 Wild Buzzard runtime cancellation crate
- Provider or network implications: no provider endpoint, telemetry, resolver, or external-network test; the connectable target type admits only numeric IPv4 loopback addresses and `::1`
- Blocked on: the HTML/Fetch owner must add policy and an asynchronous/backpressured adapter before this contract can leave loopback or run on a DOM/UI event loop
- Recommended next action: connect bounded response bytes to incremental HTML parsing for the first vertical slice, preserving cancellation, limits, response filtering, and the explicit future security-policy boundary
