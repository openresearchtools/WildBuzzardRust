use std::collections::BTreeSet;
use std::mem::{size_of, size_of_val};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::{fmt, num::NonZeroU64};

use num_traits::ToPrimitive;
use wild_buzzard_dom::bindings::{ScriptMutationBatch, ScriptMutationLimits};
use wild_buzzard_dom::{DocumentSnapshot, DocumentVersion};
use wild_buzzard_headless::{
    FrameRequest, FrameSize, HeadlessError, HeadlessLimits, HeadlessRenderer, RgbaFrame,
    ShapedSceneText, ShutdownReport,
};
use wild_buzzard_html::{HtmlParser, TokenizerLimits};
use wild_buzzard_layout::{
    Au, ComputedStyle, LayoutLimits, MonospaceTextMeasurer, TextMeasurer, TextMetrics, Viewport,
    layout_document_with_style_snapshot_and_limits,
};
use wild_buzzard_net::{
    AlpnOutcome, CancellationToken, ClientConfig, ConnectionSecurity, Error as NetworkError,
    GeneralWebClient, GeneralWebConfig, GeneralWebNetworkAccess, GeneralWebRequest,
    GeneralWebResponse, GeneralWebTarget, HttpClient, LimitKind as NetworkLimitKind,
    LocalNetworkAccessPermissions, LoopbackTarget, RedirectPolicy, Request, TlsVersion, TrustStore,
    WebScheme,
};
use wild_buzzard_renderer::{
    CompileRequest, CompiledScene, PipelineKey, SceneCompiler, SceneLimits,
};
use wild_buzzard_stylo_adapter::{StaticStyleOptions, StyleLimits, prepare_computed_styles};
use wild_buzzard_text::{
    FontSourcePolicy, InvalidTextField, LineHeight, LineHeightProvenance, ShapedText, TextError,
    TextLimits, TextRequest, TextResource, TextShutdownReport, TextSystem,
};

use crate::document_policy::{UnboundDocumentResponseMetadata, capture_document_response_metadata};
use crate::dynamic::{
    DocumentUpdateError, DocumentUpdateRejection, DynamicRenderEvidence, LiveDocumentPage,
    RenderedDocumentUpdate, RenderedLiveDocument,
};
use crate::navigation::StyleDocumentCurrentOwner;
use crate::style_fetch::{
    NonProductStyleFetchAuthority, StyleFetchAuthority, StyleFetchOwnerError,
    StyleFetchTransportPolicy,
};
use crate::{
    DocumentPolicyError, NavigationAlpn, NavigationCommitMetadata, NavigationConnectionSecurity,
    NavigationId, NavigationTlsVersion, PipelineError, PipelineStage, RedirectLocationFailure,
};

const PAGE_PIPELINE: PipelineKey = PipelineKey::new(0x5742, 1);
const FIRST_EPOCH: u32 = 1;
/// Maximum number of top-level HTTP redirects admitted by one navigation.
pub const MAX_TOP_LEVEL_REDIRECTS: u8 = 10;

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
    /// Per-batch script mutation limits, always bounded by the DOM hard caps.
    pub script_mutations: ScriptMutationLimits,
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
            script_mutations: ScriptMutationLimits::DEFAULT,
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
    /// Exact final URL and authenticated/cleartext transport commitment.
    pub navigation_commit: NavigationCommitMetadata,
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

/// Monotone identity of one presentation scene compiled by an engine owner.
///
/// The identity is never a renderer epoch and carries no graphics authority.
/// It exists so the browser shell can reject a stale or cross-paired
/// presentation candidate before mapping it into a compositor-owned revision.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PresentationSceneRevision(NonZeroU64);

impl PresentationSceneRevision {
    /// Returns the diagnostic integer representation.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// Fixed-size metadata for one immutable presentation scene.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PresentationSceneMetadata {
    revision: PresentationSceneRevision,
    document_version: DocumentVersion,
    pipeline: PipelineKey,
    scene_items: usize,
    shaped_runs: usize,
    display_list_bytes: usize,
    retained_charge_bytes: usize,
}

impl PresentationSceneMetadata {
    /// Engine-owner monotone scene revision.
    #[must_use]
    pub const fn revision(self) -> PresentationSceneRevision {
        self.revision
    }

    /// Exact document identity and revision represented by the scene.
    #[must_use]
    pub const fn document_version(self) -> DocumentVersion {
        self.document_version
    }

    /// Renderer-independent page pipeline compiled into the scene.
    #[must_use]
    pub const fn pipeline(self) -> PipelineKey {
        self.pipeline
    }

    /// Validated immutable scene-item count.
    #[must_use]
    pub const fn scene_items(self) -> usize {
        self.scene_items
    }

    /// Exact number of canonical shaped page-text entries.
    #[must_use]
    pub const fn shaped_runs(self) -> usize {
        self.shaped_runs
    }

    /// Serialized pending-text display-list bytes owned by the scene.
    #[must_use]
    pub const fn display_list_bytes(self) -> usize {
        self.display_list_bytes
    }

    /// Deterministic conservative retained-resource charge used by the
    /// worker/session bounds.
    ///
    /// The charge includes serialized display-list data, retained scene/text
    /// structures, variation arrays, glyphs, clusters, strings, and each
    /// unique selected font blob. It deliberately overcharges allocator/Arc
    /// overhead and is not a claim about complete process RSS or GPU memory.
    #[must_use]
    pub const fn retained_charge_bytes(self) -> usize {
        self.retained_charge_bytes
    }
}

/// One exact renderer-neutral page scene prepared for native presentation.
///
/// The compiled display list and its canonical shaped-text inventory are
/// owned together and can be moved into a compositor only once. No headless
/// pixels are uploaded, copied, or relabelled as this scene.
pub struct PresentationScene {
    metadata: PresentationSceneMetadata,
    compiled: CompiledScene,
    shaped_text: Box<[ShapedSceneText]>,
}

impl fmt::Debug for PresentationScene {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PresentationScene")
            .field("metadata", &self.metadata)
            .finish_non_exhaustive()
    }
}

impl PresentationScene {
    /// Fixed metadata which remains comparable without graphics authority.
    #[must_use]
    pub const fn metadata(&self) -> PresentationSceneMetadata {
        self.metadata
    }

    /// Borrows the exact compiled scene for validation only.
    #[must_use]
    pub const fn compiled(&self) -> &CompiledScene {
        &self.compiled
    }

    /// Borrows the canonical shaped-text inventory for validation only.
    #[must_use]
    pub fn shaped_text(&self) -> &[ShapedSceneText] {
        &self.shaped_text
    }

    /// Consumes the inseparable presentation lease into compositor inputs.
    #[must_use]
    pub fn into_parts(self) -> (CompiledScene, Box<[ShapedSceneText]>) {
        (self.compiled, self.shaped_text)
    }
}

/// Successful navigation output for the presentation-only engine mode.
#[derive(Debug)]
pub struct RenderedPresentationPage {
    /// Stage counts proving the concrete fetch-to-compiled-scene path.
    pub evidence: PipelineEvidence,
    /// Text measurement and shaping counts.
    pub text: TextEvidence,
    /// Exact one-shot page scene; it has no headless RGBA8 representation.
    pub scene: PresentationScene,
}

/// Explicit cleanup reports from the text and `WebRender` owners.
#[derive(Debug)]
pub struct EngineShutdownReport {
    /// Headless renderer/backend/EGL cleanup evidence, absent in the explicit
    /// presentation-only mode which never constructs that renderer.
    pub renderer: Option<ShutdownReport>,
    /// Rust text-system cache cleanup evidence.
    pub text: TextShutdownReport,
}

/// Stateful Linux x86-64 static-page integration boundary.
pub struct StaticPageEngine {
    transport: PageTransport,
    parser_limits: TokenizerLimits,
    script_mutation_limits: ScriptMutationLimits,
    style_options: StaticStyleOptions,
    layout_limits: LayoutLimits,
    scene_compiler: SceneCompiler,
    renderer: PipelineRenderer,
    text: ShapingTextMeasurer,
    operation_timeout: Duration,
    next_epoch: u32,
    next_presentation_revision: u64,
    live_style_document: Option<StyleDocumentCurrentOwner>,
    live_document: Option<LiveDocumentPage>,
}

pub(crate) struct DetachedLiveDocument {
    pub(crate) style_owner: Option<StyleDocumentCurrentOwner>,
    pub(crate) page: LiveDocumentPage,
}

enum PipelineRenderer {
    Headless(Box<HeadlessRenderer>),
    PresentationOnly,
}

enum PageTransport {
    NumericLoopback(HttpClient),
    GeneralWeb(GeneralWebClient),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PageTransportKind {
    NumericLoopback,
    GeneralWeb,
}

enum PageTransportConfig {
    NumericLoopback,
    GeneralWeb {
        config: GeneralWebConfig,
        trust_store: TrustStore,
    },
}

struct FetchedDocument {
    http_status: u16,
    source: Vec<u8>,
    navigation_commit: NavigationCommitMetadata,
    response_metadata: UnboundDocumentResponseMetadata,
}

impl PipelineRenderer {
    const fn is_usable(&self) -> bool {
        match self {
            Self::Headless(renderer) => renderer.is_usable(),
            Self::PresentationOnly => true,
        }
    }
}

pub(crate) enum PipelineFrame {
    Headless(RgbaFrame),
    Presentation(Box<PresentationScene>),
}

impl StaticPageEngine {
    /// Initializes the bounded Rust text system and real Linux EGL/WebRender renderer.
    ///
    /// # Errors
    ///
    /// Returns a configuration, font-system, EGL, GL, or `WebRender` initialization error.
    pub fn new(config: StaticPageConfig) -> Result<Self, PipelineError> {
        Self::new_with_renderer(config, true, PageTransportConfig::NumericLoopback)
    }

    /// Initializes a headless page pipeline with the distinct authenticated
    /// general-web transport capability.
    ///
    /// This constructor does not weaken or replace [`Self::new`]: callers must
    /// deliberately provide general-web DNS/TLS policy and trust anchors.
    ///
    /// # Errors
    ///
    /// Returns a transport, configuration, font-system, EGL, GL, or
    /// `WebRender` initialization error.
    pub fn new_general_web(
        config: StaticPageConfig,
        general_web: GeneralWebConfig,
        trust_store: TrustStore,
    ) -> Result<Self, PipelineError> {
        Self::new_with_renderer(
            config,
            true,
            PageTransportConfig::GeneralWeb {
                config: general_web,
                trust_store,
            },
        )
    }

    /// Initializes the page pipeline without constructing the headless
    /// renderer, so each successful operation returns its exact immutable
    /// compiled scene for a native presenter.
    ///
    /// This is an explicit alternative to [`Self::new`]. A scene produced by
    /// this mode is never independently rendered headlessly or represented as
    /// RGBA8 pixels.
    ///
    /// # Errors
    ///
    /// Returns a configuration or font-system initialization error.
    pub fn new_for_presentation(config: StaticPageConfig) -> Result<Self, PipelineError> {
        Self::new_with_renderer(config, false, PageTransportConfig::NumericLoopback)
    }

    /// Initializes the presentation-only page pipeline with the distinct
    /// authenticated general-web transport capability.
    ///
    /// # Errors
    ///
    /// Returns a transport, configuration, or font-system initialization
    /// error.
    pub fn new_general_web_for_presentation(
        config: StaticPageConfig,
        general_web: GeneralWebConfig,
        trust_store: TrustStore,
    ) -> Result<Self, PipelineError> {
        Self::new_with_renderer(
            config,
            false,
            PageTransportConfig::GeneralWeb {
                config: general_web,
                trust_store,
            },
        )
    }

    fn new_with_renderer(
        config: StaticPageConfig,
        create_headless_renderer: bool,
        transport: PageTransportConfig,
    ) -> Result<Self, PipelineError> {
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

        let transport = match transport {
            PageTransportConfig::NumericLoopback => {
                PageTransport::NumericLoopback(HttpClient::new(config.network))
            }
            PageTransportConfig::GeneralWeb {
                config,
                trust_store,
            } => PageTransport::GeneralWeb(GeneralWebClient::new(config, trust_store)?),
        };
        let text = ShapingTextMeasurer::new(config.text, config.font_source)?;
        let renderer = if create_headless_renderer {
            let size = FrameSize::new(config.viewport_width, config.viewport_height)?;
            PipelineRenderer::Headless(Box::new(HeadlessRenderer::new(size, config.headless)?))
        } else {
            PipelineRenderer::PresentationOnly
        };
        Ok(Self {
            transport,
            parser_limits: config.parser,
            script_mutation_limits: config.script_mutations,
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
            next_presentation_revision: 1,
            live_style_document: None,
            live_document: None,
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
    pub fn load_with_deadline(
        &mut self,
        url: &str,
        cancellation: &CancellationToken,
        deadline: Instant,
    ) -> Result<RenderedStaticPage, PipelineError> {
        if !matches!(self.renderer, PipelineRenderer::Headless(_)) {
            return Err(PipelineError::InvalidConfiguration {
                field: "engine_output_mode",
                detail: "headless load requested from a presentation-only engine",
            });
        }
        let rendered = self.load_pipeline_with_deadline(
            url,
            cancellation,
            deadline,
            PageTransportKind::NumericLoopback,
        )?;
        let PipelineFrame::Headless(frame) = rendered.frame else {
            return Err(PipelineError::InvalidConfiguration {
                field: "engine_output_mode",
                detail: "presentation output crossed the headless API",
            });
        };
        Ok(RenderedStaticPage {
            evidence: rendered.evidence,
            text: rendered.text,
            frame,
        })
    }

    /// Fetches and renders one explicit HTTP(S) top-level document with the
    /// separately constructed general-web capability.
    ///
    /// # Errors
    ///
    /// Returns the same bounded pipeline failures as [`Self::load`], typed
    /// redirect target/chain failures, or a capability mismatch.
    pub fn load_general_web(
        &mut self,
        url: &str,
        cancellation: &CancellationToken,
    ) -> Result<RenderedStaticPage, PipelineError> {
        let deadline = Instant::now()
            .checked_add(self.operation_timeout)
            .ok_or(PipelineError::DeadlineOverflow)?;
        self.load_general_web_with_deadline(url, cancellation, deadline)
    }

    /// General-web form of [`Self::load_with_deadline`] using one caller-owned
    /// absolute deadline across DNS, TCP, TLS, body delivery, and rendering.
    ///
    /// # Errors
    ///
    /// Returns the same failures as [`Self::load_general_web`].
    pub fn load_general_web_with_deadline(
        &mut self,
        url: &str,
        cancellation: &CancellationToken,
        deadline: Instant,
    ) -> Result<RenderedStaticPage, PipelineError> {
        if !matches!(self.renderer, PipelineRenderer::Headless(_)) {
            return Err(PipelineError::InvalidConfiguration {
                field: "engine_output_mode",
                detail: "headless load requested from a presentation-only engine",
            });
        }
        let rendered = self.load_pipeline_with_deadline(
            url,
            cancellation,
            deadline,
            PageTransportKind::GeneralWeb,
        )?;
        let PipelineFrame::Headless(frame) = rendered.frame else {
            return Err(PipelineError::InvalidConfiguration {
                field: "engine_output_mode",
                detail: "presentation output crossed the headless API",
            });
        };
        Ok(RenderedStaticPage {
            evidence: rendered.evidence,
            text: rendered.text,
            frame,
        })
    }

    /// Fetches and compiles one page in the explicit presentation-only mode.
    ///
    /// # Errors
    ///
    /// Returns the same bounded fetch, document, style, layout, and shaping
    /// failures as [`Self::load`], or a mode mismatch for a headless engine.
    pub fn load_for_presentation(
        &mut self,
        url: &str,
        cancellation: &CancellationToken,
    ) -> Result<RenderedPresentationPage, PipelineError> {
        let deadline = Instant::now()
            .checked_add(self.operation_timeout)
            .ok_or(PipelineError::DeadlineOverflow)?;
        self.load_for_presentation_with_deadline(url, cancellation, deadline)
    }

    /// Compiles one presentation page with a caller-owned absolute deadline.
    ///
    /// # Errors
    ///
    /// Returns the same failures as [`Self::load_for_presentation`].
    pub fn load_for_presentation_with_deadline(
        &mut self,
        url: &str,
        cancellation: &CancellationToken,
        deadline: Instant,
    ) -> Result<RenderedPresentationPage, PipelineError> {
        if !matches!(self.renderer, PipelineRenderer::PresentationOnly) {
            return Err(PipelineError::InvalidConfiguration {
                field: "engine_output_mode",
                detail: "presentation load requested from a headless engine",
            });
        }
        let rendered = self.load_pipeline_with_deadline(
            url,
            cancellation,
            deadline,
            PageTransportKind::NumericLoopback,
        )?;
        let PipelineFrame::Presentation(scene) = rendered.frame else {
            return Err(PipelineError::InvalidConfiguration {
                field: "engine_output_mode",
                detail: "headless output crossed the presentation API",
            });
        };
        Ok(RenderedPresentationPage {
            evidence: rendered.evidence,
            text: rendered.text,
            scene: *scene,
        })
    }

    /// Compiles one explicit HTTP(S) document in presentation-only mode using
    /// the separately constructed general-web capability.
    ///
    /// # Errors
    ///
    /// Returns the same failures as [`Self::load_general_web`].
    pub fn load_general_web_for_presentation(
        &mut self,
        url: &str,
        cancellation: &CancellationToken,
    ) -> Result<RenderedPresentationPage, PipelineError> {
        let deadline = Instant::now()
            .checked_add(self.operation_timeout)
            .ok_or(PipelineError::DeadlineOverflow)?;
        self.load_general_web_for_presentation_with_deadline(url, cancellation, deadline)
    }

    /// Presentation-only general-web load with one caller-owned deadline.
    ///
    /// # Errors
    ///
    /// Returns the same failures as [`Self::load_general_web_for_presentation`].
    pub fn load_general_web_for_presentation_with_deadline(
        &mut self,
        url: &str,
        cancellation: &CancellationToken,
        deadline: Instant,
    ) -> Result<RenderedPresentationPage, PipelineError> {
        if !matches!(self.renderer, PipelineRenderer::PresentationOnly) {
            return Err(PipelineError::InvalidConfiguration {
                field: "engine_output_mode",
                detail: "presentation load requested from a headless engine",
            });
        }
        let rendered = self.load_pipeline_with_deadline(
            url,
            cancellation,
            deadline,
            PageTransportKind::GeneralWeb,
        )?;
        let PipelineFrame::Presentation(scene) = rendered.frame else {
            return Err(PipelineError::InvalidConfiguration {
                field: "engine_output_mode",
                detail: "headless output crossed the presentation API",
            });
        };
        Ok(RenderedPresentationPage {
            evidence: rendered.evidence,
            text: rendered.text,
            scene: *scene,
        })
    }

    fn load_pipeline_with_deadline(
        &mut self,
        url: &str,
        cancellation: &CancellationToken,
        deadline: Instant,
        transport: PageTransportKind,
    ) -> Result<RenderedPipelinePage, PipelineError> {
        if !self.renderer.is_usable() {
            return Err(HeadlessError::RendererUnusable.into());
        }
        self.preflight_presentation_revision()?;
        checkpoint(cancellation, deadline, PipelineStage::Fetch)?;

        let fetched = self.fetch_document(url, cancellation, deadline, transport)?;
        checkpoint(cancellation, deadline, PipelineStage::Parse)?;

        let html = std::str::from_utf8(&fetched.source).map_err(|_| PipelineError::NonUtf8Html)?;
        let mut parser = HtmlParser::new(self.parser_limits);
        parser.feed(html)?;
        let parsed = parser.finish()?;
        let html_diagnostics = parsed.errors.len();
        checkpoint(cancellation, deadline, PipelineStage::Snapshot)?;

        let snapshot = parsed.document.snapshot()?;
        let rendered = self.render_snapshot(&snapshot, cancellation, deadline)?;
        let document_version = rendered.evidence.document_version;
        let bound_navigation = fetched
            .navigation_commit
            .bind_document(document_version)
            .map_err(|_| DocumentPolicyError::BindingMismatch)?;
        let (navigation_commit, style_owner) = bound_navigation.into_parts();
        let response_metadata = fetched
            .response_metadata
            .bind(document_version, navigation_commit.clone());
        let result = RenderedPipelinePage {
            evidence: PipelineEvidence {
                document_version,
                http_status: fetched.http_status,
                navigation_commit,
                source_bytes: fetched.source.len(),
                dom_nodes: rendered.evidence.dom_nodes,
                html_diagnostics,
                stylo_style_entries: rendered.evidence.stylo_style_entries,
                style_diagnostics: rendered.evidence.style_diagnostics,
                dropped_style_diagnostics: rendered.evidence.dropped_style_diagnostics,
                layout_boxes: rendered.evidence.layout_boxes,
                layout_warnings: rendered.evidence.layout_warnings,
                scene_items: rendered.evidence.scene_items,
                pre_composition_display_list_bytes: rendered
                    .evidence
                    .pre_composition_display_list_bytes,
            },
            text: rendered.text,
            frame: rendered.frame,
        };
        let live_document =
            LiveDocumentPage::new(parsed.document, document_version, response_metadata)?;
        self.install_live_document(DetachedLiveDocument {
            style_owner,
            page: live_document,
        });
        Ok(result)
    }

    fn fetch_document(
        &self,
        url: &str,
        cancellation: &CancellationToken,
        deadline: Instant,
        requested: PageTransportKind,
    ) -> Result<FetchedDocument, PipelineError> {
        match (&self.transport, requested) {
            (PageTransport::NumericLoopback(client), PageTransportKind::NumericLoopback) => {
                let target = LoopbackTarget::parse(url)?;
                let navigation_commit = NavigationCommitMetadata::new(
                    target.url().as_str(),
                    0,
                    NavigationConnectionSecurity::Cleartext,
                    false,
                )
                .map_err(|_| {
                    PipelineError::RedirectLocation(RedirectLocationFailure::UrlTooLong)
                })?;
                let request = Request::get(target, RedirectPolicy::Reject)
                    .with_cancellation(cancellation.clone())
                    .with_deadline(deadline);
                let response = client
                    .execute(&request)
                    .map_err(|error| map_fetch_error(error, cancellation, deadline))?;
                checkpoint(cancellation, deadline, PipelineStage::Fetch)?;
                let http_status = response.head().status().as_u16();
                if !(200..=299).contains(&http_status) {
                    return Err(PipelineError::HttpStatus(http_status));
                }
                let response_metadata =
                    capture_document_response_metadata(response.head().headers())?;
                let source = response
                    .read_body_to_end()
                    .map_err(|error| map_fetch_error(error, cancellation, deadline))?;
                Ok(FetchedDocument {
                    http_status,
                    source,
                    navigation_commit,
                    response_metadata,
                })
            }
            (PageTransport::GeneralWeb(client), PageTransportKind::GeneralWeb) => {
                fetch_general_web_document(client, url, cancellation, deadline)
            }
            (PageTransport::NumericLoopback(_), PageTransportKind::GeneralWeb) => {
                Err(PipelineError::InvalidConfiguration {
                    field: "network_capability",
                    detail: "general-web load requested from a numeric-loopback engine",
                })
            }
            (PageTransport::GeneralWeb(_), PageTransportKind::NumericLoopback) => {
                Err(PipelineError::InvalidConfiguration {
                    field: "network_capability",
                    detail: "numeric-loopback load requested from a general-web engine",
                })
            }
        }
    }

    /// Returns the one mutable document retained by the latest successful load.
    ///
    /// The returned view exposes only read-only lookup operations. The document
    /// arena cannot be removed from its engine owner or mutated outside
    /// [`Self::apply_and_render`].
    #[must_use]
    pub const fn live_document(&self) -> Option<&LiveDocumentPage> {
        self.live_document.as_ref()
    }

    /// Delegates product stylesheet networking from one exact live navigation.
    ///
    /// The returned authority preserves this engine's unforgeable general-web
    /// client identity while replacing its transport limits with the fixed
    /// style-fetch profile. It is bound to the response's exact initial
    /// document revision and the supplied non-optional worker navigation. A
    /// direct, numeric-loopback, absent, replaced, mutated, or differently
    /// bound document cannot issue it.
    ///
    /// # Errors
    ///
    /// Returns a redacted typed error when no coherent committed general-web
    /// response/document authority exists or the requested transport policy
    /// would enlarge the hard style-fetch bounds.
    pub fn delegate_style_fetch_authority(
        &self,
        navigation: NavigationId,
        transport_policy: StyleFetchTransportPolicy,
    ) -> Result<StyleFetchAuthority, StyleFetchOwnerError> {
        let PageTransport::GeneralWeb(client) = &self.transport else {
            return Err(StyleFetchOwnerError::AuthorityUnavailable);
        };
        let page = self
            .live_document
            .as_ref()
            .ok_or(StyleFetchOwnerError::AuthorityUnavailable)?;
        let metadata = page.captured_response_metadata();
        let document_version = metadata.response_document_version();
        if page.live_version() != document_version {
            return Err(StyleFetchOwnerError::AuthorityUnavailable);
        }
        StyleFetchAuthority::from_committed_document(
            client,
            metadata.navigation_commit(),
            navigation,
            document_version,
            transport_policy,
        )
    }

    /// Delegates stylesheet networking for a direct non-product fixture.
    ///
    /// This separate authority is available only to a coherent committed
    /// general-web document which has never been bound to a worker
    /// [`NavigationId`]. It cannot be promoted into product authority.
    ///
    /// # Errors
    ///
    /// Returns a redacted typed error when no coherent direct document exists,
    /// the document is product-bound, its sole issuance was already consumed,
    /// or the requested transport policy exceeds the hard bounds.
    pub fn delegate_non_product_style_fetch_authority(
        &self,
        transport_policy: StyleFetchTransportPolicy,
    ) -> Result<NonProductStyleFetchAuthority, StyleFetchOwnerError> {
        let PageTransport::GeneralWeb(client) = &self.transport else {
            return Err(StyleFetchOwnerError::AuthorityUnavailable);
        };
        let page = self
            .live_document
            .as_ref()
            .ok_or(StyleFetchOwnerError::AuthorityUnavailable)?;
        let metadata = page.captured_response_metadata();
        let document_version = metadata.response_document_version();
        if page.live_version() != document_version {
            return Err(StyleFetchOwnerError::AuthorityUnavailable);
        }
        NonProductStyleFetchAuthority::from_committed_document(
            client,
            metadata.navigation_commit(),
            document_version,
            transport_policy,
        )
    }

    /// Exchanges the active live page for worker-private per-context storage.
    ///
    /// This remains crate-private so callers cannot detach the DOM arena or
    /// create two mutable owners. The navigation executor invokes it only on
    /// the renderer owner thread and leaves the engine empty between commands.
    pub(crate) fn replace_live_document(
        &mut self,
        replacement: Option<DetachedLiveDocument>,
    ) -> Option<DetachedLiveDocument> {
        let previous = self.live_document.take().map(|page| DetachedLiveDocument {
            style_owner: self.live_style_document.take(),
            page,
        });
        debug_assert!(
            self.live_style_document.is_none(),
            "style-document ownership cannot exist without its live page"
        );
        if let Some(replacement) = replacement {
            self.live_style_document = replacement.style_owner;
            self.live_document = Some(replacement.page);
        }
        previous
    }

    fn install_live_document(&mut self, replacement: DetachedLiveDocument) {
        if let Some(owner) = self.live_style_document.take() {
            owner.retire();
        }
        drop(self.live_document.take());
        self.live_style_document = replacement.style_owner;
        self.live_document = Some(replacement.page);
    }

    /// Whether another frame attempt may safely enter the owned renderer.
    ///
    /// `false` is terminal for this engine instance. The caller must tear the
    /// engine down and create a replacement rather than attempting repair.
    #[must_use]
    pub const fn renderer_is_usable(&self) -> bool {
        self.renderer.is_usable()
    }

    /// Applies one exact-version bounded DOM batch, fully recomputes style,
    /// layout and shaped text, then returns one composed frame.
    ///
    /// Mutation rejection is atomic and leaves both tracked versions
    /// unchanged. Once the DOM batch commits, a downstream failure cannot be
    /// rolled back: the returned error carries the advanced live version and
    /// created-node map while identifying the revision represented by the last
    /// frame this engine returned. It makes no claim about rollback of the
    /// renderer's internal surface after a post-send failure.
    ///
    /// This synchronous seam performs no network request, script execution,
    /// event-loop dispatch, or incremental style invalidation.
    ///
    /// # Errors
    ///
    /// Returns [`DocumentUpdateError::Rejected`] before the DOM commit point or
    /// [`DocumentUpdateError::Committed`] after an irreversible DOM commit.
    pub fn apply_and_render(
        &mut self,
        batch: ScriptMutationBatch,
        cancellation: &CancellationToken,
    ) -> Result<RenderedDocumentUpdate, DocumentUpdateError> {
        let versions = self.dynamic_owner_state()?;
        let deadline = Instant::now()
            .checked_add(self.operation_timeout)
            .ok_or(PipelineError::DeadlineOverflow)
            .map_err(|error| {
                rejected_update_for_versions(
                    Some(versions),
                    DocumentUpdateRejection::Pipeline(error),
                )
            })?;
        self.apply_and_render_with_deadline(batch, cancellation, deadline)
    }

    /// Applies and renders one batch with a caller-owned absolute deadline.
    ///
    /// # Errors
    ///
    /// Returns the same two-phase failures as [`Self::apply_and_render`].
    pub fn apply_and_render_with_deadline(
        &mut self,
        batch: ScriptMutationBatch,
        cancellation: &CancellationToken,
        deadline: Instant,
    ) -> Result<RenderedDocumentUpdate, DocumentUpdateError> {
        if !matches!(self.renderer, PipelineRenderer::Headless(_)) {
            return Err(rejected_update_for_versions(
                self.dynamic_owner_state().ok(),
                DocumentUpdateRejection::Pipeline(PipelineError::InvalidConfiguration {
                    field: "engine_output_mode",
                    detail: "headless mutation rendering requested from a presentation-only engine",
                }),
            ));
        }
        let rendered = self.apply_pipeline_with_deadline(batch, cancellation, deadline)?;
        let PipelineFrame::Headless(frame) = rendered.frame else {
            return Err(DocumentUpdateError::Committed {
                previous_live_version: rendered.previous_live_version,
                last_returned_frame_version: rendered.previous_last_returned_frame_version,
                commit: rendered.commit,
                source: Box::new(PipelineError::InvalidConfiguration {
                    field: "engine_output_mode",
                    detail: "presentation output crossed the headless mutation API",
                }),
            });
        };
        Ok(RenderedDocumentUpdate::new(
            rendered.previous_live_version,
            rendered.previous_last_returned_frame_version,
            rendered.evidence,
            rendered.text,
            frame,
            rendered.commit,
        ))
    }

    pub(crate) fn apply_for_navigation(
        &mut self,
        batch: ScriptMutationBatch,
        cancellation: &CancellationToken,
    ) -> Result<RenderedNavigationUpdate, DocumentUpdateError> {
        let versions = self.dynamic_owner_state()?;
        let deadline = Instant::now()
            .checked_add(self.operation_timeout)
            .ok_or(PipelineError::DeadlineOverflow)
            .map_err(|error| {
                rejected_update_for_versions(
                    Some(versions),
                    DocumentUpdateRejection::Pipeline(error),
                )
            })?;
        self.apply_pipeline_with_deadline(batch, cancellation, deadline)
    }

    fn apply_pipeline_with_deadline(
        &mut self,
        batch: ScriptMutationBatch,
        cancellation: &CancellationToken,
        deadline: Instant,
    ) -> Result<RenderedNavigationUpdate, DocumentUpdateError> {
        let (previous_live_version, previous_last_returned_frame_version) =
            self.dynamic_preflight(cancellation, deadline)?;
        let style_owner = self.live_style_document.take();
        let mutation = {
            let Some(page) = self.live_document.as_mut() else {
                self.live_style_document = style_owner;
                return Err(rejected_update_for_versions(
                    None,
                    DocumentUpdateRejection::NoLiveDocument,
                ));
            };
            let apply = || {
                page.document
                    .apply_script_mutations(batch, self.script_mutation_limits)
            };
            match style_owner.as_ref() {
                Some(owner) => owner.retire_if_succeeded(apply),
                None => Some(apply()),
            }
        };
        let commit = match mutation {
            Some(Ok(commit)) => commit,
            Some(Err(error)) => {
                self.live_style_document = style_owner;
                return Err(rejected_update_for_versions(
                    Some((previous_live_version, previous_last_returned_frame_version)),
                    DocumentUpdateRejection::Mutation(error),
                ));
            }
            None => {
                return Err(rejected_update_for_versions(
                    Some((previous_live_version, previous_last_returned_frame_version)),
                    DocumentUpdateRejection::NoLiveDocument,
                ));
            }
        };
        drop(style_owner);
        let rendered = match self.render_snapshot(commit.snapshot(), cancellation, deadline) {
            Ok(rendered) => rendered,
            Err(source) => {
                return Err(DocumentUpdateError::Committed {
                    previous_live_version,
                    last_returned_frame_version: previous_last_returned_frame_version,
                    commit: crate::DocumentMutationCommit::from_script_commit(commit),
                    source: Box::new(source),
                });
            }
        };
        self.record_live_frame_returned();
        let commit = crate::DocumentMutationCommit::from_script_commit(commit);

        Ok(RenderedNavigationUpdate {
            previous_live_version,
            previous_last_returned_frame_version,
            evidence: rendered.evidence,
            text: rendered.text,
            frame: rendered.frame,
            commit,
        })
    }

    /// Recomputes and returns a fresh frame for one exact live DOM revision.
    ///
    /// This performs no fetch, parse, DOM mutation, created-node mapping, or
    /// revision increment. Both control failures and downstream render failures
    /// are reported as [`DocumentUpdateError::Rejected`] because this call
    /// commits no DOM mutation.
    ///
    /// # Errors
    ///
    /// Returns an exact rejection for no live document, an unusable renderer,
    /// cancellation/deadline construction, a stale expected version, snapshot,
    /// or any downstream pipeline failure.
    pub fn rerender_live(
        &mut self,
        expected_live_version: DocumentVersion,
        cancellation: &CancellationToken,
    ) -> Result<RenderedLiveDocument, DocumentUpdateError> {
        let versions = self.dynamic_owner_state()?;
        let deadline = Instant::now()
            .checked_add(self.operation_timeout)
            .ok_or(PipelineError::DeadlineOverflow)
            .map_err(|error| {
                rejected_update_for_versions(
                    Some(versions),
                    DocumentUpdateRejection::Pipeline(error),
                )
            })?;
        self.rerender_live_with_deadline(expected_live_version, cancellation, deadline)
    }

    /// Recomputes the current exact live revision with a caller-owned deadline.
    ///
    /// # Errors
    ///
    /// Returns the same no-mutation rejections as [`Self::rerender_live`].
    pub fn rerender_live_with_deadline(
        &mut self,
        expected_live_version: DocumentVersion,
        cancellation: &CancellationToken,
        deadline: Instant,
    ) -> Result<RenderedLiveDocument, DocumentUpdateError> {
        if !matches!(self.renderer, PipelineRenderer::Headless(_)) {
            return Err(rejected_update_for_versions(
                self.dynamic_owner_state().ok(),
                DocumentUpdateRejection::Pipeline(PipelineError::InvalidConfiguration {
                    field: "engine_output_mode",
                    detail: "headless rerender requested from a presentation-only engine",
                }),
            ));
        }
        let rendered =
            self.rerender_pipeline_with_deadline(expected_live_version, cancellation, deadline)?;
        let PipelineFrame::Headless(frame) = rendered.frame else {
            return Err(rejected_update_for_versions(
                Some((
                    rendered.evidence.document_version,
                    rendered.previous_last_returned_frame_version,
                )),
                DocumentUpdateRejection::Pipeline(PipelineError::InvalidConfiguration {
                    field: "engine_output_mode",
                    detail: "presentation output crossed the headless rerender API",
                }),
            ));
        };
        Ok(RenderedLiveDocument {
            previous_last_returned_frame_version: rendered.previous_last_returned_frame_version,
            evidence: rendered.evidence,
            text: rendered.text,
            frame,
        })
    }

    pub(crate) fn rerender_for_navigation(
        &mut self,
        expected_live_version: DocumentVersion,
        cancellation: &CancellationToken,
    ) -> Result<RenderedNavigationRerender, DocumentUpdateError> {
        let versions = self.dynamic_owner_state()?;
        let deadline = Instant::now()
            .checked_add(self.operation_timeout)
            .ok_or(PipelineError::DeadlineOverflow)
            .map_err(|error| {
                rejected_update_for_versions(
                    Some(versions),
                    DocumentUpdateRejection::Pipeline(error),
                )
            })?;
        self.rerender_pipeline_with_deadline(expected_live_version, cancellation, deadline)
    }

    fn rerender_pipeline_with_deadline(
        &mut self,
        expected_live_version: DocumentVersion,
        cancellation: &CancellationToken,
        deadline: Instant,
    ) -> Result<RenderedNavigationRerender, DocumentUpdateError> {
        let (live_version, previous_last_returned_frame_version) =
            self.dynamic_preflight(cancellation, deadline)?;
        if expected_live_version != live_version {
            return Err(rejected_update_for_versions(
                Some((live_version, previous_last_returned_frame_version)),
                DocumentUpdateRejection::LiveVersionMismatch {
                    expected: expected_live_version,
                    actual: live_version,
                },
            ));
        }

        let snapshot = self
            .live_document
            .as_ref()
            .map(|page| page.document.snapshot())
            .transpose()
            .map_err(|error| {
                rejected_update_for_versions(
                    Some((live_version, previous_last_returned_frame_version)),
                    DocumentUpdateRejection::Pipeline(error.into()),
                )
            })?;
        let Some(snapshot) = snapshot else {
            return Err(rejected_update_for_versions(
                None,
                DocumentUpdateRejection::NoLiveDocument,
            ));
        };
        let rendered = self
            .render_snapshot(&snapshot, cancellation, deadline)
            .map_err(|error| {
                rejected_update_for_versions(
                    Some((live_version, previous_last_returned_frame_version)),
                    DocumentUpdateRejection::Pipeline(error),
                )
            })?;
        self.record_live_frame_returned();

        Ok(RenderedNavigationRerender {
            previous_last_returned_frame_version,
            evidence: rendered.evidence,
            text: rendered.text,
            frame: rendered.frame,
        })
    }

    #[allow(clippy::too_many_lines)]
    fn render_snapshot(
        &mut self,
        snapshot: &DocumentSnapshot,
        cancellation: &CancellationToken,
        deadline: Instant,
    ) -> Result<RenderedSnapshot, PipelineError> {
        let dom_nodes = snapshot.nodes_in_document_order().len();
        checkpoint(cancellation, deadline, PipelineStage::Style)?;

        let stylo = prepare_computed_styles((*snapshot).clone(), self.style_options)?;
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
            snapshot,
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

        let presentation_only = matches!(self.renderer, PipelineRenderer::PresentationOnly);
        let reserved_revision = if presentation_only {
            Some(self.reserve_presentation_revision()?)
        } else {
            None
        };
        let reserved_epoch = if presentation_only {
            None
        } else {
            Some(self.reserve_epoch()?)
        };
        let frame = match &mut self.renderer {
            PipelineRenderer::Headless(renderer) => {
                let epoch = reserved_epoch.ok_or(PipelineError::InvalidConfiguration {
                    field: "engine_output_mode",
                    detail: "headless renderer omitted its reserved epoch",
                })?;
                PipelineFrame::Headless(renderer.render_composed(
                    compiled,
                    &shaped.entries,
                    FrameRequest::new(document_version, epoch),
                )?)
            }
            PipelineRenderer::PresentationOnly => {
                let revision = reserved_revision.ok_or(PipelineError::InvalidConfiguration {
                    field: "engine_output_mode",
                    detail: "presentation renderer omitted its reserved revision",
                })?;
                let display_list_bytes = compiled.built_display_list().size_in_bytes();
                let retained_charge_bytes = presentation_retained_charge(&compiled, &shaped)?;
                let metadata = PresentationSceneMetadata {
                    revision,
                    document_version,
                    pipeline: compiled.pipeline(),
                    scene_items,
                    shaped_runs: shaped.run_count,
                    display_list_bytes,
                    retained_charge_bytes,
                };
                PipelineFrame::Presentation(Box::new(PresentationScene {
                    metadata,
                    compiled,
                    shaped_text: shaped.entries.into_boxed_slice(),
                }))
            }
        };

        Ok(RenderedSnapshot {
            evidence: DynamicRenderEvidence {
                document_version,
                dom_nodes,
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

    fn dynamic_owner_state(
        &self,
    ) -> Result<(DocumentVersion, DocumentVersion), DocumentUpdateError> {
        validate_dynamic_owner(
            self.live_document
                .as_ref()
                .map(|page| (page.live_version(), page.last_returned_frame_version())),
            self.renderer.is_usable(),
        )
    }

    fn record_live_frame_returned(&mut self) {
        if let Some(page) = self.live_document.as_mut() {
            page.last_returned_frame_version = page.live_version();
        }
    }

    fn dynamic_preflight(
        &self,
        cancellation: &CancellationToken,
        deadline: Instant,
    ) -> Result<(DocumentVersion, DocumentVersion), DocumentUpdateError> {
        let versions = self.dynamic_owner_state()?;
        self.preflight_presentation_revision().map_err(|error| {
            rejected_update_for_versions(Some(versions), DocumentUpdateRejection::Pipeline(error))
        })?;
        checkpoint(cancellation, deadline, PipelineStage::Snapshot).map_err(|error| {
            rejected_update_for_versions(Some(versions), DocumentUpdateRejection::Pipeline(error))
        })?;
        Ok(versions)
    }

    /// Explicitly tears down WebRender/EGL and releases text caches.
    ///
    /// # Errors
    ///
    /// Returns a bounded renderer shutdown failure after local cleanup has still run.
    pub fn shutdown(mut self) -> Result<EngineShutdownReport, PipelineError> {
        if let Some(owner) = self.live_style_document.take() {
            owner.retire();
        }
        let Self { renderer, text, .. } = self;
        let text = text.shutdown();
        let renderer = match renderer {
            PipelineRenderer::Headless(renderer) => Some((*renderer).shutdown()?),
            PipelineRenderer::PresentationOnly => None,
        };
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

    fn reserve_presentation_revision(
        &mut self,
    ) -> Result<PresentationSceneRevision, PipelineError> {
        let current = self.next_presentation_revision;
        let next = current
            .checked_add(1)
            .ok_or(PipelineError::PresentationRevisionExhausted)?;
        let revision = PresentationSceneRevision(
            NonZeroU64::new(current).ok_or(PipelineError::PresentationRevisionExhausted)?,
        );
        self.next_presentation_revision = next;
        Ok(revision)
    }

    fn preflight_presentation_revision(&self) -> Result<(), PipelineError> {
        if matches!(self.renderer, PipelineRenderer::PresentationOnly)
            && (self.next_presentation_revision == 0
                || self.next_presentation_revision.checked_add(1).is_none())
        {
            Err(PipelineError::PresentationRevisionExhausted)
        } else {
            Ok(())
        }
    }
}

fn validate_dynamic_owner(
    versions: Option<(DocumentVersion, DocumentVersion)>,
    renderer_is_usable: bool,
) -> Result<(DocumentVersion, DocumentVersion), DocumentUpdateError> {
    let Some(versions) = versions else {
        return Err(rejected_update_for_versions(
            None,
            DocumentUpdateRejection::NoLiveDocument,
        ));
    };
    if !renderer_is_usable {
        return Err(rejected_update_for_versions(
            Some(versions),
            DocumentUpdateRejection::RendererUnavailable,
        ));
    }
    Ok(versions)
}

fn rejected_update_for_versions(
    versions: Option<(DocumentVersion, DocumentVersion)>,
    reason: DocumentUpdateRejection,
) -> DocumentUpdateError {
    let (live_version, last_returned_frame_version) = versions
        .map_or((None, None), |(live, last_returned)| {
            (Some(live), Some(last_returned))
        });
    DocumentUpdateError::Rejected {
        live_version,
        last_returned_frame_version,
        reason,
    }
}

struct RenderedSnapshot {
    evidence: DynamicRenderEvidence,
    text: TextEvidence,
    frame: PipelineFrame,
}

fn fetch_general_web_document(
    client: &GeneralWebClient,
    url: &str,
    cancellation: &CancellationToken,
    deadline: Instant,
) -> Result<FetchedDocument, PipelineError> {
    let (mut identity, mut target) = GeneralWebTarget::parse_navigation(url)?;
    ensure_navigation_url_bound(identity.as_str())?;
    let mut visited = BTreeSet::new();
    visited.insert(Box::<str>::from(identity.as_str()));
    let mut redirect_count = 0u8;
    let mut saw_authenticated_https = false;
    let mut had_https_downgrade = false;
    let access = browser_access(client);

    loop {
        checkpoint(cancellation, deadline, PipelineStage::Fetch)?;
        let response = execute_web_get(client, &target, &access, cancellation, deadline)?;
        let security = project_navigation_security(target.origin().scheme(), response.security())?;
        if matches!(
            security,
            NavigationConnectionSecurity::AuthenticatedTls { .. }
        ) {
            saw_authenticated_https = true;
        }
        let status = response.head().status();
        let http_status = status.as_u16();

        if status.is_redirect() {
            let location = {
                let mut locations = response.head().headers().values("location");
                let value = locations.next().ok_or(PipelineError::RedirectLocation(
                    RedirectLocationFailure::Missing,
                ))?;
                if locations.next().is_some() {
                    return Err(PipelineError::RedirectLocation(
                        RedirectLocationFailure::Multiple,
                    ));
                }
                value
                    .to_str()
                    .ok_or(PipelineError::RedirectLocation(
                        RedirectLocationFailure::NonUtf8,
                    ))?
                    .trim_matches([' ', '\t'])
                    .to_owned()
            };
            let inherited_fragment = identity.fragment().map(str::to_owned);
            let mut resolved_identity = identity
                .join(&location)
                .map_err(|_| PipelineError::RedirectLocation(RedirectLocationFailure::Invalid))?;
            if resolved_identity.fragment().is_none() {
                resolved_identity.set_fragment(inherited_fragment.as_deref());
            }
            ensure_navigation_url_bound(resolved_identity.as_str())?;
            // A fragment is browser-navigation identity but is never sent in
            // an HTTP request target. Validate and fetch the otherwise exact
            // URL through the transport's intentionally fragment-free type.
            let (resolved_identity, next) =
                GeneralWebTarget::from_navigation_url(resolved_identity)
                    .map_err(|error| map_redirect_target_error(&error))?;
            if visited.contains(resolved_identity.as_str()) {
                return Err(PipelineError::RedirectLoop);
            }
            if redirect_count >= MAX_TOP_LEVEL_REDIRECTS {
                return Err(PipelineError::TooManyRedirects {
                    maximum: MAX_TOP_LEVEL_REDIRECTS,
                });
            }
            redirect_count =
                redirect_count
                    .checked_add(1)
                    .ok_or(PipelineError::TooManyRedirects {
                        maximum: MAX_TOP_LEVEL_REDIRECTS,
                    })?;
            if saw_authenticated_https && next.origin().scheme() == WebScheme::Http {
                had_https_downgrade = true;
            }
            visited.insert(Box::<str>::from(resolved_identity.as_str()));
            identity = resolved_identity;
            target = next;
            continue;
        }

        if (300..=399).contains(&http_status) {
            return Err(PipelineError::UnsupportedRedirectStatus {
                status: http_status,
            });
        }
        if !(200..=299).contains(&http_status) {
            return Err(PipelineError::HttpStatus(http_status));
        }
        let navigation_commit = NavigationCommitMetadata::from_general_web_response(
            identity.as_str(),
            redirect_count,
            had_https_downgrade,
            &response,
        )
        .map_err(|_| DocumentPolicyError::BindingMismatch)?;
        let response_metadata = capture_document_response_metadata(response.head().headers())?;
        let source = response
            .read_body_to_end()
            .map_err(|error| map_fetch_error(error, cancellation, deadline))?;
        return Ok(FetchedDocument {
            http_status,
            source,
            navigation_commit,
            response_metadata,
        });
    }
}

fn browser_access(client: &GeneralWebClient) -> GeneralWebNetworkAccess {
    client.browser_navigation_network_access(LocalNetworkAccessPermissions::deny_all())
}

fn execute_web_get(
    client: &GeneralWebClient,
    target: &GeneralWebTarget,
    network_access: &GeneralWebNetworkAccess,
    cancellation: &CancellationToken,
    deadline: Instant,
) -> Result<GeneralWebResponse, PipelineError> {
    let request = GeneralWebRequest::get_with_network_access(
        target.clone(),
        RedirectPolicy::Manual,
        network_access.clone(),
    )
    .with_cancellation(cancellation.clone())
    .with_deadline(deadline);
    let response = client
        .execute(&request)
        .map_err(|error| map_fetch_error(error, cancellation, deadline))?;
    checkpoint(cancellation, deadline, PipelineStage::Fetch)?;
    Ok(response)
}

fn ensure_navigation_url_bound(url: &str) -> Result<(), PipelineError> {
    if url.is_empty() || url.len() > crate::MAX_NAVIGATION_URL_BYTES {
        Err(PipelineError::RedirectLocation(
            RedirectLocationFailure::UrlTooLong,
        ))
    } else {
        Ok(())
    }
}

fn map_redirect_target_error(error: &NetworkError) -> PipelineError {
    let failure = match error {
        NetworkError::CredentialsNotAllowed => RedirectLocationFailure::CredentialsNotAllowed,
        NetworkError::UnsupportedScheme(_) => RedirectLocationFailure::UnsupportedScheme,
        NetworkError::LimitExceeded {
            kind: NetworkLimitKind::UrlBytes,
            ..
        } => RedirectLocationFailure::UrlTooLong,
        // Redirect identity fragments are stripped before this transport
        // validation, so FragmentNotAllowed is an unreachable
        // defense-in-depth invalid-target case together with the remainder.
        _ => RedirectLocationFailure::Invalid,
    };
    PipelineError::RedirectLocation(failure)
}

fn project_navigation_security(
    scheme: WebScheme,
    security: ConnectionSecurity,
) -> Result<NavigationConnectionSecurity, PipelineError> {
    match (scheme, security) {
        (WebScheme::Http, ConnectionSecurity::Cleartext) => {
            Ok(NavigationConnectionSecurity::Cleartext)
        }
        (WebScheme::Https, ConnectionSecurity::Tls { version, alpn }) => {
            let version = match version {
                TlsVersion::Tls12 => NavigationTlsVersion::Tls12,
                TlsVersion::Tls13 => NavigationTlsVersion::Tls13,
            };
            let alpn = match alpn {
                AlpnOutcome::Http11 => NavigationAlpn::Http11,
                AlpnOutcome::NotNegotiated => NavigationAlpn::NotNegotiated,
            };
            Ok(NavigationConnectionSecurity::AuthenticatedTls { version, alpn })
        }
        (WebScheme::Http, ConnectionSecurity::Tls { .. })
        | (WebScheme::Https, ConnectionSecurity::Cleartext) => {
            Err(PipelineError::TransportSecurityMismatch)
        }
    }
}

struct RenderedPipelinePage {
    evidence: PipelineEvidence,
    text: TextEvidence,
    frame: PipelineFrame,
}

pub(crate) struct RenderedNavigationUpdate {
    pub(crate) previous_live_version: DocumentVersion,
    pub(crate) previous_last_returned_frame_version: DocumentVersion,
    pub(crate) evidence: DynamicRenderEvidence,
    pub(crate) text: TextEvidence,
    pub(crate) frame: PipelineFrame,
    pub(crate) commit: crate::DocumentMutationCommit,
}

pub(crate) struct RenderedNavigationRerender {
    pub(crate) previous_last_returned_frame_version: DocumentVersion,
    pub(crate) evidence: DynamicRenderEvidence,
    pub(crate) text: TextEvidence,
    pub(crate) frame: PipelineFrame,
}

struct ShapedPendingRuns {
    run_count: usize,
    glyphs: usize,
    clusters: usize,
    entries: Vec<ShapedSceneText>,
}

fn presentation_retained_charge(
    compiled: &CompiledScene,
    shaped: &ShapedPendingRuns,
) -> Result<usize, PipelineError> {
    const ALLOCATION_OVERHEAD: usize = size_of::<usize>() * 4;
    const ARC_OVERHEAD: usize = size_of::<usize>() * 4;

    fn add(total: &mut usize, additional: usize) -> Result<(), PipelineError> {
        *total = total
            .checked_add(additional)
            .ok_or(PipelineError::EvidenceOverflow)?;
        Ok(())
    }

    let mut total = size_of::<CompiledScene>();
    add(&mut total, compiled.built_display_list().size_in_bytes())?;
    add(&mut total, size_of_val(compiled.scene().items()))?;
    add(&mut total, size_of_val(compiled.scene().pending_text()))?;
    for pending in compiled.scene().pending_text() {
        add(&mut total, pending.text().len())?;
        add(&mut total, ALLOCATION_OVERHEAD)?;
    }
    add(&mut total, size_of_val(shaped.entries.as_slice()))?;

    let mut shaped_allocations = BTreeSet::new();
    let mut font_blobs = BTreeSet::new();
    for entry in &shaped.entries {
        let shaped_text = entry.shaped();
        let allocation = Arc::as_ptr(shaped_text).cast::<()>() as usize;
        if !shaped_allocations.insert(allocation) {
            continue;
        }
        add(&mut total, size_of::<ShapedText>())?;
        add(&mut total, ARC_OVERHEAD)?;
        add(&mut total, shaped_text.text().len())?;
        add(&mut total, ALLOCATION_OVERHEAD)?;
        add(&mut total, size_of_val(shaped_text.runs()))?;
        add(&mut total, ALLOCATION_OVERHEAD)?;
        for run in shaped_text.runs() {
            add(
                &mut total,
                size_of_val(run.normalized_variation_coordinates()),
            )?;
            add(&mut total, ALLOCATION_OVERHEAD)?;
            add(&mut total, size_of_val(run.glyphs()))?;
            add(&mut total, ALLOCATION_OVERHEAD)?;
            add(&mut total, size_of_val(run.clusters()))?;
            add(&mut total, ALLOCATION_OVERHEAD)?;
            let blob_id = run.face().id().blob_id();
            if font_blobs.insert(blob_id) {
                add(&mut total, run.face().bytes().len())?;
                add(&mut total, ARC_OVERHEAD)?;
            }
        }
    }
    add(
        &mut total,
        shaped_allocations
            .len()
            .checked_add(font_blobs.len())
            .and_then(|entries| entries.checked_mul(size_of::<usize>() * 4))
            .ok_or(PipelineError::EvidenceOverflow)?,
    )?;
    Ok(total.max(1))
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

fn map_fetch_error(
    error: wild_buzzard_net::Error,
    cancellation: &CancellationToken,
    deadline: Instant,
) -> PipelineError {
    if cancellation.is_cancelled() {
        PipelineError::Cancelled {
            stage: PipelineStage::Fetch,
        }
    } else if Instant::now() >= deadline {
        PipelineError::DeadlineExceeded {
            stage: PipelineStage::Fetch,
        }
    } else {
        PipelineError::Network(error)
    }
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
    fn unusable_renderer_preflight_rejects_before_mutation_state_can_advance() {
        let initial = wild_buzzard_dom::Document::new().version();
        let live = DocumentVersion::new(initial.document_id(), initial.revision() + 1);
        let error = validate_dynamic_owner(Some((live, initial)), false).unwrap_err();

        assert!(matches!(
            error,
            DocumentUpdateError::Rejected {
                live_version: Some(actual_live),
                last_returned_frame_version: Some(last_returned),
                reason: DocumentUpdateRejection::RendererUnavailable,
            } if actual_live == live && last_returned == initial
        ));
        assert!(matches!(
            validate_dynamic_owner(None, false),
            Err(DocumentUpdateError::Rejected {
                live_version: None,
                last_returned_frame_version: None,
                reason: DocumentUpdateRejection::NoLiveDocument,
            })
        ));
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
