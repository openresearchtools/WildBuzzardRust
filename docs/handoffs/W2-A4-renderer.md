# W2-A4 renderer-boundary handoff

- Task: W2-A4 immutable layout-to-WebRender scene and built-display-list boundary
- Owner: Agent 4 — graphics and media; hardened, integrated, and independently reviewed by the main orchestrator
- Status: Complete for the Wave 2 display-list boundary; this does not submit a frame, create a GPU renderer, shape text, or produce pixels
- Firefox commit and source paths: ESR153 `c19b7e89270787889495688244ec6ee8e79288a1`; WebRender builder/items/units plus Gecko display-list and clip-command adapters are enumerated in `gfx/wild_buzzard_renderer/README.md`
- Firefox test paths: Wrench raw display-list tests and focused display-list/displayport reftests are recorded in the crate README with relevant full-history revisions
- Wild Buzzard paths changed: `gfx/wild_buzzard_renderer`, root `Cargo.toml`, root `Cargo.lock`, and this status/handoff evidence
- Contract added or changed: exact-revision `SceneCompiler`; checked `SceneLimits`; immutable scene primitives and IDs; viewport root/clip contract; typed pending text; consuming `CompiledScene::into_webrender` submission boundary
- Tests run and results: 17 integration tests passed, 0 failed/ignored; root-integrated package formatting, all-target check, strict all-feature Clippy, locked tests, release build, and warning-denied rustdoc passed for `x86_64-unknown-linux-gnu` using the external build directory
- Parity evidence: deterministic preorder conversion; actual imported WebRender rectangle, border, clip, clip-chain, and built-list serialization; public-data round trip; conservative preallocation-size rejection; local-root clipping of negative/out-of-viewport primitives
- Known behavioral differences: text is not shaped or emitted; no renderer/device/surface, GPU submission, pixels, screenshots, stacking contexts, scrolling tree, transforms, images, effects, hit testing, retained invalidation, or full CSS painting
- Unsafe or FFI introduced: no first-party unsafe or FFI; adopted `peek-poke` serialization has upstream audited unsafe internals, and `webrender_api` contains unused unsafe surfaces; the used builder also reaches Linux `clock_gettime` through `zeitstempel`
- Licenses and provenance: MPL-2.0 first-party boundary informed by the ignored pinned ESR reference; imported WebRender API/support sources retain their MPL-2.0 and MIT/Apache-2.0 licensing
- Provider or network implications: None; the crate performs no filesystem or network access and contains no provider endpoint or telemetry
- Blocked on: font discovery/fallback and shaping are required before pending text can become glyph items; a Linux surface/renderer owner is required for frame submission and screenshot evidence
- Recommended next action: preserve this immutable contract while adding a Rust font/shaping resource service and a headless Linux WebRender renderer, then connect parser/DOM/Stylo/layout output to deterministic pixel tests
