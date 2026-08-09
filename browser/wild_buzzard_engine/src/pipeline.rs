use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use num_traits::ToPrimitive;
use wild_buzzard_dom::DocumentVersion;
use wild_buzzard_headless::{
    FrameRequest, FrameSize, HeadlessLimits, HeadlessRenderer, RgbaFrame, ShapedTextFrame,
    ShutdownReport, TextColor, TextOrigin, TextPipelineKey,
};
use wild_buzzard_html::{HtmlParser, TokenizerLimits};
use wild_buzzard_layout::{
    Au, ComputedStyle, LayoutLimits, MonospaceTextMeasurer, TextMeasurer, TextMetrics, Viewport,
    layout_document_with_style_snapshot_and_limits,
};
use wild_buzzard_net::{
    CancellationToken, ClientConfig, HttpClient, LoopbackTarget, RedirectPolicy, Request,
};
use wild_buzzard_renderer::{CompileRequest, PipelineKey, SceneCompiler, SceneLimits};
use wild_buzzard_stylo_adapter::{StaticStyleOptions, StyleLimits, prepare_computed_styles};
use wild_buzzard_text::{
    FontSourcePolicy, InvalidTextField, LineHeight, LineHeightProvenance, ShapedText, TextError,
    TextLimits, TextRequest, TextShutdownReport, TextSystem,
};

use crate::{PipelineError, PipelineStage};

const PAGE_PIPELINE: PipelineKey = PipelineKey::new(0x5742, 1);
const TEXT_PIPELINE: TextPipelineKey = TextPipelineKey::new(0x5742, 2);
const FIRST_EPOCH: u32 = 1;

/// All resource and time policy for one static-page engine instance.
#[derive(Clone, Debug)]
pub struct StaticPageConfig {
    /// Fixed CSS/device-pixel width at scale one.
    pub viewport_width: u32,
    /// Fixed CSS/device-pixel height at scale one.
    pub viewport_height: u32,
    /// Whole-operation deadline checked between bounded synchronous stages.
    pub operation_timeout: Duration,
    /// Loopback HTTP limits and socket inactivity timeouts.
    pub network: ClientConfig,
    /// HTML tokenizer and tree-depth limits.
    pub parser: TokenizerLimits,
    /// Immutable Stylo adapter and CSS work limits.
    pub style: StyleLimits,
    /// Layout recursion limits.
    pub layout: LayoutLimits,
    /// Rust text-system, font, cache, glyph, and coordinate limits.
    pub text: TextLimits,
    /// Linux production fonts or the deterministic embedded-only test source.
    pub font_source: FontSourcePolicy,
    /// Layout-to-WebRender scene limits.
    pub scene: SceneLimits,
    /// EGL/WebRender frame, resource, and internal deadline limits.
    pub headless: HeadlessLimits,
}

impl Default for StaticPageConfig {
    fn default() -> Self {
        Self {
            viewport_width: 800,
            viewport_height: 600,
            operation_timeout: Duration::from_secs(15),
            network: ClientConfig::default(),
            parser: TokenizerLimits::default(),
            style: StyleLimits::default(),
            layout: LayoutLimits::default(),
            text: TextLimits::default(),
            font_source: FontSourcePolicy::LinuxSystemWithEmbeddedFallback,
            scene: SceneLimits::default(),
            headless: HeadlessLimits::default(),
        }
    }
}

/// Evidence for the exact immutable document state consumed and rendered.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PipelineEvidence {
    /// Exact document identity and local revision consumed by every downstream stage.
    pub document_version: DocumentVersion,
    /// Final HTTP status returned without redirect following.
    pub http_status: u16,
    /// Decoded response-body bytes retained by the bounded transport.
    pub source_bytes: usize,
    /// Immutable nodes in the DOM snapshot.
    pub dom_nodes: usize,
    /// Recoverable HTML diagnostics retained by the parser.
    pub html_diagnostics: usize,
    /// Native imported-Stylo computed-style entries.
    pub stylo_style_entries: usize,
    /// Recoverable CSS diagnostics retained by the Stylo adapter.
    pub style_diagnostics: usize,
    /// CSS diagnostics dropped at the configured bound.
    pub dropped_style_diagnostics: usize,
    /// Layout boxes published for the exact style revision.
    pub layout_boxes: usize,
    /// Recoverable layout warnings.
    pub layout_warnings: usize,
    /// Validated renderer-independent scene items.
    pub scene_items: usize,
    /// Serialized real `WebRender` display-list bytes.
    pub display_list_bytes: usize,
}

/// Aggregate evidence from shaping every pending layout text run.
///
/// The current pending-run contract carries text, font size, used line height,
/// and color. It does not carry CSS family, weight, style, letter spacing, or
/// an exact glyph-baseline placement contract, so this evidence does not claim
/// those properties reached the independent glyph proof. Layout's measurement
/// trait also returns metrics rather than the exact shaped allocation, so this
/// evidence does not claim `Arc` identity with layout's transient result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TextEvidence {
    /// Measurement requests served by the same Rust shaper during layout.
    pub layout_measurement_requests: usize,
    /// Pending page text runs shaped after scene compilation.
    pub shaped_runs: usize,
    /// Aggregate exact glyph count across those shaped runs.
    pub glyphs: usize,
    /// Aggregate Unicode cluster count across those shaped runs.
    pub clusters: usize,
    /// Index of the non-whitespace shaped run sent to the proof renderer.
    pub proof_run_index: Option<usize>,
}

/// Honest status of text composition in the returned page screenshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompositionStatus {
    /// The page had no pending text, so its page frame is complete for admitted primitives.
    NoText,
    /// Every text run was shaped, but the page frame contains only decorations.
    /// One shaped run is painted in the separate glyph-proof frame.
    SeparateGlyphProof {
        /// Number of shaped runs still omitted from the page display list.
        pending_page_runs: usize,
        /// Run selected for the independent `WebRender` proof.
        proof_run_index: usize,
    },
    /// Text existed and was shaped, but it contained no paintable non-whitespace run.
    WhitespaceOnlyText {
        /// Number of shaped whitespace-only page runs.
        pending_page_runs: usize,
    },
}

impl CompositionStatus {
    /// Returns whether the page screenshot contains every admitted visual primitive.
    #[must_use]
    pub const fn is_composed(self) -> bool {
        matches!(self, Self::NoText)
    }
}

/// Owned outputs from one exact static-page load.
#[derive(Debug)]
pub struct RenderedStaticPage {
    /// Stage counts proving the concrete fetch-to-WebRender path.
    pub evidence: PipelineEvidence,
    /// Text measurement and shaping counts.
    pub text: TextEvidence,
    /// Real `WebRender` RGBA8 page frame; pending text is not painted in it yet.
    pub page_frame: RgbaFrame,
    /// Real `WebRender` RGBA8 proof for one exact shaped run, when available.
    pub glyph_proof_frame: Option<RgbaFrame>,
    /// Explicit composition limitation for this result.
    pub composition: CompositionStatus,
}

/// Explicit cleanup reports from the text and `WebRender` owners.
#[derive(Debug)]
pub struct EngineShutdownReport {
    /// Headless renderer/backend/EGL cleanup evidence.
    pub renderer: ShutdownReport,
    /// Rust text-system cache cleanup evidence.
    pub text: TextShutdownReport,
}

/// Stateful Linux x86-64 static-page integration boundary.
pub struct StaticPageEngine {
    client: HttpClient,
    parser_limits: TokenizerLimits,
    style_options: StaticStyleOptions,
    layout_limits: LayoutLimits,
    scene_compiler: SceneCompiler,
    renderer: HeadlessRenderer,
    text: ShapingTextMeasurer,
    operation_timeout: Duration,
    next_epoch: u32,
}

impl StaticPageEngine {
    /// Initializes the bounded Rust text system and real Linux EGL/WebRender renderer.
    ///
    /// # Errors
    ///
    /// Returns a configuration, font-system, EGL, GL, or `WebRender` initialization error.
    pub fn new(config: StaticPageConfig) -> Result<Self, PipelineError> {
        if config.operation_timeout.is_zero() {
            return Err(PipelineError::InvalidConfiguration {
                field: "operation_timeout",
                detail: "must be non-zero",
            });
        }
        if config.viewport_width > i32::MAX.cast_unsigned()
            || config.viewport_height > i32::MAX.cast_unsigned()
        {
            return Err(PipelineError::InvalidConfiguration {
                field: "viewport",
                detail: "dimensions must fit signed CSS-pixel geometry",
            });
        }

        let size = FrameSize::new(config.viewport_width, config.viewport_height)?;
        let text = ShapingTextMeasurer::new(config.text, config.font_source)?;
        let renderer = HeadlessRenderer::new(size, config.headless)?;
        Ok(Self {
            client: HttpClient::new(config.network),
            parser_limits: config.parser,
            style_options: StaticStyleOptions {
                viewport_width: config.viewport_width,
                viewport_height: config.viewport_height,
                limits: config.style,
            },
            layout_limits: config.layout,
            scene_compiler: SceneCompiler::new(config.scene),
            renderer,
            text,
            operation_timeout: config.operation_timeout,
            next_epoch: FIRST_EPOCH,
        })
    }

    /// Fetches and processes one numeric-loopback HTTP URL through the deepest
    /// currently composable static-page path.
    ///
    /// Redirects are rejected, no DNS is performed, response bytes are bounded,
    /// and cancellation/deadlines are checked between every synchronous stage.
    ///
    /// # Errors
    ///
    /// Returns a structured transport, parse, DOM, style, layout, text, scene,
    /// cancellation, deadline, EGL, `WebRender`, or readback failure.
    #[allow(clippy::too_many_lines)]
    pub fn load(
        &mut self,
        url: &str,
        cancellation: &CancellationToken,
    ) -> Result<RenderedStaticPage, PipelineError> {
        let deadline = Instant::now()
            .checked_add(self.operation_timeout)
            .ok_or(PipelineError::DeadlineOverflow)?;
        self.load_with_deadline(url, cancellation, deadline)
    }

    /// Processes one numeric-loopback URL with a caller-owned absolute deadline.
    ///
    /// An already-elapsed deadline fails at the first checkpoint before target
    /// parsing or network access. Rendering remains additionally bounded by the
    /// renderer deadline in [`StaticPageConfig::headless`].
    ///
    /// This synchronous proof uses two renderer transactions when paintable
    /// text exists. If cancellation, the deadline, or glyph-proof rendering
    /// fails after the page transaction succeeds, this method returns an error
    /// but cannot roll back the renderer epoch or that internal page
    /// publication. A product presentation owner must add an atomic
    /// navigation-generation gate rather than treating `Err` as proof that no
    /// renderer state changed.
    ///
    /// # Errors
    ///
    /// Returns the same structured failures as [`Self::load`], including
    /// [`PipelineError::DeadlineExceeded`] at the last observed stage.
    #[allow(clippy::too_many_lines)]
    pub fn load_with_deadline(
        &mut self,
        url: &str,
        cancellation: &CancellationToken,
        deadline: Instant,
    ) -> Result<RenderedStaticPage, PipelineError> {
        checkpoint(cancellation, deadline, PipelineStage::Fetch)?;

        let target = LoopbackTarget::parse(url)?;
        let request = Request::get(target, RedirectPolicy::Reject)
            .with_cancellation(cancellation.clone())
            .with_deadline(deadline);
        let response = self.client.execute(&request)?;
        let http_status = response.head().status().as_u16();
        if !(200..=299).contains(&http_status) {
            return Err(PipelineError::HttpStatus(http_status));
        }
        let source = response.read_body_to_end()?;
        checkpoint(cancellation, deadline, PipelineStage::Parse)?;

        let html = std::str::from_utf8(&source).map_err(|_| PipelineError::NonUtf8Html)?;
        let mut parser = HtmlParser::new(self.parser_limits);
        parser.feed(html)?;
        let parsed = parser.finish()?;
        let html_diagnostics = parsed.errors.len();
        checkpoint(cancellation, deadline, PipelineStage::Snapshot)?;

        let snapshot = parsed.document.snapshot()?;
        let dom_nodes = snapshot.nodes_in_document_order().len();
        checkpoint(cancellation, deadline, PipelineStage::Style)?;

        let stylo = prepare_computed_styles(snapshot.clone(), self.style_options)?;
        let stylo_style_entries = stylo.stylo_style_count();
        let style_diagnostics = stylo.diagnostics().len();
        let dropped_style_diagnostics = stylo.dropped_diagnostic_count();
        checkpoint(cancellation, deadline, PipelineStage::Layout)?;

        self.text.begin_layout();
        let viewport = Viewport::from_css_pixels(
            self.style_options.viewport_width.cast_signed(),
            self.style_options.viewport_height.cast_signed(),
        );
        let layout = layout_document_with_style_snapshot_and_limits(
            &snapshot,
            viewport,
            stylo.layout_styles(),
            &self.text,
            self.layout_limits,
        )?;
        if let Some(error) = self.text.take_layout_error() {
            return Err(error.into());
        }
        let layout_measurement_requests = self.text.layout_measurement_requests();
        let layout_boxes = layout.boxes.len();
        let layout_warnings = layout.warnings.len();
        checkpoint(cancellation, deadline, PipelineStage::SceneCompilation)?;

        let document_version = layout.document_version;
        let compiled = self.scene_compiler.compile(
            &layout,
            CompileRequest::new(document_version, PAGE_PIPELINE),
        )?;
        let scene_items = compiled.scene().items().len();
        let display_list_bytes = compiled.built_display_list().size_in_bytes();
        let shaped = self.shape_pending_runs(&compiled, cancellation, deadline)?;
        checkpoint(cancellation, deadline, PipelineStage::PageRender)?;

        let page_epoch = self.reserve_epoch()?;
        let page_frame = self
            .renderer
            .render(compiled, FrameRequest::new(document_version, page_epoch))?;
        checkpoint(cancellation, deadline, PipelineStage::TextProofRender)?;

        let (glyph_proof_frame, composition) = if let Some(proof) = shaped.proof {
            let proof_epoch = self.reserve_epoch()?;
            let frame = ShapedTextFrame::new(document_version, TEXT_PIPELINE, proof.shaped)
                .with_origin(proof.origin)
                .with_color(proof.color);
            let rendered = self
                .renderer
                .render_shaped_text(&frame, FrameRequest::new(document_version, proof_epoch))?;
            (
                Some(rendered),
                CompositionStatus::SeparateGlyphProof {
                    pending_page_runs: shaped.run_count,
                    proof_run_index: proof.index,
                },
            )
        } else if shaped.run_count == 0 {
            (None, CompositionStatus::NoText)
        } else {
            (
                None,
                CompositionStatus::WhitespaceOnlyText {
                    pending_page_runs: shaped.run_count,
                },
            )
        };

        checkpoint(cancellation, deadline, PipelineStage::TextProofRender)?;
        Ok(RenderedStaticPage {
            evidence: PipelineEvidence {
                document_version,
                http_status,
                source_bytes: source.len(),
                dom_nodes,
                html_diagnostics,
                stylo_style_entries,
                style_diagnostics,
                dropped_style_diagnostics,
                layout_boxes,
                layout_warnings,
                scene_items,
                display_list_bytes,
            },
            text: TextEvidence {
                layout_measurement_requests,
                shaped_runs: shaped.run_count,
                glyphs: shaped.glyphs,
                clusters: shaped.clusters,
                proof_run_index: shaped.proof_index,
            },
            page_frame,
            glyph_proof_frame,
            composition,
        })
    }

    /// Explicitly tears down WebRender/EGL and releases text caches.
    ///
    /// # Errors
    ///
    /// Returns a bounded renderer shutdown failure after local cleanup has still run.
    pub fn shutdown(self) -> Result<EngineShutdownReport, PipelineError> {
        let Self { renderer, text, .. } = self;
        let text = text.shutdown();
        let renderer = renderer.shutdown()?;
        Ok(EngineShutdownReport { renderer, text })
    }

    fn shape_pending_runs(
        &self,
        compiled: &wild_buzzard_renderer::CompiledScene,
        cancellation: &CancellationToken,
        deadline: Instant,
    ) -> Result<ShapedPendingRuns, PipelineError> {
        let runs = compiled.scene().pending_text();
        let mut glyphs = 0_usize;
        let mut clusters = 0_usize;
        let mut proof = None;

        for (index, run) in runs.iter().enumerate() {
            checkpoint(cancellation, deadline, PipelineStage::TextShaping)?;
            let request = request_from_app_units(run.text(), run.font_size(), run.line_height())?;
            let shaped = self.text.shape(&request)?;
            glyphs = glyphs
                .checked_add(shaped.glyph_count())
                .ok_or(PipelineError::EvidenceOverflow)?;
            clusters = clusters
                .checked_add(shaped.cluster_count())
                .ok_or(PipelineError::EvidenceOverflow)?;
            if proof.is_none() && !run.text().trim().is_empty() {
                proof = Some(ProofRun {
                    index,
                    origin: TextOrigin::new(
                        coordinate_app_units_to_px(run.rect().x())?,
                        coordinate_app_units_to_px(run.rect().y())?,
                    ),
                    color: TextColor::rgba(
                        run.color().red(),
                        run.color().green(),
                        run.color().blue(),
                        run.color().alpha(),
                    ),
                    shaped,
                });
            }
        }

        let proof_index = proof.as_ref().map(|entry| entry.index);
        Ok(ShapedPendingRuns {
            run_count: runs.len(),
            glyphs,
            clusters,
            proof_index,
            proof,
        })
    }

    fn reserve_epoch(&mut self) -> Result<u32, PipelineError> {
        if self.next_epoch == u32::MAX {
            return Err(PipelineError::EpochExhausted);
        }
        let epoch = self.next_epoch;
        self.next_epoch = self.next_epoch.saturating_add(1);
        Ok(epoch)
    }
}

struct ShapedPendingRuns {
    run_count: usize,
    glyphs: usize,
    clusters: usize,
    proof_index: Option<usize>,
    proof: Option<ProofRun>,
}

struct ProofRun {
    index: usize,
    origin: TextOrigin,
    color: TextColor,
    shaped: Arc<ShapedText>,
}

struct ShapingTextMeasurer {
    system: Mutex<TextSystem>,
    first_layout_error: Mutex<Option<TextError>>,
    layout_measurement_requests: AtomicUsize,
}

impl ShapingTextMeasurer {
    fn new(limits: TextLimits, source: FontSourcePolicy) -> Result<Self, TextError> {
        let system = match source {
            FontSourcePolicy::EmbeddedOnly => TextSystem::new_deterministic(limits)?,
            FontSourcePolicy::LinuxSystemWithEmbeddedFallback => TextSystem::new_linux(limits)?,
        };
        Ok(Self {
            system: Mutex::new(system),
            first_layout_error: Mutex::new(None),
            layout_measurement_requests: AtomicUsize::new(0),
        })
    }

    fn begin_layout(&self) {
        self.layout_measurement_requests.store(0, Ordering::Release);
        *lock_unpoisoned(&self.first_layout_error) = None;
    }

    fn layout_measurement_requests(&self) -> usize {
        self.layout_measurement_requests.load(Ordering::Acquire)
    }

    fn take_layout_error(&self) -> Option<TextError> {
        lock_unpoisoned(&self.first_layout_error).take()
    }

    fn shape(&self, request: &TextRequest) -> Result<Arc<ShapedText>, TextError> {
        lock_unpoisoned(&self.system).shape(request)
    }

    fn shutdown(self) -> TextShutdownReport {
        into_inner_unpoisoned(self.system).shutdown()
    }

    fn record_layout_error(&self, error: TextError) {
        let mut slot = lock_unpoisoned(&self.first_layout_error);
        if slot.is_none() {
            *slot = Some(error);
        }
    }
}

impl TextMeasurer for ShapingTextMeasurer {
    fn measure(&self, text: &str, style: &ComputedStyle) -> TextMetrics {
        self.layout_measurement_requests
            .fetch_add(1, Ordering::AcqRel);
        let result = request_from_style(text, style)
            .and_then(|request| self.shape(&request))
            .and_then(|shaped| metrics_to_layout(shaped.metrics()));
        match result {
            Ok(metrics) => metrics,
            Err(error) => {
                self.record_layout_error(error);
                MonospaceTextMeasurer.measure(text, style)
            }
        }
    }
}

fn request_from_style(text: &str, style: &ComputedStyle) -> Result<TextRequest, TextError> {
    request_from_app_units_text(text, style.font_size.raw(), style.line_height.raw())
}

fn request_from_app_units(
    text: &str,
    font_size: i32,
    line_height: i32,
) -> Result<TextRequest, PipelineError> {
    request_from_app_units_text(text, font_size, line_height).map_err(Into::into)
}

fn request_from_app_units_text(
    text: &str,
    font_size: i32,
    line_height: i32,
) -> Result<TextRequest, TextError> {
    let font_size_px = app_units_to_px_text(font_size, InvalidTextField::FontSize)?;
    let line_height_px = app_units_to_px_text(line_height, InvalidTextField::LineHeight)?;
    Ok(
        TextRequest::new(text, font_size_px).with_line_height(LineHeight::Used {
            px: line_height_px,
            provenance: LineHeightProvenance::Explicit,
        }),
    )
}

fn metrics_to_layout(metrics: wild_buzzard_text::TextMetrics) -> Result<TextMetrics, TextError> {
    Ok(TextMetrics {
        advance: px_to_app_units(metrics.full_width(), InvalidTextField::OutputMetric)?,
        ascent: px_to_app_units(metrics.ascent(), InvalidTextField::OutputMetric)?,
        descent: px_to_app_units(metrics.descent(), InvalidTextField::OutputMetric)?,
    })
}

fn coordinate_app_units_to_px(raw: i32) -> Result<f32, PipelineError> {
    let raw = raw.to_f32().ok_or(TextError::InvalidValue {
        field: InvalidTextField::OutputCoordinate,
    })?;
    let value = raw
        / Au::PER_CSS_PX.to_f32().ok_or(TextError::InvalidValue {
            field: InvalidTextField::OutputCoordinate,
        })?;
    if value.is_finite() {
        Ok(value)
    } else {
        Err(TextError::InvalidValue {
            field: InvalidTextField::OutputCoordinate,
        }
        .into())
    }
}

fn app_units_to_px_text(raw: i32, field: InvalidTextField) -> Result<f32, TextError> {
    let raw = raw.to_f32().ok_or(TextError::InvalidValue { field })?;
    let value = raw
        / Au::PER_CSS_PX
            .to_f32()
            .ok_or(TextError::InvalidValue { field })?;
    if value.is_finite() && value > 0.0 {
        Ok(value)
    } else {
        Err(TextError::InvalidValue { field })
    }
}

fn px_to_app_units(value: f32, field: InvalidTextField) -> Result<Au, TextError> {
    if !value.is_finite() || value < 0.0 {
        return Err(TextError::InvalidValue { field });
    }
    let scaled = f64::from(value) * f64::from(Au::PER_CSS_PX);
    let raw = scaled
        .round()
        .to_i32()
        .ok_or(TextError::InvalidValue { field })?;
    Ok(Au::from_raw(raw))
}

fn checkpoint(
    cancellation: &CancellationToken,
    deadline: Instant,
    stage: PipelineStage,
) -> Result<(), PipelineError> {
    if cancellation.is_cancelled() {
        return Err(PipelineError::Cancelled { stage });
    }
    if Instant::now() >= deadline {
        return Err(PipelineError::DeadlineExceeded { stage });
    }
    Ok(())
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn into_inner_unpoisoned<T>(mutex: Mutex<T>) -> T {
    mutex
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
