# W9-A5L final-response policy envelope

Status: READY REREVIEW (not staged or committed)

Implementation started at `06b227bccfce04286ed489c81b7dd12afd114b43`.
Disjoint collapsed-nowrap and public-probe documentation gates landed while
this task was active; the frozen live integration base is
`832636c1f82dba95ab161948d13421149aa04878`.

## Outcome

The browser engine now captures the bounded policy-relevant inputs of the
exact final successful top-level response and moves them with the owning
`LiveDocumentPage`. The envelope is bound to both the initial parsed
`DocumentVersion` and the exact `NavigationCommitMetadata`. It remains bound to
that original response when DOM mutations advance the live revision and when
the navigation executor moves the page between its active slot and private
per-context storage.

This gate does not fetch or apply an external stylesheet. It does not parse or
enforce CSP, choose a request referrer, use Content-Type for MIME/encoding
admission, or mutate a cookie jar. The public type and accessor consistently use
`Captured`/`Metadata`/`Input` language and explicitly disclaim admission and
enforcement.

## Exact source scope

- `browser/wild_buzzard_engine/src/document_policy.rs` (new)
- `browser/wild_buzzard_engine/src/pipeline.rs`
- `browser/wild_buzzard_engine/src/error.rs`
- `browser/wild_buzzard_engine/src/lib.rs`
- `browser/wild_buzzard_engine/src/dynamic.rs`
- `browser/wild_buzzard_engine/src/navigation.rs`
- `browser/wild_buzzard_engine/tests/document_policy.rs` (new)
- `browser/wild_buzzard_engine/README.md`
- `docs/handoffs/W9-A5L-document-policy-envelope.md` (new)

The `navigation.rs` production diff adds the exhaustive `map_pipeline_error`
arms required by the new typed error: impossible binding mismatch maps to
`Internal/Document`; byte/count/work/allocation exhaustion maps to
`ResourceLimit/Fetch`. It also changes the private final-URL storage in
`NavigationCommitMetadata` from `Box<str>` to `Arc<str>` so derived policy
ownership can share the exact immutable allocation. A focused private unit test
proves pointer identity across a clone. No public navigation API, observable
value, protocol, fixture, or event changed.

Unrelated pre-existing `js/` modifications were neither read for implementation
nor edited, formatted, staged, or tested by this task.

## Captured contract

Only a final 2xx response is captured. Policy headers on admitted redirects are
discarded with each redirect response; its body is still never read as the
document. A cancellation/deadline checkpoint runs immediately after each
general-web response and before redirect or metadata processing. Numeric
loopback performs the same checkpoint before final-response capture.

The envelope contains:

- separate enforcing `Content-Security-Policy` raw-byte field values in wire
  order;
- separate `Content-Security-Policy-Report-Only` raw-byte field values in wire
  order;
- recognized Referrer Policy tokens in field/comma-token order plus the count
  of nonempty ignored inputs;
- separate Content-Type field classifications, each either a normalized typed
  media type with ordered charset parameters or a non-sensitive malformed
  reason;
- Set-Cookie presence, exact field count, and exact aggregate value bytes only.

CSP raw bytes are retained because the next security gate needs a dedicated
parser. They are never included in `Debug`. Set-Cookie values are inspected
only for checked count/byte accounting while the response lives and are never
copied into the capture object. The envelope's custom `Debug` also omits the
final URL and reports CSP counts/bytes rather than values. Errors carry stable
field/limit/count information and never peer-controlled values.

All response-derived retained vectors/strings use `try_reserve` or
`try_reserve_exact` before copying or pushing. All count/byte accumulation uses
checked arithmetic. `NavigationCommitMetadata` allocates its bounded final URL
once as an `Arc<str>` at construction. Derived clones increment that allocation's
reference count and copy only fixed-size scalar evidence, so giving both
existing pipeline evidence and the private live-page security owner the exact
commitment performs no second URL allocation.

Set-Cookie prospective field count/value bytes are checked before the equal
global 64 KiB input bound, so a 65,537-byte sole cookie value deterministically
reports the Set-Cookie family limit. Cookie and global counters commit only
after both preflights pass. CSP aggregate bytes likewise commit only after the
fallible value copy and vector push succeed.

### Hard bounds

| Input | Bound |
| --- | ---: |
| Enforcing CSP fields | 16 |
| Report-only CSP fields | 16 |
| One CSP value | 16 KiB |
| Both CSP kinds combined | 32 KiB |
| Referrer-Policy fields | 16 |
| One Referrer-Policy value | 4 KiB |
| Referrer-Policy nonempty tokens inspected | 128 |
| Recognized Referrer-Policy inputs retained | 64 |
| Content-Type fields | 8 |
| One Content-Type value | 4 KiB |
| Charset parameters per Content-Type | 16 |
| Set-Cookie fields counted | 128 |
| Set-Cookie value bytes counted | 64 KiB |
| All policy-relevant value bytes inspected | 64 KiB |

The transport's existing response-header count and byte limits remain an outer
bound. Policy limits are an additional subsystem contract and return
`PipelineError::DocumentPolicy` rather than silently truncating.

## Ownership and failure behavior

`LiveDocumentPage::new` is crate-private and now fails closed unless all three
versions agree: the mutable document, last returned initial frame, and captured
response document. The policy object has no public constructor, so safe callers
cannot bind response fields to another document or final URL. The existing
`replace_live_document` moves the whole page by value, including its private
envelope; no parallel policy registry or stale context key exists.

A hostile policy limit failure occurs before HTML body parsing and cannot
replace an already-live document. A malformed Content-Type remains visible as a
typed malformed input rather than being comma-joined, logged, or made into an
unreviewed MIME decision. Unknown Referrer-Policy tokens are counted and ignored
while the last recognized input remains inspectable; no request behavior changes.

The public worker facade intentionally does not expose raw live-page policy
metadata to UI code. Therefore the integration test can directly inspect
load/mutation/rerender retention through `StaticPageEngine`, but cannot inspect
private cross-context movement through `NavigationEngine`. Cross-context safety
is structural: the metadata is a private field of the value already moved by
the existing per-context executor path, whose full regression suite passes.

## Firefox ESR153 reference research

Reference checkout remained clean at
`c19b7e89270787889495688244ec6ee8e79288a1`.

Narrow reference paths inspected:

- `dom/base/Document.cpp` (`Document::InitCSP`) obtains enforcing and
  report-only response headers before appending policies.
- `dom/security/ReferrerInfo.cpp`
  (`ReferrerPolicyFromHeaderString`) tokenizes comma-separated inputs and keeps
  the last supported token.
- `netwerk/protocol/http/nsHttpResponseHead.cpp`
  (`ParseContentTypeValue`) maintains response Content-Type parsing state.
- `netwerk/protocol/http/nsHttpHeaderArray.cpp` preserves response header
  visitation/original-header mechanics.
- `netwerk/protocol/http/HttpBaseChannel.cpp` processes individual Set-Cookie
  values through the cookie service.
- `dom/security/test/csp/`, `dom/security/test/referrer-policy/`,
  `netwerk/test/unit_ipc/test_duplicate_headers_wrap.js`, and cookie tests under
  `netwerk/test/` provide later parity inputs.

History inspected with focused logs for `dom/security/ReferrerInfo.cpp`,
`dom/base/Document.cpp`, `dom/security/nsCSPContext.cpp`, and
`netwerk/protocol/http/nsHttpResponseHead.cpp`. This gate intentionally adopts
behavioral inputs and ownership requirements, not Gecko's C++ channel/CSP
architecture.

## Deterministic test evidence

New internal parser tests: 8 passed. The matrix executes exact-limit admission
and next-unit rejection for every exported field-count, field-byte, aggregate,
token-work, retained-input, and charset bound. It also covers checked counter
overflow.

New internal navigation allocation-sharing test: 1 passed. It proves
`Arc::ptr_eq` for the final URL across a metadata clone while all commitment
values remain exact.

New integration tests: 6 passed, covering:

- duplicate and mixed-case field names without invalid merging;
- final response/URL/redirect-count/document-version identity, with redirect
  CSP and Set-Cookie values proven absent;
- recognized/unknown Referrer-Policy ordering;
- parsed duplicate Content-Type inputs and malformed typed classification;
- CSP field-size and field-count rejection while the old live page survives;
- Set-Cookie count/bytes and sensitive-value debug redaction;
- cancellation precedence over an elapsed deadline, and deadline precedence
  over target/policy work when not cancelled;
- retention across a committed DOM mutation and exact-version rerender;
- pixel-identical visible frames with and without observed policy headers at
  both 1366×768 and 1920×1080.

Full engine workspace matrix: 82 passed, 0 failed, 1 deliberately ignored
public-network smoke. This includes the existing real multi-context navigation
worker regression.

Commands used, all with the task-local pinned generator and sole external
target:

```sh
PYTHON3=/home/user/Documents/wildbuzzardbuilds/w9-css-policy-envelope/target/python/bin/python
CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w9-css-policy-envelope/target

cargo check --manifest-path browser/wild_buzzard_engine/Cargo.toml \
  --workspace --all-targets --locked --offline \
  --target x86_64-unknown-linux-gnu

cargo clippy --manifest-path browser/wild_buzzard_engine/Cargo.toml \
  --workspace --all-targets --locked --offline \
  --target x86_64-unknown-linux-gnu --no-deps -- \
  -D warnings -W clippy::all -W clippy::pedantic

cargo test --manifest-path browser/wild_buzzard_engine/Cargo.toml \
  --workspace --locked --offline --target x86_64-unknown-linux-gnu \
  -- --test-threads=1

cargo build --manifest-path browser/wild_buzzard_engine/Cargo.toml \
  --workspace --locked --offline --release \
  --target x86_64-unknown-linux-gnu

RUSTDOCFLAGS='-D warnings' cargo doc \
  --manifest-path browser/wild_buzzard_engine/Cargo.toml \
  --workspace --locked --offline --target x86_64-unknown-linux-gnu --no-deps

cargo fmt --manifest-path browser/wild_buzzard_engine/Cargo.toml -- --check
git diff --check
```

Check, strict no-dependency Clippy, release, rustdoc-with-warnings-denied,
rustfmt, and diff gates passed. The release dependency graph emits one existing
non-fatal WebRender `frame_id` dead-code warning; the task crate is clean under
the strict no-dependency Clippy gate.

The first release attempt experienced no source failure: another lane deleted
the shared generator virtualenv during its cleanup. Per orchestrator approval,
the pinned `servo/style-build-requirements.txt` packages were then installed
from the local pip cache under this task's sole target at `target/python`, and
all Python-dependent gates were rerun with that stable path.

## Sole retained artifact

For independent review only:

`/home/user/Documents/wildbuzzardbuilds/w9-css-policy-envelope/target`

Final `du -sb` retained sizes after all gates:

- target: `8,052,964,922` bytes;
- whole task root: `8,052,964,922` bytes (the target is its sole child).

It contains Cargo debug/release/rustdoc output and the pinned build-only Python
environment. There is no worktree, screenshot, profile, corpus, standalone log,
or other task artifact. Delete the entire
`/home/user/Documents/wildbuzzardbuilds/w9-css-policy-envelope` task root after
review/integration; never recursively target the shared `wildbuzzardbuilds`
root.

## Explicitly open

- Dedicated CSP parsing, source-list matching, reporting, and enforcement.
- `<base>` processing and CSP `base-uri` admission.
- External stylesheet discovery, fetch metadata, redirect/CSP/mixed-content/
  referrer/integrity/CORS checks, MIME/encoding handling, CSS parsing, cascade,
  diagnostics, cancellation, and immutable live-page retention.
- Cookie parsing/storage/SameSite/partitioning/credential-mode behavior.
- Document encoding sniffing and authoritative Content-Type/MIME behavior.
- Referrer computation on concrete subresource/navigation requests.
- A UI-safe non-sensitive policy evidence projection, if product diagnostics
  later need one; raw CSP is deliberately not put in `PipelineEvidence`.
