# W2-A6C composed static-page and navigation-frame integration

- Task: Connect the canonical finalized text inventory from the bounded static-page engine to
  W2-A4D's exact-scene composed renderer and publish the resulting frame through W2-A6N.
- Owner: Agent 6 — product/UI integration; independently reviewed after source freeze.
- Status: Complete for the bounded headless M1 contract. This is not a window, browser, complete
  navigation stack, CSS/rendering parity result, or Firefox UI parity result.
- Firefox commit and source paths: ESR153
  `c19b7e89270787889495688244ec6ee8e79288a1`; Firefox layout text baselines, WebRender display
  list construction, navigation publication, reftests, and browser tests remain behavioral
  reference. No Firefox implementation or branded UI was copied.
- Wild Buzzard paths changed: `browser/wild_buzzard_engine` plus current-status architecture and
  handoff documentation. No manifest, lockfile, imported source, renderer-internal API, or platform
  implementation changed.
- Contract added or changed: the engine shapes only `CompiledScene::pending_text()` after scene
  finalization, in canonical order and including whitespace. It retains each exact bounded
  `Arc<ShapedText>` in `ShapedSceneText`, projects line extent as `first_baseline` above and
  `height - first_baseline` below the baseline, and passes the original compiled scene plus the
  complete inventory to one `HeadlessRenderer::render_composed` call. The public result and
  navigation lease carry one RGBA8 frame; `EngineFrame::from_rendered` rejects nonzero pending
  text. The retired `CompositionStatus`, page/glyph-proof split, and corresponding navigation API
  cannot be consumed by new code.
- Transaction and publication rule: page primitives, fonts, positioned glyphs, root pipeline,
  epoch, notification, and frame generation enter one renderer transaction. Pre-send validation
  cannot publish renderer state. A post-send failure poisons the renderer; navigation publishes
  only a successful owned result after rechecking the current navigation generation and
  cancellation under its existing lock. A successful leased frame has zero pending text.
- Tests run and results: an independently fresh locked run passed strict no-dependency all-target
  Clippy and all 23 tests then present. After both low evidence gaps were closed, the same strict
  Clippy gate and all 24 tests passed (6 unit, 15 navigation-facade, and 3 real static-pipeline),
  with zero failures. Exact-file rustfmt, diff, manifest/lock, artifact, symlink, unsafe/FFI,
  native-code, provider, platform, Firefox-dependency, and stale-consumer audits passed. The added
  regressions identify a glyph pixel in the exact worker-leased frame and exercise fail-closed
  conversion of a real lower-level frame with pending text.
- Parity evidence: numeric-loopback fixtures prove two positioned text blocks become four canonical
  word runs, exact ordering and first-baseline mismatches are rejected, whitespace is resolved
  without invented glyphs, page and glyph pixels coexist in one zero-pending frame, repeated loads
  are byte-identical, and the dedicated worker publishes the composed result through one exact
  generation-tagged lease before clean shutdown.
- Known behavioral differences: synchronous numeric-loopback HTTP, whole-body explicit UTF-8,
  incomplete Fetch/navigation lifecycle, incomplete CSS and layout, initial font family/style/
  weight/spacing inputs, embedded fallback only, no downloadable fonts, no window/input,
  no JavaScript/DOM binding, and no general website support. Preliminary display-list byte evidence
  measures the validated pending-text list; the final glyph list is rebuilt privately and is not
  misreported by that field.
- Unsafe or FFI introduced: none. `wild_buzzard_engine` forbids unsafe code and reaches the already
  audited Stylo, EGL/OpenGL, FreeType, Fontconfig, and WebRender boundaries through their owners.
- Licenses and provenance: no imported source or license changed; existing exact component pins and
  licenses remain recorded in `docs/upstream-components.toml`.
- Provider or network implications: none beyond the explicit numeric-loopback HTTP capability; no
  DNS, external endpoint, credential, telemetry, provider service, or unsolicited request was
  introduced.
- Blocked on: no blocker within M1. A product browser still requires the Linux window/surface/input
  owner, rooted Brimstone DOM bindings and document tasks, normal secure networking, storage,
  media, accessibility, automation, and the broader parity program.
- Recommended next action: present this exact generation-tagged composed-frame contract through a
  minimal Rust-native Linux x86-64 window without exposing engine internals, while Agent 2 advances
  rooted Brimstone execution and the browser-owned Wasmtime boundary.
