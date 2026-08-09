# W2-A6 bounded static-page integration handoff

- Task: W2-A6 independently locked numeric-loopback URL-to-Stylo-to-layout-to-WebRender integration proof
- Owner: Agent 6 — product/UI integration, with typed cross-owner contracts hardened and independently reviewed by the main orchestrator
- Status: Complete for the bounded synchronous proof. W2-A6N subsequently added a typed bounded
  in-process navigation/event/lease facade around this unchanged executor. Milestone M1 remains in
  progress because page decorations and shaped text are still separate engine frames and no window
  boundary exists.
- Firefox commit and source paths: ESR153 `c19b7e89270787889495688244ec6ee8e79288a1`; component-level Firefox, WebRender, Stylo, text, and network references remain recorded in the W2-A3T, W2-A4, W2-A4B, W2-A4C, and W2-A5 handoffs
- Firefox test paths: no Firefox harness was claimed for this integration-only proof; future acceptance must map the composed fixture to relevant `layout/reftests`, WPT, navigation, and browser screenshot behavior
- Wild Buzzard paths changed: `browser/wild_buzzard_engine`; typed `DocumentVersion` propagation in `dom`, `layout`, `gfx/wild_buzzard_renderer`, `gfx/wild_buzzard_text_webrender`, and `gfx/wild_buzzard_headless`; the Stylo adapter's independently enabled lint policy; root lock metadata; architecture, provenance, status, and handoff documents
- Contract added or changed: public `DocumentVersion` pairs one `DocumentId` with its local revision; layout, scene compilation, shaped-text frames, frame requests, RGBA8 results, and pipeline evidence carry that exact type; `StaticPageEngine` applies bounded component policies and returns explicit `CompositionStatus` rather than claiming separate frames are composed
- Tests run and results: focused strict Clippy passed for DOM, layout, renderer, text-WebRender, and headless crates; DOM 9, layout 15, renderer 17, text-WebRender 3, headless unit 12, real-frame 3, and shaped-text 1 tests passed with all doc tests; the independently locked integration workspace passed check, strict no-dependency Clippy, 2 integration tests, release build, and warning-denied no-dependency rustdoc on `x86_64-unknown-linux-gnu`
- Parity evidence: a deterministic numeric-loopback HTML/CSS fixture uses imported Stylo values to paint exact background pixels in a real EGL/WebRender RGBA8 frame; every pending run uses Rust shaping and one exact non-whitespace shaped result produces changed pixels in a second real WebRender frame; sequential documents retain distinct IDs and a lower local revision is accepted without synthetic rebasing
- Known behavioral differences: synchronous loopback-only operation; whole-body UTF-8 only; no
  redirects, Fetch/CORS/CSP/origin lifecycle, external resources, images, media, script, window, or
  general network; text lacks complete CSS font inputs and finalized shaped-object identity; only
  one shaped run is separately painted, with provisional origin/baseline behavior. W2-A6N adds
  atomic generation-checked external frame publication, but it does not roll back an internal
  renderer submission made by this synchronous executor.
- Unsafe or FFI introduced: `wild_buzzard_engine` forbids unsafe code; no new FFI was added; it reaches the already audited imported Stylo unsafe internals and Linux EGL/OpenGL, FreeType, and Fontconfig boundaries through their existing Rust owners
- Licenses and provenance: the new first-party crate is MPL-2.0; every imported component retains the exact provenance and license recorded in `docs/upstream-components.toml`
- Provider or network implications: runtime networking remains numeric loopback HTTP only, redirects are rejected, and no DNS, telemetry, provider service, credential, or unsolicited request path was added
- Blocked on: no blocker to the next graphics/layout integration. W2-A4D supplies the reusable
  multi-run composed graphics contract and W2-A6N supplies generation-checked publication; M1 is
  blocked on connecting the synchronous engine to those contracts and Linux presentation.
- Recommended next action: retain the exact bounded shaped allocation for each finalized layout
  fragment, project `first_baseline`, call W2-A4D's `render_composed` once, and publish that frame
  through W2-A6N with a deterministic URL-to-zero-pending-text integration fixture.
