# W9-A3R: contained parser/Brimstone/DOM coordinator

- Owner: main orchestrator, browser integration lane
- Working-tree base: `8eefa01f`
- Status: owner gates passed; independent review pending
- Product admission: compile-time disabled

## Outcome

`browser/wild_buzzard_script` is a first-party Rust coordinator crate for one bounded vertical
proof. With the opt-in `contained_inline_classic` feature, it parses numeric-loopback HTML and
executes admitted inline classic scripts against the exact live Rust DOM. The default build has no
runtime dependencies and exposes `PRODUCT_SCRIPT_ADMISSION_ENABLED = false`.

The gate is deliberately unavailable to HTTPS/general-web navigation. It proves the generic
parser/realm/DOM lifecycle before browser-engine and event-loop admission; it is not a YouTube or
normal-site execution path.

## One-document lifecycle

For one request the coordinator owns:

- one Brimstone `OwnedContext` and initial realm;
- one `ScriptDocument` and `RootedDomTask`;
- one cumulative parser-document `ClassicScriptLimits` budget;
- one parser arena and monotone lease sequence;
- one paired browser cancellation and Brimstone interrupt authority; and
- bounded final evidence for at most 64 script candidates.

HTML is fed in UTF-8-safe chunks of at most 16 KiB so cancellation and the absolute deadline are
observed between parser work units. At each explicit parser-inserted script boundary it:

1. restores the exact arena through the one-use lease;
2. performs and validates the sealed pre-script checkpoint;
3. reads inline text from the now-current live DOM;
4. classifies with the immutable start-tag snapshot;
5. executes one admitted classic or accounts one intentionally skipped candidate;
6. requires the post checkpoint for every admitted classic, including recoverable throws and
   parse/analyze/compile failures;
7. records the exact completed document version; and
8. returns the arena to the parser once.

After parser completion it requires the sealed final checkpoint and obtains a
`PublishedParserDocument` before producing a snapshot or report.

## Current classification

This contained gate admits only inline classic scripts with an absent/empty `type` or
ASCII-case-insensitive `text/javascript`. It accounts but does not execute:

- scripts with `src` present, including an empty value;
- `nomodule` scripts;
- modules and import maps; and
- unsupported MIME/type values.

The classification uses frozen start-tag state even if the live element's attributes are changed
later. Inline source uses the live post-pre-checkpoint text. No hostname, page, site, or YouTube
special case exists.

## Bounds and failure behavior

- Maximum candidates: 64.
- Maximum cumulative inline source: 4 MiB.
- Maximum inspected `type` value: 4 KiB.
- Brimstone heap: fixed 64 MiB for this proof.
- Brimstone document wall time: at most 30 seconds and never later than the caller's absolute
  deadline.
- Parser chunk: at most 16 KiB.

Cancellation requests both network/browser-task cancellation and the Brimstone interrupt flag.
Typed parser, host, runtime, checkpoint, allocation, source, attribute, cancellation, deadline,
and invariant errors retire both the host task and document. Terminal owners do not escape. The
product-disabled JIT must report no native entries in every accepted script/checkpoint phase.

An EOF-unclosed script remains DOM text, contributes an HTML diagnostic, and produces no boundary
or execution.

## Verification

All commands used the Data-drive Podman wrapper, network-disabled containers, Rust 1.95, and
Data-drive Cargo/target/temp directories.

- Default feature build: passed with no tests or runtime dependency activation.
- Contained debug: 9 tests passed.
- Contained release: 9 tests passed.
- Strict all-target Clippy: passed with `-D warnings`.
- Warning-denied rustdoc: passed.
- Parser dependency: 9 tokenizer + 17 tree tests passed in debug and release.
- Bridge dependency: 1 library + 10 integration tests passed in debug and release.

Coordinator regressions cover one realm across parser pauses, live microtask DOM mutation,
recoverable throw/post-checkpoint ordering, exact classification, start-tag mutation resistance,
malformed EOF nonexecution, future-markup invisibility, cumulative candidate termination, paired
cancellation of an infinite loop, and rejection of a general-web URL before context/DOM creation.

Evidence targets are under:

```text
/run/media/user/Data/Repositories/wildbuzzardbuilds/w9-a3r-script-loop-c2/
```

## Required next gate

The browser engine still calls its static `HtmlParser` path and does not consume this coordinator.
The next vertical slice must bind one paired cancellation owner to fetch plus script execution,
render the coordinator's exact published snapshot through Stylo/layout/WebRender, retain the
context/host for event-loop work, and publish the resulting frame under the navigation identity.

General-web script admission remains blocked on wider Brimstone lifetime/resource/conformance
work, external scripts/modules, WebIDL globals, principals/CSP, event scheduling, and independent
review. This handoff makes no live-app, YouTube, or Firefox-parity claim.
