# W9-A5M: bounded CSP style-policy parser and matcher

- Task: W9-A5M
- Owner: browser-engine policy prerequisite for the W9-A3P external-author-CSS slice
- Status: **IMPLEMENTED — HOSTILE REVIEW GO; CANONICAL FULL MATRIX PASS**
- Exact live and canonical verification base: `1d7c017a13b43d5103cc93c41fbeed538e2078fd`
- Firefox baseline: ESR153 commit `c19b7e89270787889495688244ec6ee8e79288a1`
- Product target: Linux x86-64 at normal desktop viewports

## Outcome

`StylePolicySet` is a pure, bounded CSP subset parser and matcher for document bases and author
styles. It consumes only the separate enforcing and report-only CSP field values already sealed in
`CapturedDocumentResponseMetadata`. The result remains bound to that envelope's exact initial
`DocumentVersion` and cloned `NavigationCommitMetadata`; the latter is validated as one canonical,
credential-free HTTP(S) final commitment before its origin can define `'self'` or scheme-less host
sources.

This object has no network client, request builder, cookie state, referrer state, DOM handle,
pipeline callback, renderer, report sender, or logging capability. No existing engine path calls
it. W9-A5M therefore changes no fetch, style application, layout, or pixel and does not yet enforce
CSP in the product.

## Exact source scope

- `browser/wild_buzzard_engine/src/style_policy.rs` (new)
- `browser/wild_buzzard_engine/src/lib.rs`
- `browser/wild_buzzard_engine/tests/style_policy.rs` (new)
- `browser/wild_buzzard_engine/README.md`
- `docs/handoffs/W9-A5M-csp-style-policy.md` (new)

There is no manifest, lockfile, pipeline, dynamic-document, navigation, network, DOM, layout,
adapter, JavaScript, Firefox-reference, program-status, root-workspace, or generated-source edit.
No source is staged or committed by this task owner.

## Parsing contract

Captured field values are never concatenated. Each exact field is independently tokenized as a
comma-separated serialized policy list, matching Firefox's `CSP_AppendCSPFromHeader` use of
`nsCharSeparatedTokenizer`: leading and interior empty members are yielded and charged to the
custom inspected-member bound, while a trailing empty member is omitted. Firefox's parser returns
null for a member with zero directives and `nsCSPContext` does not append it; this matcher likewise
does not retain such a member as a policy record. Header-list trimming uses only TAB/LF/CR/SP; the
inner policy tokenizer separately recognizes HTML form feed as whitespace. This distinction has a
focused trailing-form-feed regression proving that a form-feed tail is inspected but not retained.

Within one policy, nonempty semicolon directives consume the work/count budgets. Directive names
are ASCII-case-insensitive. A directive containing bytes outside the serialized CSP grammar is
ignored. Unknown directives are ignored after bounded inspection. The first occurrence of one of
the five relevant directives wins and every later case-variant duplicate is ignored, even when it
would be stricter:

- `base-uri`;
- `style-src-elem`;
- `style-src`;
- `style-src-attr`; and
- `default-src`.

A member containing a directive recognized by pinned ESR153 but outside those five retains one
neutral style-matcher record, so (for example) `script-src 'none'` cannot accidentally restrict a
style operation. Unknown-only and pinned-unsupported `reflected-xss` members retain no record.
Specialized value validation for non-style directives remains outside this parser. Consequently,
`enforcing_policy_count()` and `report_only_policy_count()` are explicitly retained style-matcher
record counts, not a claim of full Firefox `GetPolicyCount` parity;
`inspected_policy_member_count()` is the separately named aggregate DoS-accounting count.

Enforcing input is parsed first. Any enforcing validation, bound, counter, or allocation failure is
returned and no policy set exists. Report-only input is then parsed in an independent transaction
whose aggregate member and source-expression counters are seeded with the exact accepted enforcing
usage. A report-only failure discards every report-only policy, unsupported-source record,
duplicate count, inspected-member count, and source-expression count. The exact enforcing result
remains usable, `report_only_policy_count()` and report-only would-block counts are zero, and
`report_only_parse_failure()` exposes one copyable redacted `StylePolicyError`. Thus callers never
have to choose between allowing past a failed enforcing parse and accidentally enforcing a failed
Report-Only header. On successful report parsing, the aggregate accessors include both committed
transactions.

An empty relevant list, a list containing only `'none'`, or a list containing only nonmatching
expressions denies. When another expression is present, `'none'` is ignored. Unsupported relevant
expressions are retained only as `UnsupportedStyleSourceKind` plus exact token length and remain
nonmatching; neither their bytes nor normalized host/path text are retained in public evidence.

## Matching and fallback contract

The directive chain is selected independently in each policy:

| Operation | Directive chain |
| --- | --- |
| Candidate `base[href]` | `base-uri` only; no fallback |
| External stylesheet | `style-src-elem` → `style-src` → `default-src` |
| Inline `style` element | `style-src-elem` → `style-src` → `default-src` |
| Inline style attribute | `style-src-attr` → `style-src` → `default-src` |

Every enforcing policy must admit an operation. The decision records the exact number of enforcing
policies which block. Report-only policies execute the identical matching algorithm but can only
increment a would-block count; they never change `is_allowed()`.

The admitted source subset is:

- `'none'`, `'self'`, and `*`;
- `http:` and `https:` scheme sources;
- scheme-less or explicit HTTP(S) host sources;
- exact domains and IPv4 without cross-kind matching;
- `*.example` strict subdomains, which never include the bare suffix;
- default, exact, leading-zero numeric, and wildcard ports;
- valid base64/base64url nonce sources; and
- `'unsafe-inline'`.

Scheme-less hosts use the protected document's scheme. CSP's asymmetric secure upgrade is
preserved: an HTTP source can match HTTPS, while HTTPS never matches HTTP. After that independent
scheme check, pinned Firefox `permitsPort` treats any enforcement port 80 as matching resource port
443; the Rust helper therefore also admits explicit `https://example:80` against canonical
`https://example/`. Exact non-upgrade controls cover an HTTP candidate and HTTPS port 444. Host
spelling is normalized through the existing `GeneralWebTarget` URL boundary without DNS or
public-suffix inference. Short numeric spellings such as `127.1` are not promoted into a permissive
match for canonical `127.0.0.1`. Candidate URLs themselves must be the exact WHATWG serialization
returned by `GeneralWebTarget`; credentials, unsupported schemes, and noncanonical spelling fail
with value-redacting typed errors.

Bracketed IPv6 source expressions are classified as privacy-safe malformed/nonmatching evidence,
so they cannot grant a valid candidate IPv6 URL. This follows pinned ESR153
`nsCSPParser::host()`, whose admitted host-label characters exclude `[`. IPv4 and domain controls
prove that rejecting the IPv6 source does not broaden another source kind.

One standards-forward choice deliberately differs from pinned Firefox and is not a parity claim:

- Rust parses a numeric source port into `u16`, so leading zeros are removed before comparison.
  Pinned Firefox preserves the source-port spelling and its metadata marks
  `base-uri-allow-leading-zero-port.sub.html` expected `FAIL`.

This choice has a direct regression and must be reassessed against later standards/browser evidence
rather than attributed to the ESR baseline.

A matching nonce admits an external link stylesheet or an inline style element for that exact
policy. It never admits a style attribute. Nonce comparison is case-sensitive and the stored nonce
has no `Debug` implementation. The presence of any valid nonce or syntactically valid
`sha256`/`sha384`/`sha512` source disables `'unsafe-inline'`. Hash-content evaluation is not part of
this gate, so a valid hash remains typed and nonmatching; malformed hash-like tokens neither match
nor gain the unsafe-inline-invalidating effect. A candidate nonce above the 1,024-byte retained
nonce cap cannot match and is treated as absent rather than producing an early error; URL matching,
every enforcing policy, and every available report-only policy still run. The returned decision
exposes only `candidate_nonce_ignored_over_limit()`. Restrictive host paths other than `/` likewise
remain typed and nonmatching rather than being approximated as a broader host grant.

## Bounds and failure behavior

| Resource | Hard bound |
| --- | ---: |
| Inspected comma members across enforcing plus report-only fields | 16 |
| Nonempty directives in one member | 128 |
| Relevant source expressions across all policies | 512 |
| One serialized member | 16 KiB |
| One relevant source token | 1,024 bytes |
| Charged work in one member | 17 Ki units |
| Candidate nonce matching eligibility | 1,024 bytes |

Member, directive, expression, byte, and work excess is a typed parser failure; input is never
truncated. Enforcing failure is returned. Report-only failure is rolled back and retained as a
redacted diagnostic-unavailable status, never as an enforcing decision or partial would-block set.
The member cap deliberately charges nontrailing empty members even though they create no retained
record. An over-limit candidate nonce is simply ineligible to match and sets a privacy-safe decision
bit. All owned vectors and nonce/host `String` values reserve fallibly before retention and perform
no post-reserve boxing conversion. Every count, byte-capacity, work, and would-block accumulation
uses checked arithmetic. Errors contain
only stable input/limit/allocation categories and counts. `StylePolicySet` has a custom redacted
`Debug`; raw CSP, nonce values, the final URL, source host/path text, and candidate URLs never enter
`Debug`, `Display`, or errors.

Each parser bound has an exact-edge/next-unit regression; the nonce matching cap has exact-edge and
over-limit nonmatching regressions. The work bound is
reachable through the real parser: the exact input combines a maximum-size admitted source token
with bounded leading empty directives so its charged member bytes, one directive, and token scan
equal 17 Ki units; one additional semicolon fails `PolicyWork` while remaining under the 16 KiB
policy-byte bound.

## Firefox ESR153 and WPT evidence

The ignored Firefox checkout was read only and remained a reference, never a source or build input.
Focused paths inspected:

- `dom/security/nsCSPUtils.cpp`: `CSP_AppendCSPFromHeader`, `permitsScheme`, `permitsPort`,
  `nsCSPHostSrc::permits`, nonce pre-request admission, inline behavior, policy fallback, and
  enforcing/report-only iteration;
- `dom/security/nsCSPContext.cpp`: append-only-on-non-null parsed-policy behavior;
- `dom/security/nsCSPParser.cpp`: zero-directive null return, nonce/hash grammar, source-list
  parsing, first-duplicate behavior, source scheme injection, host grammar, invalid directive
  handling, and style fallback setup;
- `dom/security/nsCSPUtils.h`: the pinned recognized-directive spelling table;
- `dom/security/PolicyTokenizer.cpp` and `xpcom/ds/nsCharSeparatedTokenizer.h`: semicolon and
  comma-list/empty-member behavior;
- `dom/security/test/gtest/TestCSPParser.cpp`: casing, malformed inputs, `'none'`, wildcard, scheme,
  host, path, and port parsing; and
- `dom/security/test/csp/test_base-uri.html`, `test_ignore_unsafe_inline.html`,
  `test_nonce_source.html`, and `file_nonce_source.html`.

Focused WPT inputs inspected under `testing/web-platform/tests/content-security-policy/`:

- `generic/duplicate-directive.sub.html`;
- `style-src-attr-elem/` fallback and separation cases;
- `style-src/` nonce, inline, stylesheet, hash, and multiple-policy cases; and
- `base-uri/base-uri-{allow,deny,allow-leading-zero-port}.sub.html` plus the encoded-host denial.

Pinned metadata at
`testing/web-platform/meta/content-security-policy/base-uri/base-uri-allow-leading-zero-port.sub.html.ini`
was also inspected; it records Firefox's leading-zero-port case as expected `FAIL`.

## Deterministic evidence

Final canonical evidence:

- all 19 internal parser/matcher tests passed after the transactional, nonce, one-quote, IPv6, and
  exact-bound test-structure corrections;
- all 9 style-policy integration tests passed, including the transactional captured-metadata test
  and disconnected 1366×768 and 1920×1080 frame proof;
- all policy lists/fields, duplicate/case/whitespace/malformed/unknown inputs, fallback chains,
  retained-versus-inspected accounting, transactional report-only member/source/work/injected
  allocation failure, intersection/report-only counts, schemes/hosts/ports/wildcards/IP kinds,
  one-byte and empty quoted sources, bounded candidate nonces, link and element nonce distinction,
  unsafe-inline interaction, redaction, owned-string independence, and actual limit edges are
  covered; and
- the complete engine workspace passed 39 library tests plus 71 integration tests: 110 passed,
  zero failed, and one intentionally ignored opt-in public-network smoke.

The desktop regression loads identical HTML with and without `style-src 'none'`, constructs and
evaluates the disconnected policy object, proves it would block the current inline element, and
still requires byte-identical 1366×768 and 1920×1080 RGBA frames. This is structural/no-behavior
evidence, not CSP enforcement evidence.

## Verification and retained artifacts

The final matrix ran in a detached clean worktree at exact base
`1d7c017a13b43d5103cc93c41fbeed538e2078fd` with only the five authorized paths overlaid from the
live tree. Every Cargo command used the task-owned Python environment and the sole reusable target:

```sh
PYTHON3=/home/user/Documents/wildbuzzardbuilds/w9-a5m-csp-style-policy/python/bin/python
CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w9-a5m-csp-style-policy/target
```

Toolchain: `rustc 1.96.0 (ac68faa20 2026-05-25)`, LLVM 22.1.2,
`cargo 1.96.0 (30a34c682 2026-05-25)`, Python 3.12.11, target
`x86_64-unknown-linux-gnu`.

Exact final commands, all successful:

```sh
rustfmt --edition 2024 --check \
  browser/wild_buzzard_engine/src/style_policy.rs \
  browser/wild_buzzard_engine/src/lib.rs \
  browser/wild_buzzard_engine/tests/style_policy.rs
git diff --check -- \
  browser/wild_buzzard_engine/README.md \
  browser/wild_buzzard_engine/src/lib.rs \
  browser/wild_buzzard_engine/src/style_policy.rs \
  browser/wild_buzzard_engine/tests/style_policy.rs \
  docs/handoffs/W9-A5M-csp-style-policy.md
cargo check --manifest-path browser/wild_buzzard_engine/Cargo.toml \
  --workspace --all-targets --locked --target x86_64-unknown-linux-gnu
cargo clippy --manifest-path browser/wild_buzzard_engine/Cargo.toml \
  --workspace --all-targets --locked --target x86_64-unknown-linux-gnu --no-deps -- \
  -D warnings -W clippy::all -W clippy::pedantic
cargo test --manifest-path browser/wild_buzzard_engine/Cargo.toml \
  --workspace --locked --target x86_64-unknown-linux-gnu -- --test-threads=1
cargo build --manifest-path browser/wild_buzzard_engine/Cargo.toml \
  --workspace --release --locked --target x86_64-unknown-linux-gnu
RUSTDOCFLAGS='-D warnings' cargo doc \
  --manifest-path browser/wild_buzzard_engine/Cargo.toml \
  --workspace --no-deps --locked --target x86_64-unknown-linux-gnu
```

The first strict-Clippy attempt identified only a 131-line internal bounds test. It was split into
four focused tests without a lint allowance or behavior change, and the complete matrix above was
then restarted from the beginning on the same target. The release build emitted one pre-existing
dependency warning for WebRender's unread `RenderTaskGraph::frame_id`; strict `--no-deps` Clippy
and warning-denied no-dependency rustdoc were clean.

Only these current integration-review artifacts are retained:

| Purpose | Exact path | Final bytes/hash |
| --- | --- | ---: |
| Reusable Cargo target | `/home/user/Documents/wildbuzzardbuilds/w9-a5m-csp-style-policy/target` | 8,103,185,995 bytes |
| Task-local Stylo Python | `/home/user/Documents/wildbuzzardbuilds/w9-a5m-csp-style-policy/python` | 11,992,677 bytes |

No external log, screenshot, fixture, patch, package, temporary profile, repository-local build
artifact, or canonical source checkout is retained. The target is kept only for orchestrator
integration; the task root can be removed after integration.

## Deliberate remaining work

W9-A5M does not discover a `base` or stylesheet element, resolve a relative URL, apply header/meta
policy timing, fetch or redirect, enforce mixed content, compute/send reports, match source paths
or inline hashes, parse `@import`, inspect MIME/encoding, freeze a style set, reject dynamic
resource mutations, or feed Stylo. It also does not implement CSP outside the five relevant
directives. Those remain explicit W9-A3P integration and later CSP-conformance gates.

The later loader must not broaden a typed unsupported source, bypass the matcher, concatenate CSP
fields, treat report-only as permission, or fetch before enforcing every applicable policy. It must
add deterministic redirect/mixed-content/MIME/cancellation/deadline tests and rerun the same two
desktop viewports before product activation.

## Frozen source hashes

| Path | SHA-256 |
| --- | --- |
| `browser/wild_buzzard_engine/README.md` | `b35b8ef5f01074b92c7d6d19c3782787c43443b06c58613dd39467b576610eed` |
| `browser/wild_buzzard_engine/src/lib.rs` | `d0cf4c58449c51e2dd3be953fea7469fddca29f55cf535cfcea741f9edd6d806` |
| `browser/wild_buzzard_engine/src/style_policy.rs` | `220fe53d6c235a263092fbd5c605f77b23fcd0fe6b9f5002c64658903c50c5e6` |
| `browser/wild_buzzard_engine/tests/style_policy.rs` | `52b19bf66e630b30d5bc373f031ce0b1ae8ef77ca8cdccc26d787b8c007471f2` |

This self-referential handoff is excluded from its own table; its final SHA-256 is supplied in the
owner message.
