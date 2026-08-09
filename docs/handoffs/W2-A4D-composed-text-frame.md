# W2-A4D composed positioned-text frame

- Task: Resolve every shaped layout text record into its original paint slot and submit fonts,
  page decorations, positioned glyphs, epoch, and frame generation in one bounded WebRender
  transaction.
- Owner: Agent 4 — graphics/media; implemented, corrected after independent NO-GO findings, and
  accepted only after a separate post-fix review.
- Status: Complete for the reusable graphics composition boundary. The synchronous browser engine
  does not call it yet, so this is not M1 completion, a window compositor, or rendering parity.
- Firefox reference: ESR153 `c19b7e89270787889495688244ec6ee8e79288a1`; imported WebRender,
  Firefox text/layout behavior, and relevant renderer/reftest paths remain behavioral reference.
  No Gecko, C++, or Firefox product implementation was copied.
- Wild Buzzard paths changed: `gfx/wild_buzzard_renderer`,
  `gfx/wild_buzzard_text_webrender`, and `gfx/wild_buzzard_headless` only.
- Contract added or changed: exact `DocumentVersion`, UTF-8 bytes, canonical pending ID order,
  app-unit-quantized width/height/first-baseline/font-size/line-height, renderer namespace, glyph
  coordinates, paint order, and aggregate resource limits are checked before composition. Missing,
  duplicate, unknown, reordered, wrong-text, wrong-metric, foreign-namespace, overflow, and limit
  failures are typed and occur before renderer submission. A private process-local identity is
  allocated without wraparound for each successful compilation, propagated through the complete
  resolution capability, and compared before any composition mutation; even a separately compiled
  byte-identical scene cannot accept that capability. Successful scenes contain zero pending text.
- Baseline rule: Parley glyph Y already contains `first_baseline`; composition adds the fragment
  top exactly once and never adds font ascent. A real-shaped-glyph regression proves the final Y
  exactly and rejects the former ascent-double-add projection.
- Transaction rule: `PreparedSceneText` reserves and stages complete font/instance and glyph state;
  one transaction installs those resources, the composed display list, root pipeline, epoch,
  notifications, and frame generation. Pre-send failures preserve epoch and registry retryability.
  Post-send failures poison the renderer while retaining committed identities for teardown.
- Tests run and results: 23 renderer, 5 text-adapter, and 20 headless tests passed (48 total),
  including four composed real-EGL tests, at least two canonical text entries, exact baseline
  geometry, every-fragment pixel contribution, deterministic repeated pixels, zero pending text,
  cross-scene and namespace rebinding rejection, mapping/namespace retry, resource reuse/teardown,
  and forced post-send timeout poisoning. A fresh post-fix reviewer also rejected a byte-identical
  cross-scene probe and returned GO. Strict Clippy, warning-denied rustdoc, locked Linux x86-64
  release builds, explicit rustfmt, diff, dependency-cycle, artifact, platform/provider, and unsafe
  audits passed.
- Parity evidence: this proves the admitted static decorations and all fixture-shaped runs can
  share one real EGL/WebRender RGBA8 frame. It does not prove CSS Fonts, multiline/vertical text,
  downloadable fonts, exhaustive fallback, selection/decorations, reftest parity, or site-scale
  rendering.
- Unsafe or FFI introduced: none in first-party code. The existing audited Linux EGL/OpenGL,
  FreeType, Fontconfig, and imported WebRender native boundaries remain unchanged.
- Dependency/provenance impact: no manifest, lockfile, dependency edge, imported source, provider,
  telemetry, or non-Linux product path changed. Namespace consistency is enforced; this is not a
  cryptographic proof that an arbitrary same-namespace key exists in a registry. The product
  `render_composed` path supplies only keys prepared by its bound registry.
- Remaining blocker: `browser/wild_buzzard_engine` currently reshapes then discards most exact
  `Arc<ShapedText>` values, projects layout baseline from `metrics.ascent()`, and emits a separate
  glyph proof. It must retain the exact shaped inventory, project above-baseline as
  `first_baseline` and below-baseline as `height - first_baseline`, then call `render_composed`.
- Recommended next action: connect that exact composed result to W2-A6N's generation-tagged frame
  publication and add one deterministic URL-to-zero-pending-text integration fixture.
