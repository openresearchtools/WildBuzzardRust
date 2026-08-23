# W9-A5O stylesheet fetch owner — corrected through W9-A5T5

- Original slice: W9-A5O
- Hostile corrections: W9-A5P, W9-A5Q, W9-A5R, W9-A5S, W9-A5S-C1, W9-A5T4, W9-A5T5
- Self-review: **GO for this bounded fetch/response-admission slice**
- Product status: no stylesheet-fetch consumer, CSS parsing, application, or rendering integration
- Firefox reference: ESR153 `c19b7e89270787889495688244ec6ee8e79288a1`

## Result

`StyleFetchOwner` consumes immutable `StyleResourcePlan` request identities in deterministic
document order. It owns a bounded child `GeneralWebClient` delegated from the exact client and
response which fetched the document. Redirects are returned manually and revalidated. The owner
checks exact plan/document/request ownership, cancellation, deadline, response count, exchange
count, redirect count, retained body/header bytes, status, MIME, nosniff, mixed content, CSP
continuity, transport security, and Local Network Access before returning one all-or-nothing
`StyleFetchSet`.

Successful results retain only the bounded final URL, status and connection-security facts, typed
CSSOM origin cleanliness, merged Content-Type evidence, selected charset, MIME/nosniff result, and
complete bounded body needed by a future CSS parser. This slice does not parse or apply CSS.

W9-A5T5 closes detached-owner liveness and quota replay:

1. Each authoritative response document owns a private `Arc` lifecycle ledger guarded by one
   mutex. Its state is an enum, not a raw or copied boolean. A unique non-Clone current-document
   owner moves with the live DOM page; explicit retirement or `Drop` changes the ledger
   monotonically to `Retired`.
2. Direct document replacement retires the old ledger before installing the replacement. The
   navigation owner retains an independent current-commitment map and retires it before publishing
   a replacement frame/document, closing a context, invalidating a document, stopping, or
   completing worker teardown. A successful DOM mutation also retires the initial response
   document's stylesheet authority before the new document version can become observable.
3. Fetch admission acquires the exact ledger and keeps its transaction guard for the whole bounded
   operation. Retirement and admission therefore have one race-safe linearization point: if
   retirement wins, fetch fails before DNS/socket creation; if fetch wins, replacement waits while
   the old document remains current, then retires it immediately after the transaction releases.
4. Issuance is `Available -> Issued(Ready) -> Issued(Active) -> Issued(Consumed)` or `Retired`.
   There is one issuance and one transaction per exact response document. Dropping an unused
   authority does not restore the issuance. The first `fetch_plan` call consumes the transaction
   whether it succeeds, is cancelled, has an invalid plan/deadline, or fails admission. Repeated
   calls and duplicate minting cannot multiply quotas.
5. Product `StyleFetchAuthority` requires an exact non-optional `NavigationId` already bound to
   the same final response and `DocumentVersion`. A direct `StaticPageEngine` document has
   `navigation() == None` and receives `ProductNavigationRequired`. Direct deterministic fixtures
   use the separate `NonProductStyleFetchAuthority`/`NonProductStyleFetchOwner` types; there is no
   conversion into the product type.
6. Both authorities and both owners are non-Clone. `fetch_plan` requires `&mut self`; compile-fail
   doctests cover authority copying, non-product promotion, and two simultaneous mutable owner
   borrows. The shared ledger remains the defense-in-depth serialization boundary.

## Identity and transport authority retained from W9-A5T4

`GeneralWebResponse` carries an opaque non-`Copy` `CommittedResponseAuthority` issued only after
the exact selected candidate connection and response head. Private state binds a never-reused
client identity, response identity, exact fragment-free target/origin, connection security, and
peer address space. Its `Debug` output omits the target.

Authoritative `NavigationCommitMetadata` is created atomically from that final response, verified
against the canonical final browser identity, bound to the parsed `DocumentVersion`, and then
bound once to the worker `NavigationId`. Clones share the same private document and revocation
ledger. Synthetic metadata can validate URL/security structure but has no response or subresource
authority.

`GeneralWebClient::delegate_for_response` and
`GeneralWebClient::network_access_for_committed_response` require the hidden issuing-client
identity. The identity-preserving child reuses the originating resolver and TLS trust state while
replacing every transport limit with the exhaustive sealed stylesheet profile. An unrelated
client cannot redeem the commitment.

The immutable style plan still contains no client, resolver, socket, TLS, LNA, cookie, credential,
or other ambient network capability.

## Prior security corrections preserved

### Restricted ports

Every `GeneralWebClient` execution rejects the pinned ESR153 restricted-port set before request
preparation, DNS, or socket creation. There is no override. The gate therefore applies to initial
requests and each manually followed redirect:

```text
1, 7, 9, 11, 13, 15, 17, 19, 20, 21, 22, 23, 25, 37, 42, 43, 53, 69,
77, 79, 87, 95, 101, 102, 103, 104, 109, 110, 111, 113, 115, 117, 119,
123, 135, 137, 139, 143, 161, 179, 389, 427, 465, 512, 513, 514, 515,
526, 530, 531, 532, 540, 548, 554, 556, 563, 587, 601, 636, 989, 990,
993, 995, 1719, 1720, 1723, 2049, 3659, 4045, 4190, 5060, 5061, 6000,
6566, 6665, 6666, 6667, 6668, 6669, 6679, 6697, 10080
```

Tests compare every `u16` to the exact table, cover adjacent allowed ports, and prove restricted
domain and numeric targets reach neither resolver nor listener.

### Local Network Access

Every production `GeneralWebRequest` carries explicit immutable initiator address-space and
Local/Private permission evidence. Each resolved candidate is classified and authorized
immediately before its socket attempt; mixed DNS answers cannot bypass the check. Stylesheets
reuse the exact final committed document evidence on the initial and every redirect request, with
both permissions explicitly denied in this slice.

| Candidate | Address space |
| --- | --- |
| IPv4 `127/8` or exact `0.0.0.0` | Local |
| IPv6 exact `::1` or `::` | Local |
| IPv4 `0.0.0.1` through `0.255.255.255` | Private |
| IPv4 RFC1918, `100.64/10`, or `169.254/16` | Private |
| IPv6 `fc00::/7` or `fe80::/10` | Private |
| IPv4-mapped IPv6 | normalized and classified as IPv4 |
| Every other IP | Public |

Public-to-Private requires exact Private `Granted`; Public-to-Local and Private-to-Local require
exact Local `Granted`. Denied, Pending, Unknown, and absent permission fail closed whenever a
grant is required. Same-space and more-public transitions remain eligible. Hostname spelling and
DNS do not promote address space.

There is no permission UI, persistence, PNA preflight, service-worker integration, or claim of
full Fetch/LNA parity.

### Response admission

- Final status must be `200..=299`; manual redirect responses are revalidated at every hop.
- Identical repeated `Location` fields are accepted after HTTP-whitespace trim; differing values
  reject.
- Modern merged Content-Type extraction runs first. Total failure invokes the ESR153
  default-enabled legacy parser against only the latest original Content-Type field.
- Repeated Content-Type inserts a comma only when the accumulated merged buffer is nonempty.
- Unquoted charset parsing trims trailing HTTP whitespace and preserves leading whitespace.
- `text/html garbage` remains explicit non-CSS; `text/css garbage` is recovered as CSS by the
  legacy fallback.
- XCTO merges fields in wire order and inspects only the first trimmed comma token,
  ASCII-insensitively: `nosniff,foo` enforces and `foo,nosniff` does not.
- CSSOM origin state is Clean only when the document principal subsumes the final response and
  every redirect stayed same-origin. Initial cross-origin loads and any cross-origin hop taint the
  result without retaining redirect URLs for this purpose.
- HTTPS-to-HTTP remains blocked except for syntactic loopback names, IPv4 `127/8`, or exact `::1`;
  IPv4-mapped IPv6 and non-loopback forms reject before connection. DNS never promotes trust.
- Report-only CSP diagnostics are bounded-lossy and non-authoritative. An authoritative redirect,
  unknown-MIME, or rejection diagnostic can evict the oldest report-only record. Zero or exhausted
  diagnostic capacity cannot alter bytes, network behavior, success, or rejection.

### Bounds and configuration drift

| Resource | Hard maximum |
| --- | ---: |
| Final responses | 64 |
| Retained body per response | 2 MiB |
| Aggregate retained bodies | 16 MiB |
| Redirects per request | 8 |
| HTTP exchanges | 256 |
| Diagnostics | 1,024 records |
| Merged retained Content-Type | 4 KiB |
| Aggregate Content-Type plus charset | 256 KiB |
| Caller deadline horizon | 30 seconds |
| Chunk-size line excluding CRLF | 8 KiB |

All caller limits can only narrow these maxima. Retention uses checked arithmetic and fallible
reservation. Failure returns no partial response set.

`ClientConfig::try_new_explicit_v1` and `GeneralWebConfig::try_new_explicit_v1` are exhaustive
in-module `Self { ... }` literals, not `Default` seeds or struct updates. A future private field
makes its owning literal fail compilation until the explicit style policy is reviewed. Tests check
every getter and reject out-of-policy timeout/quota values, including an enlarged chunk-line bound.

## Privacy

Errors and diagnostics retain typed document/owner/request/redirect identity, status/limit kind,
and coarse network family only. Their `Debug`/`Display` output includes no peer URL, host, path,
query, fragment, raw header/body, nonce, CSP text, or underlying peer error string. Successful
response accessors intentionally expose admitted bounded final identity/header/body evidence to a
future CSS parser; custom `Debug` implementations expose only metadata and lengths.

`StyleFetchDiagnostic::owner` is exactly a `NodeId`; plan-level failures produce no diagnostic
record. The documentation no longer implies an optional owner value.

## Exact source scope

- `netwerk/rust/wild_buzzard_net/src/client.rs`
- `netwerk/rust/wild_buzzard_net/src/general.rs`
- `netwerk/rust/wild_buzzard_net/src/lib.rs`
- `browser/wild_buzzard_engine/src/navigation.rs`
- `browser/wild_buzzard_engine/src/pipeline.rs`
- `browser/wild_buzzard_engine/src/style_fetch.rs`
- `browser/wild_buzzard_engine/src/lib.rs`
- `browser/wild_buzzard_engine/tests/style_fetch.rs`
- `browser/wild_buzzard_engine/tests/redirect_navigation.rs`
- `docs/handoffs/W9-A5O-style-fetch.md`

W9-A5T5 changed only the engine lifecycle/style files, focused tests, exports, and this handoff;
the three network files retain the already reviewed W9-A5R/S/S-C1 implementation. No manifest,
lockfile, parser, JavaScript, media, automation, Firefox reference, or unrelated lane file was
edited by this correction. Nothing was staged, committed, or pushed.

## Firefox ESR153 evidence inspected read-only

- `layout/style/Loader.cpp`, `Loader.h`, and `SheetLoadData.h`: `DropDocumentReference`, `Stop`,
  cancellation, sheet-ready checks, origin-clean propagation, MIME/nosniff admission, and redirect
  same-origin evidence.
- `netwerk/base/nsIOService.cpp`: exact `gBadPortList`; narrow history included
  `47e078bfd4c2db0da11489bbbec182a6c57070e4`.
- `netwerk/dns/DNS.cpp`, `netwerk/base/nsNetUtil.cpp`, and
  `netwerk/protocol/http/nsHttpTransaction.cpp`: address-space classification and connection gate.
- `netwerk/test/gtest/TestLocalNetworkAccess.cpp` and
  `netwerk/test/unit/test_local_network_access.js`, including mapped and boundary cases/history.
- `netwerk/protocol/http/nsHttpResponseHead.cpp`, `netwerk/base/nsURLHelper.cpp`,
  `dom/base/test/gtest/TestMimeType.cpp`, and the fallback preference in `StaticPrefList.yaml`.
- Narrow CSP style-src-elem, mixed-content, and link-stylesheet MIME/redirect tests and history.

The Firefox checkout was not modified and is not a build input.

## Verification

Frozen identities:

```text
Wild Buzzard shared-worktree HEAD: 5739fa22359919b86a4bda4771fd6ac367592884
Firefox ESR153 reference HEAD:    c19b7e89270787889495688244ec6ee8e79288a1
```

Every authoritative container command used:

```text
/run/media/user/Data/Repositories/wildbuzzardbuilds/podman/podman-wildbuzzard run --rm --pull never --network none ...
```

All Cargo homes, targets, logs, temporary files, Python tooling, and binaries are under:

```text
/run/media/user/Data/Repositories/wildbuzzardbuilds/w9-a5t5-style-liveness/
```

Cargo commands used `--locked --offline --target x86_64-unknown-linux-gnu`,
`CARGO_HOME=/build/cargo-home`, `CARGO_TARGET_DIR=/build/target`, `TMPDIR=/build/tmp`, and
`XDG_CACHE_HOME=/build/xdg-cache`. Engine commands also used
`PYTHON3=/build/python/bin/python3`.

Final gates:

- `cargo test --lib security_policy_tests -- --test-threads=1` in the network crate:
  **10 passed**.
- `cargo test --all-targets -- --test-threads=1` in the network crate: **73 passed, 1 manual
  public-network test ignored**; all **37 inherited HTTP parser/integration tests passed**.
- `cargo test --lib` in the engine workspace: **45 passed**.
- `cargo test --test style_fetch -- --test-threads=1`: **28 passed**.
- The exact gated replacement/admission test repeated in a 50-iteration shell loop: **50/50
  passed**.
- `cargo test --test redirect_navigation -- --test-threads=1`: **6 passed**.
- `cargo test --workspace --all-targets -- --test-threads=1`: **156 passed, 2 opt-in
  public-network tests ignored**.
- Network and engine `cargo clippy --all-targets --no-deps -- -D warnings -W clippy::all
  -W clippy::pedantic`: **passed**.
- Network and engine `RUSTDOCFLAGS=-Dwarnings cargo doc --no-deps`: **passed**.
- Network `cargo test --doc`: **1 compile-fail doctest passed**. Engine `cargo test --doc`:
  **3 compile-fail doctests passed**.
- Engine `cargo check --workspace --all-targets --release`: **passed** with one pre-existing
  `webrender` dead-field warning outside this task.
- Engine `cargo test --release --test style_fetch -- --test-threads=1`: **28 passed**.
- Network and engine `cargo metadata --no-deps --format-version 1`: **passed**. All 21 metadata
  package entries have Data-workspace manifest paths and contain zero Firefox inputs.
- `rustfmt --edition 2024 --config skip_children=true --check` over the exact nine Rust files:
  **passed**.
- Runtime scans over the exact Rust scope found zero unsafe/native/generated constructs and zero
  site-specific host literals/branches. Source-tree scans found no `target`, `__pycache__`, `.pyc`,
  or `.pyo` artifact below either changed crate.
- Host `git diff --check` plus explicit whitespace/conflict-marker checks for the two untracked
  style files and this handoff: **passed**.

The authoritative logs are in the lane's `logs/` directory. An initial Podman `--userns keep-id`
probe was rejected by the existing graph-root permissions; all successful gates used the required
wrapper without that extra flag. The browser build image lacks rustfmt/Clippy components, so those
gates used `localhost/wildbuzzard-rust-tests:1.90-trixie-tools`. A first scan attempted `rg` in that
tools image, found it unavailable, and was discarded; the recorded scan was rerun successfully
with `grep`.

## Frozen SHA-256

```text
8231506cdc5efe33784bc6202bafc00b3b1d94dfd2305f45ac6aa81dc52ba0a4  netwerk/rust/wild_buzzard_net/src/client.rs
e1f1345c6b1de869386d034d859970f4e4ebde9c6eb99628c92e41a084cf3819  netwerk/rust/wild_buzzard_net/src/general.rs
0fb3819b5cfb4a3cab41679c8de31c139a2d6e6927e618a1611167b8f25539c8  netwerk/rust/wild_buzzard_net/src/lib.rs
3fab8aeabe7130ff5f3b36debb45b35550268d20915507a5d31e084a060dc7f4  browser/wild_buzzard_engine/src/navigation.rs
d0391e0fd5cfbe152192c06fb8ea5e243481bf50d22a3ca64cb62deb3ee4930c  browser/wild_buzzard_engine/src/pipeline.rs
f6e7caf66bb4254d4ae756135d59f857572342ba8cab4b93158d1127d950bfdc  browser/wild_buzzard_engine/src/style_fetch.rs
e525326ed4ab61d7ce6423fed418e0a9fb8443cf475849cfbef0d09576fbf95a  browser/wild_buzzard_engine/src/lib.rs
7674f64fdbf65e8717d6e869174e22f9b790e4287efefc7081647bdfb5d8191d  browser/wild_buzzard_engine/tests/style_fetch.rs
894b97608955473f8d832227bb0cdf2a3b1aed7dfce3530534576e2fbae44163  browser/wild_buzzard_engine/tests/redirect_navigation.rs
eb4dce028f48229745b5434c379c448c8964a2a4ef99b37fa785a25438a26fca  debug style_fetch test binary
231aafe6fabcbc391f286744ccfa6a7a33d9eec4f5ad99ae2f81d584a4045249  release style_fetch test binary
4891701fc6690db35c54d40bb462ae3fddaf7ffb45e20ebf8436b046bebe4fde  debug redirect_navigation test binary
623bfe80085d8ec8ebc64cd422448ab79dafc0feebf92c181b26aa4ef8e9eea9  debug loopback_http test binary
e549fd9e9404fb53a145ea248e026d62b0f6994f5f74ae2e914f897bfd6c92ee  network metadata JSON
3905458b5f3ff25e882dfd0fbf49f4e07eee32f560ff9dccc15ea14a16808b7b  engine metadata JSON
2d5109c15069fd2bb2fdf40f1118b5440f0adb20ef3945a24ec9c22ca8526ee2  24-entry lane logs/artifacts SHA256SUMS
```

The handoff excludes itself from the embedded hash list to avoid a self-referential digest.

## Remaining gaps and non-claims

- There is still no product consumer of `StyleFetchOwner`. The navigation worker binds and revokes
  product authority, but it does not yet invoke stylesheet fetch. The exercised fetch planner is
  the explicitly separate direct local HTTP/TLS non-product seam.
- No CSS parsing, stylesheet construction, CSSOM exposure, application, cascade integration,
  layout invalidation, rendering, DOM mutation by this fetch slice, JavaScript, cookies,
  credentials, SRI, CORS expansion, or CSP report delivery is implemented here.
- Revocation is a same-process Rust ownership/ledger protocol. No IPC serialization,
  cross-process delegation, process isolation, or persisted authority is claimed.
- A replacement which loses the transaction race waits for the active fetch to finish; it does not
  asynchronously preempt the transport. The wait is bounded by the fetch owner's 30-second hard
  deadline horizon. A pending navigation does not retire the displayed document until replacement
  publication succeeds; failed or cancelled replacement leaves the old document current.
- The one-issuance/one-transaction policy is deliberately conservative. A future CSS parser/import
  loader must extend quotas under the same document ledger rather than minting detached owners.
- The initial HTTPS-to-HTTP loopback rule is intentionally syntactic and conservative, separate
  from resolved-address LNA classification.
- LNA permission acquisition, PNA preflights, service workers, proxies, caches, HTTP/2, HTTP/3,
  and Fetch/CSP edge behavior beyond the recorded gates remain open.
- CSS application/rendering, full browser behavior, and YouTube parity remain open. This slice
  does **not** claim full Fetch, LNA, CSP, CSS, browser, or YouTube parity.
