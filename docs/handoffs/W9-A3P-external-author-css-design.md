# W9-A3P: document-owned external author CSS design

- Status: **DESIGN READY — conditional GO for implementation**
- Design base: `bf26af8375085faa5012b4b04806ae5d44b6c7a7`
- Firefox reference: ESR153 commit `c19b7e89270787889495688244ec6ee8e79288a1`
- Product target: Linux x86-64 at normal desktop viewports

## Decision

The first external-stylesheet gate belongs in the browser engine coordinator, above networking,
DOM, and Stylo. The engine discovers and policy-checks HTML `link` elements, performs bounded
fetches, freezes decoded sheets with the live document, and passes already-fetched records to the
Stylo adapter. Stylo must never own DNS, TLS, redirects, cookies, CSP, cancellation, or deadlines.

The gate is a static first-frame barrier. It is not parser-preload, CSSOM, load-event, dynamic-link,
preferred-style-set, or complete Firefox parity.

```text
HTML response and bounded policy headers
        |
        v
document_policy.rs
(final URL, CSP, referrer policy, first base)
        |
        v
style_resources.rs
(DOM-order discovery, admission, fetch, redirects, MIME, UTF-8)
        |
        v
FrozenDocumentStyleSet
(owned decoded records; no transport handle)
        |
        v
wild_buzzard_stylo_adapter
(sheet-specific URL data, parse, source-order cascade)
        |
        v
layout -> renderer
```

## First admitted vertical

Admit only document-owned HTML `link` elements with:

- an ASCII-case-insensitive `rel` token list containing `stylesheet`;
- a nonempty `href` resolving to HTTP(S);
- absent/empty `type`, or MIME essence `text/css`;
- no `disabled` state;
- absent `crossorigin`;
- absent/empty `integrity`; and
- no nonempty title and no `alternate stylesheet` relation.

Titled or alternate sheets are diagnosed but not fetched until preferred-set selection exists.
Stylo parses `media`; the first gate may conservatively wait for every admitted sheet before the
initial render. Cross-origin, absent-`crossorigin` sheets use no-CORS/same-origin-credentials
semantics and are retained as origin-dirty. The current credential capability is explicitly empty
and nonpersistent; no cookies, authentication, or cache state may be fabricated.

Only UTF-8 and UTF-8 BOM are admitted. Top-level `@import` must issue no request; it becomes a
recoverable diagnostic while independently valid rules continue to apply. CSS `url()` values get
the correct sheet base URL but trigger no image or font fetch in this gate.

## Prerequisites

1. `FetchedDocument` must retain bounded parsed policy inputs currently discarded with response
   headers: enforcing/report-only CSP, Referrer-Policy, Content-Type/charset, and relevant
   Set-Cookie presence.
2. `LiveDocumentPage` must retain an immutable style-resource set so rerender neither refetches nor
   loses sheets.
3. Inline CSS must use the final document base, and every external sheet must use its own final
   response URL. The adapter's synthetic invalid CSS URL must not survive this gate.
4. Header/meta CSP must filter external sheets, inline `style` elements, and inline style
   attributes. Adding external fetch while leaving existing inline CSS unrestricted is a NO-GO.
5. The adapter's current fatal `@import` behavior must become a bounded non-fetching diagnostic for
   this gate.

## Proposed owned contracts

The engine owns policy and resource records conceptually equivalent to:

```rust
struct DocumentPolicy {
    final_document_url: CanonicalUrl,
    document_base_url: CanonicalUrl,
    referrer_policy: ReferrerPolicy,
    enforcing_csp: Box<[ContentSecurityPolicy]>,
    report_only_csp: Box<[ContentSecurityPolicy]>,
    credentials: CredentialCapability, // EmptyNoPersistence only
}

struct FrozenDocumentStyleSet {
    document_id: DocumentId,
    document_base_url: Box<str>,
    sheets: Box<[FrozenAuthorSheet]>,
    inline_style_attribute_admission: Box<[NodeId]>,
    diagnostics: Box<[StyleResourceDiagnostic]>,
    fingerprint: StyleEnvironmentFingerprint,
}

enum FrozenAuthorSheet {
    Inline { owner: NodeId, media: Box<str> },
    External(LoadedExternalSheet),
}

struct LoadedExternalSheet {
    owner: NodeId,
    requested_url: Box<str>,
    final_url: Box<str>,
    media: Box<str>,
    css: Arc<str>,
    redirect_count: u8,
    origin_clean: bool,
    child_referrer_policy: ReferrerPolicy,
}
```

The adapter receives only a borrowed, exact-version inventory of inline owners and pre-fetched
external records. Because networking and imported Stylo currently use distinct `url` crate
instances, canonical bounded URL strings cross this boundary. The adapter reparses and verifies
canonical serialization using its own exact URL type before creating `UrlExtraData`.

Adapter validation must reject a wrong snapshot version, foreign or duplicate owner, non-DOM
order, wrong owner element kind, noncanonical/oversized URL, excess sheets or bytes, and an invalid
inline-style admission inventory before style parsing begins.

## URL, base, and security rules

- The fallback base is the final committed document URL, not the original requested URL.
- The first `base[href]` in tree order wins. If it is invalid or CSP-blocked, the fallback remains;
  later base elements do not become candidates.
- Resolve that first base against the fallback base, never against another base.
- Reject `data:` and `javascript:` bases and fetch only HTTP(S) stylesheet URLs.
- Enforce CSP `base-uri` explicitly; it has no `default-src` fallback.
- External elements use `style-src-elem`, falling back to `style-src`, then `default-src`.
- Multiple enforcing policies intersect. Report-only policies diagnose but do not block.
- The bounded CSP parser must handle `'none'`, `'self'`, `*`, HTTP(S) scheme/host sources,
  wildcard subdomains, ports, nonces, and `'unsafe-inline'`. Unsupported relevant expressions are
  nonmatching, never permissive.
- Apply the final enforcing header/meta set document-wide in this post-parse gate. This is stricter
  than exact parser-time meta activation and must remain documented.
- An HTTPS document must never fetch HTTP CSS, including downgrade redirects. Check every hop
  before opening the next connection.
- Link `referrerpolicy` overrides document policy; otherwise use the final recognized policy,
  defaulting to `strict-origin-when-cross-origin`. Recompute the referrer at every redirect.

## Fetch and response rules

Use an iterative manual GET walker with one cancellation token and one absolute navigation deadline
across all sequential sheet fetches and redirect hops. Permit at most ten redirects. Reject before
following missing/multiple/non-UTF-8 `Location`, credentials in URLs, unsupported schemes, loops,
oversized URLs, CSP failures, or mixed-content targets.

The request destination is style, mode is no-CORS, and credentials mode is same-origin. Send a
stylesheet `Accept` header and the computed `Referer`. Do not invent `Sec-Fetch-Site` without a
trustworthy site/public-suffix implementation.

Accept only a 2xx final response with exactly one unambiguous Content-Type field whose MIME essence
is `text/css`. Do not MIME-sniff. Stream through the per-sheet limit rather than using an unbounded
read-to-end helper. The final response URL—not the request URL or `Content-Location`—is the sheet
base.

Preserve encoded bytes for future SRI support. A UTF-8 BOM wins and is stripped. Without a BOM,
accept an absent/UTF-8 HTTP charset and a valid leading UTF-8 `@charset`; reject unsupported labels
and invalid UTF-8. Full Firefox decoding precedence, including legacy encodings and link/document
fallback, is later work.

Individual policy, network, status, MIME, or decode failures block and diagnose only that sheet.
Cancellation, deadline, allocation/invariant failure, and aggregate resource exhaustion abort the
navigation without publishing a partial frame.

## Limits and diagnostics

Initial limits:

- 64 total author sheets, shared with inline sheets;
- 512 KiB encoded bytes per external sheet;
- 1 MiB aggregate decoded inline plus external CSS;
- 16 KiB per canonical sheet URL;
- 10 redirects per root request;
- 16 CSP policies, 128 directives per policy, and 512 relevant source expressions total;
- existing 8,192-selector, 65,536-declaration, 50,000,000-work, and 256-diagnostic caps;
- one sequential fetch; and
- zero import depth and zero import fetches.

Add `PipelineStage::StyleResourceLoad` and a typed fatal style-resource error. Retained nonfatal
diagnostics must distinguish disabled/type/title/alternate/invalid-href/scheme/crossorigin/
integrity, CSP/mixed-content, redirect/network/status, ambiguous or wrong MIME, encoding/UTF-8,
per-sheet size, and unsupported import outcomes.

Pipeline evidence records candidates, requests, redirects, applied sheets, retained bytes, blocked
sheets, nonmatching media sheets, and origin-dirty sheets.

## Live-document rules

`LiveDocumentPage` owns `FrozenDocumentStyleSet`. Exact-version rerender reuses the same decoded
bytes and issues zero requests. Until dynamic stylesheet lifecycle exists, mutations affecting
base, CSP/referrer meta, link admission attributes, style-element content/attributes, or inline
style must be rejected before commit with a typed unsupported-resource-mutation result. Unrelated
DOM mutations may rebind the verified immutable set to the new revision. A superseded navigation
generation publishes neither commit, frame, nor retained page.

## Ownership and implementation order

Agent 3 owns new engine policy/resource modules plus the adapter input, validation, per-sheet base,
tests, and adapter documentation. Agent 6 owns pipeline/dynamic/live-page integration, engine
errors/evidence, navigation tests, and engine documentation. Agent 5 reviews request projection and
bounded streaming. The orchestrator owns any manifest/lock change, this handoff, parity records,
integrated review, commits, and cleanup.

Implementation order:

1. Add adapter records, validation, per-sheet URL data, and deterministic cascade tests with no
   networking capability.
2. Retain bounded response-policy metadata.
3. Implement base/CSP/referrer parsing and DOM-order discovery.
4. Add the bounded redirect/MIME/UTF-8 fetch path.
5. Freeze the set into the live page and reject stale style-affecting mutation.
6. Add evidence, local HTTP/TLS integration tests, ignored public probes, and two desktop-size
   externally-styled fixtures.
7. Independently review before enabling the path by default.

## Acceptance

Required deterministic coverage includes base resolution and final sheet URLs; inline/external DOM
order; link admission; same/cross-origin no-CORS use; exact referrers; CSP fallback, intersection,
nonce, report-only, and inline blocking; mixed-content direct/redirect rejection; every redirect
failure; strict status/MIME/encoding handling; zero-request `@import`; all limits; stalled-request
cancellation/deadline/supersession; retained rerender with zero refetch; stale resource-affecting
mutation rejection; and a visible external-CSS-only fixture at 1366x768 and 1920x1080.

NO-GO if the implementation injects fetched text as a synthetic inline style, keeps the synthetic
base URL, lets Stylo fetch, omits CSP/redirect/MIME/security-attribute checks, treats invalid bytes
as UTF-8, fetches imports inside a parser callback, or claims CSSOM/load-event/cookie/cache parity.

## Expected public-site impact and cleanup

A transient 2026-08-10 inventory found eight DuckDuckGo sheets (~77 KiB), two Google sheets
(~8 KiB), and five YouTube sheets (~384 KiB). All sampled responses were 200 `text/css`, below the
proposed limits, with no redirects or imports. The gate therefore materially advances all three,
but JavaScript/events, CSS breadth, images/fonts, forms/input, persistence, layout gaps, and media
remain separate blockers.

The design audit edited no live file, ran no build, and retained no external artifact. Its temporary
investigation tree had already been removed; retained size is zero bytes.
