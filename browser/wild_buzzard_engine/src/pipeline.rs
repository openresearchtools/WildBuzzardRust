use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use num_traits::ToPrimitive;
use wild_buzzard_dom::DocumentVersion;
use wild_buzzard_headless::{
    FrameRequest, FrameSize, HeadlessLimits, HeadlessRenderer, RgbaFrame, ShapedSceneText,
    ShutdownReport,
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
    TextLimits, TextRequest, TextResource, TextShutdownReport, TextSystem,
};

use crate::{PipelineError, PipelineStage};

const PAGE_PIPELINE: PipelineKey = PipelineKey::new(0x5742, 1);
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
    /// Serialized bytes in the validated pending-text display list before composition.
    /// The glyph-containing list is rebuilt privately inside `render_composed`.
    pub pre_composition_display_list_bytes: usize,
}

/// Aggregate evidence from shaping every finalized scene text record.
///
/// Speculative wrap measurements remain transient. After scene compilation,
/// the engine recovers one exact bounded [`Arc<ShapedText>`] for every pending
/// record and retains those allocations through the composed transaction.
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
}

/// Owned outputs from one exact static-page load.
#[derive(Debug)]
pub struct RenderedStaticPage {
    /// Stage counts proving the concrete fetch-to-WebRender path.
    pub evidence: PipelineEvidence,
    /// Text measurement and shaping counts.
    pub text: TextEvidence,
    /// One real `WebRender` RGBA8 frame containing every admitted primitive.
    /// A successful frame always has zero pending text.
    pub frame: RgbaFrame,
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
    /// The renderer submits page primitives, fonts, positioned glyphs, epoch,
    /// and frame generation in one transaction. A post-send failure can still
    /// leave internal renderer state changed and poisons the renderer; the
    /// navigation-generation owner therefore publishes only this method's
    /// successful owned result.
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
        let pre_composition_display_list_bytes = compiled.built_display_list().size_in_bytes();
        let shaped = shape_pending_runs(&self.text, &compiled, cancellation, deadline)?;
        checkpoint(cancellation, deadline, PipelineStage::ComposedRender)?;

        let epoch = self.reserve_epoch()?;
        let frame = self.renderer.render_composed(
            compiled,
            &shaped.entries,
            FrameRequest::new(document_version, epoch),
        )?;
        debug_assert_eq!(frame.pending_text_runs(), 0);
        checkpoint(cancellation, deadline, PipelineStage::ComposedRender)?;
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
                pre_composition_display_list_bytes,
            },
            text: TextEvidence {
                layout_measurement_requests,
                shaped_runs: shaped.run_count,
                glyphs: shaped.glyphs,
                clusters: shaped.clusters,
            },
            frame,
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
    entries: Vec<ShapedSceneText>,
}

fn shape_pending_runs(
    text: &ShapingTextMeasurer,
    compiled: &wild_buzzard_renderer::CompiledScene,
    cancellation: &CancellationToken,
    deadline: Instant,
) -> Result<ShapedPendingRuns, PipelineError> {
    let runs = compiled.scene().pending_text();
    let document_version = compiled.document_version();
    let mut glyphs = 0_usize;
    let mut clusters = 0_usize;
    let mut entries = Vec::new();
    entries
        .try_reserve_exact(runs.len())
        .map_err(|_| TextError::AllocationFailed {
            resource: TextResource::Runs,
            requested: runs.len(),
        })?;

    for run in runs {
        checkpoint(cancellation, deadline, PipelineStage::TextShaping)?;
        let request = request_from_app_units(run.text(), run.font_size(), run.line_height())?;
        let shaped = text.shape(&request)?;
        glyphs = glyphs
            .checked_add(shaped.glyph_count())
            .ok_or(PipelineError::EvidenceOverflow)?;
        clusters = clusters
            .checked_add(shaped.cluster_count())
            .ok_or(PipelineError::EvidenceOverflow)?;
        entries.push(ShapedSceneText::new(
            document_version,
            run.id().index(),
            shaped,
        ));
    }

    Ok(ShapedPendingRuns {
        run_count: runs.len(),
        glyphs,
        clusters,
        entries,
    })
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
        ascent: px_to_app_units(metrics.first_baseline(), InvalidTextField::OutputMetric)?,
        descent: px_to_app_units(
            metrics.height() - metrics.first_baseline(),
            InvalidTextField::OutputMetric,
        )?,
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use wild_buzzard_renderer::{
        GeometryField, SceneBuildError, SceneTextDescriptor, SceneTextMetrics,
    };

    const FINALIZED_TEXT_FIXTURE: &str = r"<!doctype html>
        <style>
          html, body { margin: 0; }
          div { display: block; font-size: 16px; line-height: 40px; }
        </style>
        <div>alpha</div><div>bravo</div>";

    fn descriptor(text: &ShapedSceneText) -> SceneTextDescriptor<'_> {
        let metrics = text.shaped().metrics();
        SceneTextDescriptor::new(
            text.document_version(),
            text.pending_index(),
            text.shaped().text(),
            SceneTextMetrics::new(
                metrics.full_width(),
                metrics.height(),
                metrics.first_baseline(),
                text.font_size_px().unwrap_or(0.0),
                metrics.line_height(),
            ),
        )
    }

    #[test]
    fn finalized_inventory_projects_first_baseline_and_rejects_rebinding() {
        let mut parser = HtmlParser::new(TokenizerLimits::default());
        parser.feed(FINALIZED_TEXT_FIXTURE).unwrap();
        let parsed = parser.finish().unwrap();
        let snapshot = parsed.document.snapshot().unwrap();
        let style_options = StaticStyleOptions {
            viewport_width: 320,
            viewport_height: 200,
            limits: StyleLimits::default(),
        };
        let stylo = prepare_computed_styles(snapshot.clone(), style_options).unwrap();
        let text = ShapingTextMeasurer::new(TextLimits::default(), FontSourcePolicy::EmbeddedOnly)
            .unwrap();
        text.begin_layout();
        let layout = layout_document_with_style_snapshot_and_limits(
            &snapshot,
            Viewport::from_css_pixels(320, 200),
            stylo.layout_styles(),
            &text,
            LayoutLimits::default(),
        )
        .unwrap();
        assert!(text.take_layout_error().is_none());

        let compiled = SceneCompiler::new(SceneLimits::default())
            .compile(
                &layout,
                CompileRequest::new(layout.document_version, PAGE_PIPELINE),
            )
            .unwrap();
        let cancellation = wild_buzzard_net::CancellationSource::new();
        let deadline = Instant::now().checked_add(Duration::from_secs(5)).unwrap();
        let shaped = shape_pending_runs(&text, &compiled, &cancellation.token(), deadline).unwrap();

        assert_eq!(compiled.scene().pending_text().len(), 2);
        assert_eq!(shaped.run_count, 2);
        assert_eq!(shaped.entries.len(), 2);
        assert_eq!(shaped.entries[0].pending_index(), 0);
        assert_eq!(shaped.entries[1].pending_index(), 1);
        assert!(
            text.layout_measurement_requests() > shaped.entries.len(),
            "speculative measurements must not become retained inventory entries"
        );

        let first_metrics = shaped.entries[0].shaped().metrics();
        assert_ne!(
            px_to_app_units(
                first_metrics.first_baseline(),
                InvalidTextField::OutputMetric
            )
            .unwrap(),
            px_to_app_units(first_metrics.ascent(), InvalidTextField::OutputMetric).unwrap(),
            "explicit leading must distinguish the line baseline from font ascent"
        );
        let descriptors: Vec<_> = shaped.entries.iter().map(descriptor).collect();
        compiled
            .validate_text_map(&descriptors)
            .expect("the exact first-baseline projection and canonical inventory must match");

        assert!(matches!(
            compiled.validate_text_map(&[descriptors[1], descriptors[0]]),
            Err(SceneBuildError::OutOfOrderTextResolution {
                expected: 0,
                actual: 1
            })
        ));

        let exact = descriptors[0].metrics();
        let wrong_baseline = SceneTextDescriptor::new(
            descriptors[0].document_version(),
            descriptors[0].pending_index(),
            descriptors[0].text(),
            SceneTextMetrics::new(
                exact.full_width(),
                exact.height(),
                exact.first_baseline() + 1.0,
                exact.font_size(),
                exact.line_height(),
            ),
        );
        assert!(matches!(
            compiled.validate_text_map(&[wrong_baseline, descriptors[1]]),
            Err(SceneBuildError::TextMetricMismatch {
                pending_index: 0,
                field: GeometryField::Baseline,
                ..
            })
        ));
    }
}
