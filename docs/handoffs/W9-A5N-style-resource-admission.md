# W9-A5N: bounded external stylesheet discovery and admission

- Task: W9-A5N
- Owners: Agent 5 security/network policy prerequisite and Agent 3 DOM/style integration prerequisite
- Status: **IMPLEMENTED — PRE-FETCH ADMISSION GATES PASS; FETCHING AND STYLING REMAIN DISCONNECTED**
- Exact live verification base: `8aba65d110c531bc06185f36f07c9cc765602355`
- Firefox baseline: ESR153 commit `c19b7e89270787889495688244ec6ee8e79288a1`
- Product target: Linux x86-64

## Outcome

`StyleResourcePlan` constructs one immutable, bounded, capability-free pre-fetch plan from an
immutable `wild_buzzard_dom::DocumentSnapshot` and the exact
`CapturedDocumentResponseMetadata` which produced its initial revision. It validates the exact
document version, snapshot ownership, policy-envelope binding, final navigation commitment, and
canonical final response URL before examining style resources.

In DOM order the planner:

1. uses the exact final response URL as the fallback document base;
2. examines only the first HTML `base[href]`, resolves it against that fallback, evaluates
   `base-uri` through `StylePolicySet`, and either selects it or retains one typed rejection while
   keeping the fallback;
3. discovers only HTML `link` elements whose ASCII-case-insensitive `rel` token list contains
   `stylesheet`;
4. applies every W9-A3P first-gate attribute and URL rule;
5. rejects a direct HTTPS-document to HTTP-stylesheet transition before invoking the style-policy
   matcher;
6. evaluates every applicable enforcing and report-only policy, including a borrowed link nonce;
   and
7. emits only canonical, credential-free, fragment-free HTTP(S) request identities for admitted
   candidates.

No request is made. No CSS is parsed or applied. This gate is not external-CSS rendering parity.

## Exact source scope

- `browser/wild_buzzard_engine/src/style_resources.rs` (new)
- `browser/wild_buzzard_engine/src/lib.rs`
- `browser/wild_buzzard_engine/tests/style_resource_admission.rs` (new)
- `docs/handoffs/W9-A5N-style-resource-admission.md` (new)

There is no manifest, lockfile, pipeline, dynamic-document, README, program-status, root-agent,
JavaScript, graphics, network, Stylo, Firefox-reference, or existing-test edit from this task. The
real repository index was not modified, and this task did not stage, commit, or push anything.
Unrelated concurrent worktree changes were preserved and excluded from every task-scoped check.

## Exact input and version contract

`StyleResourcePlan::from_snapshot` first requires the snapshot version to equal
`CapturedDocumentResponseMetadata::response_document_version()`. It then verifies that the
document node and every document-order node belong to the snapshot's exact `DocumentId`.

`StylePolicySet::from_response_metadata` performs the existing bounded validation of the captured
enforcing/report-only CSP fields and exact `NavigationCommitMetadata`. The planner independently
requires the policy set's response version and navigation commitment to remain equal to the supplied
metadata. It reparses the exact final URL through the existing `GeneralWebTarget` boundary and
requires byte-for-byte canonical identity before cloning the commitment into the plan. A mismatch,
invalid commitment, unsupported scheme, credentials, or noncanonical final identity fails before
DOM discovery.

`StyleResourcePlan::from_live_document` is only a convenience boundary: it requests one immutable
snapshot and delegates to the same exact constructor. A live revision which advanced after the
captured response therefore fails the exact-version gate.

## Base selection

The final response URL, including its canonical query and fragment identity, is retained as the
fallback base. Discovery skips HTML `base` elements without `href`; the first HTML `base[href]` is
the sole candidate. SVG/foreign-namespace elements do not qualify. Once that candidate is found,
the scan stops regardless of its outcome, so a later base can never recover from or replace an
invalid, unsupported, over-limit, credential-bearing, or CSP-blocked first candidate.

The candidate is resolved with the existing WHATWG URL implementation against the exact fallback.
Only canonical credential-free HTTP(S) results within 16 KiB can reach
`StylePolicySet::evaluate_base_uri`. An enforcing block keeps the fallback. A report-only block is
counted and diagnosed but does not prevent selection. Base evidence retains only the owning node,
document version, selected/rejected status, and privacy-safe policy decision; rejected URL text is
not retained.

The selected base may retain a fragment because it is a document base identity. Every stylesheet
request identity is separately fragment-stripped.

## Link discovery and first-gate admission

`rel` tokenization uses exactly HTML ASCII whitespace: TAB, LF, FF, CR, and SPACE. Token matching
for `stylesheet` and `alternate` is ASCII-case-insensitive; vertical tab, non-ASCII confusables,
and substring matches do not delimit or create tokens.

Every discovered `link[rel~=stylesheet]` consumes one candidate record in document order. Before
URL or policy evaluation it must satisfy all of these rules:

| Input | Admitted first-gate form |
| --- | --- |
| `href` | present and not exactly empty |
| resolved URL | canonical HTTP(S), no username/password |
| `type` | absent, empty after HTML-ASCII trimming, or `text/css` essence with optional parameters, ASCII-case-insensitive |
| `disabled` | absent |
| `crossorigin` | absent, including rejection of the present-empty form |
| `integrity` | absent or exactly empty |
| `title` | absent or exactly empty |
| `rel~=alternate` | absent |

All applicable attribute rejections are retained for a candidate until the diagnostic bound is
reached. A rejected candidate never reaches URL resolution or CSP. Resolution uses the selected
document base, rejects non-HTTP(S) schemes and credentials, requires canonicalization through
`GeneralWebTarget`, and strips fragments before request identity retention.

For an HTTPS final document, a directly resolved HTTP sheet is rejected as mixed content before
`StylePolicySet::evaluate_external_style`. This ordering is visible in evidence: the candidate has
no policy decision and does not increment enforcing/report-only policy-block counters. Same-origin
and cross-origin HTTP(S) candidates otherwise proceed through the same generic path.

The candidate nonce is borrowed only for the synchronous policy match and is never copied into the
plan. Every enforcing policy must admit the candidate. Report-only policies use the same matcher
but only contribute would-block evidence. Admitted records retain the request index and policy
decision; rejected records retain no URL or attribute text.

## Capability and ownership boundary

The public plan owns only:

- the exact document version and cloned final navigation commitment;
- fallback and selected canonical base strings;
- optional first-base owner/status/policy evidence;
- bounded candidate records;
- bounded canonical request-identity strings;
- bounded redacted diagnostics and aggregate policy counts; and
- one checked aggregate of retained request-URL bytes.

It owns no client, request builder, socket, DNS resolver, cookie jar, credential store, referrer
state, renderer, stylesheet parser, callback, channel, report sender, logger, DOM handle, or mutable
live-document capability. Public slices are immutable, fields are private, and the plan is proven
`Send + Sync`. `StyleResourceRequestIdentity` contains only its exact document version, owner node,
canonical request URL, and privacy-safe policy decision.

The network-backed acceptance fixtures keep their listeners armed until both plan construction and
engine shutdown. Each HTTP fixture requires exactly one top-level document request and fails if a
second connection arrives. The TLS fixture accepts exactly one authenticated document connection,
and a separate nonblocking HTTP listener proves that the mixed-content URL receives no connection.
Thus the tests prove zero stylesheet request or network side effect while the plan exists.

## Bounds, allocation, and failure behavior

| Resource | Hard bound | Exact excess behavior |
| --- | ---: | --- |
| DOM-order stylesheet candidate records | 64 | fail the complete plan; never truncate |
| One canonical fallback/base/request URL | 16 KiB | invalid final fallback fails the plan; base or link candidate is rejected |
| One scanned `rel`, `href`, `type`, or `nonce` value | 16 KiB | over-limit `rel` fails discovery; over-limit base/link content is rejected |
| Retained privacy-safe diagnostics | 256 | fail the complete plan; never truncate |

The attribute scan cap is a conservative first-gate safety bound added because discovery cannot
safely claim bounded work while scanning arbitrary peer-controlled attribute text. Presence-only
attributes (`disabled` and `crossorigin`) do not scan their values. Integrity and title values are
tested only for empty/nonempty presence and are never copied or parsed at this gate.

Candidate, request, and diagnostic vectors reserve their complete maximum capacities fallibly
before discovery. Every retained URL is copied only after a fallible exact reservation. Candidate,
diagnostic, diagnostic-range, aggregate request-byte, and policy-block arithmetic is checked. The
64-candidate and 16-KiB URL caps also bound admitted request identity text to at most 1 MiB, with
the exact aggregate still checked. Any hard invariant, counter, or allocation failure rejects the
complete plan rather than returning partial state.

The underlying existing WHATWG URL parser necessarily performs its own internal Rust allocations;
this slice cannot convert those dependency internals to fallible allocation without expanding
scope. All allocations newly retained by this plan use the explicit fallible boundaries above.

## Privacy-safe diagnostics

Diagnostics contain only document version, optional owner node, operation category, stable reason,
and bounded counts/lengths. They never contain a URL, host, path, query, fragment, raw attribute,
CSP field, nonce, integrity value, title, or other peer-controlled text.

The plan and request identity have custom `Debug` implementations. URLs are represented only by
byte lengths in `Debug`; request URLs remain available solely through the intentional typed
`canonical_url()` accessor. `StyleResourcePlanError` `Debug`/`Display` expose only stable variants,
versions, limits, counts, allocation categories, and the already-redacted `StylePolicyError`.
Nonce exact text is neither retained nor exposed by any success or failure object.

## Firefox ESR153 and WPT evidence

The ignored Firefox checkout was inspected narrowly and remained read-only. It was never a source,
fixture, include path, or build input. Focused implementation references were:

- `dom/base/Document.cpp`: document base URI lookup/cache and fallback behavior;
- `dom/base/nsContentUtils.cpp`: relative URL resolution against a base;
- `dom/html/HTMLSharedElement.cpp`: first `base[href]`, fallback, and `base-uri` CSP handling;
- `dom/html/HTMLLinkElement.cpp`: link relation, type, title, disabled, href, and stylesheet update
  gates;
- `dom/base/Link.cpp`: canonical link URL handling;
- `dom/security/nsCSPUtils.cpp`: pre-request style/base policy iteration, nonces, and report-only
  decisions; and
- `dom/security/nsCSPContext.cpp`: enforcing and report-only context behavior.

Focused WPT evidence was inspected under:

- `testing/web-platform/tests/html/semantics/document-metadata/the-link-element/` for relation-token,
  stylesheet type, alternate/title, disabled, and URL behavior;
- `testing/web-platform/tests/content-security-policy/style-src-attr-elem/` for external style
  directive fallback and separation; and
- `testing/web-platform/tests/content-security-policy/base-uri/` for first-base selection,
  resolution, and allow/deny behavior.

The implementation follows generic web-platform behavior. It contains no site, YouTube, domain,
or Firefox-brand special case.

## Acceptance evidence

The 12 focused integration tests cover:

- exact response/snapshot mismatch and exact final commitment/fallback identity;
- final-URL relative, query-only, fragment-only, and root-relative resolution;
- valid, invalid-scheme, and CSP-blocked first bases with later-base non-selection;
- HTML ASCII whitespace, token casing, alternate, vertical-tab, and Unicode-confusable `rel`
  controls;
- absent/empty, case-varied and parameterized `text/css`, and wrong `type` values;
- disabled, present-empty `crossorigin`, empty/nonempty integrity, empty/nonempty title, missing href,
  and empty href;
- canonical same-origin and cross-origin HTTP(S), fragment removal, credential rejection,
  non-HTTP schemes, and malformed URLs;
- matching and nonmatching link nonces across intersecting enforcing policies, two report-only
  policies, exact aggregate counts, and nonce/URL redaction;
- HTTPS-to-HTTP direct mixed-content rejection before policy or network access;
- exact DOM order and owner/document-version/request-index evidence;
- exact 64/65 candidate, 256/257 diagnostic, 16-KiB/next-byte attribute, and 16-KiB/next-byte
  canonical-URL edges;
- the exact fallback URL edge (its next byte is already rejected by the upstream captured-metadata
  URL bound, so the request-URL next-byte case exercises this planner's local URL excess);
- malformed and oversized peer-controlled inputs;
- privacy-safe plan, request, diagnostic, and error `Debug`/`Display`; and
- a compile-time `Send + Sync` assertion plus live listener evidence of zero stylesheet fetches.

## Verification

Every build and test artifact was placed under:

`/run/media/user/Data/Repositories/wildbuzzardbuilds/w9-a5n-style-resource-admission`

Podman used the Data-rooted wrapper
`/run/media/user/Data/Repositories/wildbuzzardbuilds/podman/podman-wildbuzzard`; no default
system-drive Podman graph root was used. Cargo ran offline with:

```text
CARGO_TARGET_DIR=/build/task/target
CARGO_HOME=/build/task/cargo-home
PYTHON3=/build/task/python/bin/python
target=x86_64-unknown-linux-gnu
```

Toolchain: `rustc 1.90.0 (1159e78c4 2025-09-14)`, LLVM 20.1.8,
`cargo 1.90.0 (840b83a10 2025-07-30)`, `rustfmt 1.8.0-stable`,
`clippy 0.1.90`, Python 3.13.5, and OpenSSL 3.5.1.

Exact final commands and outcomes:

```sh
rustfmt --edition 2024 --check \
  browser/wild_buzzard_engine/src/style_resources.rs \
  browser/wild_buzzard_engine/src/lib.rs \
  browser/wild_buzzard_engine/tests/style_resource_admission.rs
# PASS, no output

cargo test --manifest-path browser/wild_buzzard_engine/Cargo.toml \
  --locked --offline --target x86_64-unknown-linux-gnu \
  --test style_resource_admission -- --test-threads=1
# PASS: 12 passed, 0 failed, 0 ignored

cargo test --manifest-path browser/wild_buzzard_engine/Cargo.toml \
  --workspace --locked --offline --target x86_64-unknown-linux-gnu -- --test-threads=1
# PASS: 122 passed, 0 failed, 2 ignored opt-in public-network comparisons

cargo clippy --manifest-path browser/wild_buzzard_engine/Cargo.toml \
  --workspace --all-targets --locked --offline --target x86_64-unknown-linux-gnu \
  --no-deps -- -D warnings -W clippy::all -W clippy::pedantic
# PASS

RUSTDOCFLAGS='-D warnings' cargo doc \
  --manifest-path browser/wild_buzzard_engine/Cargo.toml \
  --workspace --locked --offline --target x86_64-unknown-linux-gnu --no-deps
# PASS

git diff --check -- \
  browser/wild_buzzard_engine/src/lib.rs \
  browser/wild_buzzard_engine/src/style_resources.rs \
  browser/wild_buzzard_engine/tests/style_resource_admission.rs \
  docs/handoffs/W9-A5N-style-resource-admission.md
# PASS, using a task-local temporary index so the two new files and this handoff are checked
# without modifying the real repository index
```

The complete workspace breakdown was 39 library tests; 6 document-policy, 8 dynamic-document,
5 general-navigation, 33 navigation-facade, 6 redirect, 4 static-pipeline, 9 style-policy, and 12
style-resource integration tests. The two ignored general-navigation tests are explicitly opt-in
public-network comparison captures.

Retained external artifacts at final verification:

| Purpose | Exact path | Bytes |
| --- | --- | ---: |
| Reusable Cargo target | `/run/media/user/Data/Repositories/wildbuzzardbuilds/w9-a5n-style-resource-admission/target` | 7,483,984,819 |
| Task-local Cargo home | `/run/media/user/Data/Repositories/wildbuzzardbuilds/w9-a5n-style-resource-admission/cargo-home` | 176,939,266 |
| Task-local Stylo Python environment | `/run/media/user/Data/Repositories/wildbuzzardbuilds/w9-a5n-style-resource-admission/python` | 11,143,345 |

Transient TLS certificates, keys, fixture pages, listeners, and processes were explicitly cleaned.
No repository-local target, cache, generated fixture, log, screenshot, or package was created.

## Deliberate remaining work and risks

W9-A5N deliberately does not fetch, redirect, attach credentials/cookies/referrers, enforce response
MIME or nosniff, verify SRI, implement CORS, parse CSS, process `@import`, resolve nested resources,
apply media queries, build a stylesheet set, feed Stylo, restyle, layout, render, send CSP reports,
or recover from a failed sheet. It does not process dynamically inserted or mutated links; the
exact initial-revision contract rejects later live versions. Meta-CSP timing is outside this
captured-response-only gate.

Mixed-content handling here covers only the direct final-document-to-initial-sheet identity. The
future fetch owner must repeat policy and mixed-content checks across every redirect, then add
bounded body/MIME/encoding/cancellation/deadline behavior before any bytes reach a parser. It must
not treat a report-only decision as permission, expose nonce or URL text through diagnostics, or
turn this data-only plan into a holder of ambient network/reporting/rendering authority.

This slice is therefore a prerequisite and security boundary, not a claim of external-author-CSS,
browser, or Firefox ESR153 parity.

## Frozen source hashes

| Path | SHA-256 |
| --- | --- |
| `browser/wild_buzzard_engine/src/style_resources.rs` | `7092fad751a6d4d5a3723982d252599937c9af88e15ee1d380991e4bdaa3cd91` |
| `browser/wild_buzzard_engine/src/lib.rs` | `95d6d8abc08cef9b1552936a5920052a7cacc545fdc71c724e2b0baaa47872f3` |
| `browser/wild_buzzard_engine/tests/style_resource_admission.rs` | `6b8d3020bab38c10f523f7533758f4908e1457138d008763d93a7b1befffb7df` |

This self-referential handoff is excluded from its own table. Its exact SHA-256 is supplied in the
owner report.
