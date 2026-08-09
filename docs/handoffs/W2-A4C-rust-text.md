# W2-A4C Rust text and WebRender handoff

- Task: W2-A4C bounded Rust font selection, shaping, glyph registration, and real WebRender pixel proof
- Owner: Agent 4 — graphics and media; hardened and independently validated by the main orchestrator
- Status: Complete for the isolated text-to-glyph-to-pixel boundary. W2-A4D subsequently added a
  complete-inventory graphics composition contract, but the synchronous browser engine has not
  connected finalized layout shaping to it. This is not a complete production text-parity result.
- Firefox commit and reference paths: ESR153 `c19b7e89270787889495688244ec6ee8e79288a1`; `gfx/thebes/gfxFont.{h,cpp}`, `gfxTextRun.{h,cpp}`, `gfxHarfBuzzShaper.{h,cpp}`, `gfxPlatformFontList.{h,cpp}`, Linux platform implementations, `gfx/harfbuzz`, WebRender font resource APIs, and relevant full-history fixes are recorded in `gfx/wild_buzzard_text/README.md`
- Wild Buzzard paths changed: `gfx/wild_buzzard_text`, `gfx/wild_buzzard_text_webrender`, `gfx/wild_buzzard_headless`, root `Cargo.toml` and `Cargo.lock`, provenance/status documents, and this handoff
- Contract added or changed: bounded immutable `TextRequest`/`ShapedText`; deterministic or Linux font policies; complete run/glyph/cluster metrics; bounded exact-key shaping cache; transactional renderer-scoped `TextFontRegistry`; `ShapedTextFrame`; explicit text and renderer teardown reports
- Tests run and results: 27 focused tests passed with 0 failures, including Unicode bidi and combining clusters, Fira Code contextual shaping, exact cache limits, malformed font rejection, typed missing-instance failure, WebRender registry reuse/bounds, and an actual EGL/OpenGL frame whose HarfRust-shaped glyphs produced checked pixels
- Parity evidence: Fontique selects the exact font data, Parley performs Unicode analysis and run construction, HarfRust shapes OpenType glyphs, the immutable result retains exact font blobs/face indices/clusters/metrics, and the adapter submits matching WebRender font templates/instances/glyphs without reshaping
- Known behavioral differences: the synchronous engine still emits `PendingTextRun` and a separate
  glyph proof; one unwrapped line only; no downloadable web-font lifecycle; explicit forced
  paragraph direction and non-empty variation coordinates currently fail closed; complex scripts,
  emoji fallback, vertical text, line breaking, justification, decorations, selection, exhaustive
  fallback, and AppImage native closure remain parity work.
- Unsafe or FFI introduced: both new first-party crates forbid unsafe code; Linux system-font mode uses audited third-party dynamic Fontconfig and read-only font mapping boundaries, while WebRender retains its recorded FreeType glyph-rasterizer boundary
- Licenses and provenance: first-party contracts are MPL-2.0; exact registry crate versions/source revisions and their MIT/Apache/Unicode licenses plus the copied Fira Code OFL notice and byte hashes are recorded in `docs/upstream-components.toml`
- Provider or network implications: none; shaping and registration perform no telemetry, provider-service, credential, or runtime network operations
- Blocked on: no blocker to continued integration; an end-to-end text claim requires layout to measure and paint the same `Arc<ShapedText>`, plus CSS Fonts behavior and script/font coverage
- Recommended next action: retain the exact measured `Arc<ShapedText>` for each finalized fragment,
  feed the complete inventory through W2-A4D and W2-A6N, add web-font/fallback lifecycle contracts,
  and drive screenshot/reftest fixtures through DOM, Stylo, layout, shaping, WebRender, and Linux
  presentation.
