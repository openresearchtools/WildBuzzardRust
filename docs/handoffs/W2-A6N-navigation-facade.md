# W2-A6N typed navigation and stale-frame facade

- Task: Put the existing synchronous static engine behind a bounded dedicated-worker command/event
  contract with monotonic navigation generations and atomic stale-frame suppression.
- Owner: Agent 6 — browser product/UI/tooling; corrected after independent NO-GO findings and
  accepted only after a separate post-fix review.
- Status: Complete for the bounded in-process facade. It is not a Rust window, tab lifecycle,
  asynchronous network stack, product navigation implementation, or Firefox UI parity result.
- Integration boundary: this gate originally wrapped W2-A6's transitional split-frame executor.
  W2-A6C subsequently connected its canonical shaped inventory to W2-A4D's one-transaction
  zero-pending-text path and now publishes that one composed frame through the unchanged facade.
- Firefox reference: ESR153 `c19b7e89270787889495688244ec6ee8e79288a1`; docshell, browser,
  widget, navigation, and browser-test behavior remain future parity reference. No Firefox
  implementation or branded UI was copied.
- Wild Buzzard paths changed: `browser/wild_buzzard_engine` only; existing synchronous
  `StaticPageEngine` compatibility remains intact.
- Contract added or changed: opaque nonzero `TopLevelContextId`, checked monotonic
  `NavigationGeneration`/`NavigationId`, bounded requests/commands/events/contexts/frame bytes,
  typed receipts/failures/stages, sequenced events, one-shot generation-tagged frame leases, and
  stable shutdown status. A factory constructs and owns the non-`Send` EGL/WebRender executor on
  its dedicated worker thread; UI-facing values expose only fixed metadata and owned RGBA8 bytes.
- Atomicity: admission failure changes no context, generation, cancellation, or queue state. A
  newer admitted generation cancels its predecessor. Publication rechecks the current generation
  under the shared lock and reserves both event identities/capacity before replacing the frame,
  lease, byte accounting, or phase. Stale completion cannot publish or remove a newer frame.
- Backpressure/lifecycle: event capacity below three is rejected so an undrained `Started` plus the
  atomic `Committed`/`FrameReady` pair always fits for one success. Cancellation and shutdown are
  priority controls. Factory, execution, shutdown, and destructor panics are contained; executor
  shutdown and explicit same-thread destruction finish before terminal status publication.
- Tests run and results: 22 locked tests passed (5 navigation unit, 15 no-sleep facade integration,
  and 2 existing real EGL/WebRender static-pipeline tests). They cover supersession, cancellation,
  command/event pressure, generation/context rejection, stale leases, aggregate frame limits,
  receiver drop, event/lease exhaustion, worker affinity, factory error/panic, executor panic,
  shutdown error/panic, destructor panic, exactly-once cleanup, repeat shutdown, and atomic
  three-event admission.
  Locked all-target check, strict Clippy, warning-denied rustdoc, release check, explicit rustfmt,
  diff, manifest/lock, artifact, unsafe, provider, and platform audits passed.
- Parity evidence: the result proves bounded generation-aware publication and safe stale-result
  suppression around the admitted synchronous engine. It does not prove document-commit semantics,
  redirects/history/session behavior, prompts, input, tabs/windows, process isolation, or WebDriver
  behavior.
- Unsafe or FFI introduced: none. The facade is std-only and executes the existing audited
  component/native boundaries through `StaticPageEngine`.
- Provider or network implications: none beyond the existing explicit numeric-loopback HTTP
  capability; no DNS, external endpoint, credential, telemetry, or unsolicited request was added.
- Known limitations: no `CloseContext` or context-slot reuse; `max_contexts` is a safe lifetime cap.
  Construction still accepts the pre-existing component-rich `StaticPageConfig` rather than a
  product-owned configuration contract. If shutdown fails and destruction panics, the terminal
  status retains the stronger panic outcome rather than both diagnostics.
- Subsequent integration: W2-A6C retains each finalized run's exact `Arc<ShapedText>`, projects
  above-baseline as `first_baseline` and below-baseline as `height - first_baseline`, calls
  `render_composed` once, and publishes the one zero-pending frame through this lease boundary.
- Recommended next action: add the Linux presentation shell without importing engine internals
  into UI code.
