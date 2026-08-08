# W1-A3 DOM, HTML, and static-layout handoff

- Task: W1-A3 safe DOM ownership, incremental HTML parsing, and immutable static-layout nucleus
- Owner: Agent 3 — Web platform, DOM, Stylo, and layout; integrated and reviewed by the main orchestrator
- Status: Complete for the Wave 1 DOM/HTML/basic-layout contract; this is not HTML, DOM, CSS, or layout parity
- Firefox commit and source paths: ESR153 `c19b7e89270787889495688244ec6ee8e79288a1`; exact DOM, parser, formatting, app-unit, and text-layout paths are recorded in `dom/README.md`, `parser/README.md`, and `layout/README.md`
- Firefox test paths: focused WPT/html5lib and layout reftest paths are recorded in the three component READMEs; every cited implementation path and ten cited history revisions were independently verified
- Wild Buzzard paths changed: `dom/`, `parser/`, `layout/`, and root workspace manifests
- Contract added or changed: stable document-scoped `NodeId`, atomic DOM mutations, owned revisioned `DocumentSnapshot`, incremental `HtmlParser` with source-positioned structured errors, `StyleResolver`, `TextMeasurer`, immutable box/fragment output, and checked `LayoutLimits`
- Tests run and results: owner and orchestrator gates passed on `x86_64-unknown-linux-gnu`; 39 tests passed, 0 failed/ignored; root-integrated package formatting, check, strict Clippy, locked tests, release build, and rustdoc all passed using external targets
- Parity evidence: initial parse-to-DOM-to-layout behavior, deterministic mutation/snapshot invariants, exact supported HTML/CSS whitespace classification, 1,024-level iterative DOM traversal, and structured layout-depth failure only
- Known behavioral differences: the component READMEs enumerate the missing full tokenizer/tree builder, WebIDL/events/shadow DOM, Stylo cascade, formatting contexts, shaping, bidi, scrolling, hit testing, accessibility geometry, and painting behavior
- Unsafe or FFI introduced: None; all three crates forbid unsafe code and introduce no native boundary
- Licenses and provenance: MPL-2.0 first-party implementations informed by the pinned ESR source, tests, standards fixtures, and history; the crates have no third-party runtime dependencies
- Provider or network implications: None; these crates perform no I/O and contain no service or telemetry endpoint
- Blocked on: No blocker to continued implementation; the first rendered slice still requires decoded network input, a native Stylo platform adapter, the graphics font boundary, and a WebRender display list/frame
- Recommended next action: connect Agent 5's bounded loopback response bytes to incremental parsing, adapt the admitted Stylo crates, and hand immutable layout fragments to Agent 4's renderer contract
