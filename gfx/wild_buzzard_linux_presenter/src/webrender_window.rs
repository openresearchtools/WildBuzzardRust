#![forbid(unsafe_code)]

use std::any::Any;
use std::mem;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::process;
use std::time::Instant;

use webrender::{
    PipelineInfo, RenderApi, Renderer, RendererError, Transaction, WebRenderOptions,
    create_webrender_instance,
};
use webrender_api::units::{DeviceIntRect, DeviceIntSize};
use webrender_api::{Checkpoint, ColorF, DocumentId as WebRenderDocumentId, Epoch, PipelineId};
use webrender_api::{MAX_RENDER_TASK_SIZE, RenderReasons};
use wild_buzzard_dom::{Document, DocumentVersion};
use wild_buzzard_platform::{LogicalRect, PhysicalPoint, PhysicalSize, ScaleFactor};
use wild_buzzard_renderer::{
    CompiledScene, PipelineKey, SceneBuildError, SceneTextDescriptor, SceneTextMetrics,
};
use wild_buzzard_text_webrender::{
    RegistryRelease, ShapedSceneText, TextFontRegistry, TextRegistryStatistics, TextRenderError,
};

use crate::browser_compositor::{
    BrowserCandidate, BrowserChromeScene, BrowserCompositorContract, BrowserFrameAccounting,
    BrowserFrameReceipt, BrowserFrameRequest, BrowserHitTestResult, BrowserPageScene,
    BrowserPageSnapshot, BrowserPageUpdate, BrowserPipelines, build_browser_chrome_display_list,
    build_browser_root_display_list, stage_browser_texts,
};
use crate::contract::{
    DirectFrameRequest, LinuxAccelerationClass, LinuxPresentationCapabilities, PresentationError,
    PresentationErrorKind, PresentationFailureStage, PresentationTeardownOutcome,
};
use crate::egl_window::{LinuxPresentedWindow, NativeExtentConfirmation};
use crate::window_contract::{
    WebRenderSurfaceSnapshot, WebRenderTeardownEvidence, WebRenderWindowContract,
    WebRenderWindowError, WebRenderWindowErrorKind, WebRenderWindowFailureStage,
    WebRenderWindowFrameReceipt, WebRenderWindowFrameRequest, WebRenderWindowLimits,
    WebRenderWindowResizeRequest, WebRenderWindowShutdownFailure, WebRenderWindowShutdownReport,
    WebRenderWindowStartupFailure, WebRenderWindowState, presentation_outcome,
    presentation_retention_error,
};
use crate::window_notifier::{
    FrameReadyEvidence, NotificationWaitError, WindowRenderNotifier, WindowStageWaiter,
};

const APP_UNITS_PER_CSS_PIXEL: i32 = 60;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CompletedBrowserFrame {
    backend_publish_id: u64,
    rgba8_byte_equivalent: u64,
}

/// Same-thread owner of `WebRender` nested inside one exact Linux EGL presenter.
///
/// The owner never exposes GL, EGL, Wayland, X11, or winit authority. A frame
/// is built from the existing immutable Wild Buzzard scene/text contracts and
/// is rendered directly into the presenter's native default framebuffer.
///
/// ```compile_fail
/// fn assert_send<T: Send>() {}
/// assert_send::<wild_buzzard_linux_presenter::WebRenderPresentedWindow>();
/// ```
pub struct WebRenderPresentedWindow {
    presenter: Option<LinuxPresentedWindow>,
    renderer: Option<Renderer>,
    api: Option<RenderApi>,
    document_id: WebRenderDocumentId,
    notifier: WindowRenderNotifier,
    text_registry: TextFontRegistry,
    browser_pipelines: BrowserPipelines,
    browser_contract: BrowserCompositorContract,
    browser_resource_document: DocumentVersion,
    contract: WebRenderWindowContract,
    active_stage: WebRenderWindowFailureStage,
    backend_shutdown_evidence: WebRenderTeardownEvidence,
    renderer_deinitialization_evidence: WebRenderTeardownEvidence,
    shutdown_complete: bool,
}

impl LinuxPresentedWindow {
    /// Consumes this exact native presenter and creates `WebRender` on its current
    /// GL context and window surface.
    ///
    /// Initialization failure retires the consumed presenter exactly once. The
    /// error retains both the primary `WebRender` failure and the independent
    /// ordered teardown result.
    ///
    /// # Errors
    ///
    /// Rejects a suspended/zero-sized presenter and reports recoverable native,
    /// GL, pre-worker `WebRender`, or provably ordered partial-owner teardown
    /// failures.
    ///
    /// # Process termination
    ///
    /// Aborts the owning process after a constructor thread error, constructor
    /// panic, or API-creation panic. The imported constructor exposes no worker
    /// join guard capable of proving cleanup after those stages.
    pub fn into_webrender(self) -> Result<WebRenderPresentedWindow, WebRenderWindowStartupFailure> {
        WebRenderPresentedWindow::new(self)
    }

    /// Consumes this exact native presenter into the renderer-owned browser
    /// compositor. The returned owner is the same capability-safe `WebRender`
    /// owner used by the legacy single-scene path; no second renderer or
    /// native surface is constructed.
    ///
    /// # Errors
    ///
    /// Returns the same typed initialization and teardown evidence as
    /// [`Self::into_webrender`].
    pub fn into_browser_compositor(
        self,
    ) -> Result<WebRenderPresentedWindow, WebRenderWindowStartupFailure> {
        WebRenderPresentedWindow::new(self)
    }
}

impl WebRenderPresentedWindow {
    #[allow(clippy::too_many_lines)]
    fn new(mut presenter: LinuxPresentedWindow) -> Result<Self, WebRenderWindowStartupFailure> {
        let descriptor = presenter.descriptor();
        let capabilities = presenter.capabilities();
        let limits = WebRenderWindowLimits::default();
        if descriptor.size.width == 0 || descriptor.size.height == 0 {
            let primary = WebRenderWindowError::new(
                WebRenderWindowFailureStage::InitializeRenderer,
                WebRenderWindowErrorKind::Suspended,
                "WebRender initialization requires a nonzero native window surface",
            );
            let teardown = retire_partial_presenter(
                presenter,
                None,
                None,
                None,
                &WindowRenderNotifier::default(),
                limits,
            );
            return Err(WebRenderWindowStartupFailure::new(primary, teardown));
        }

        let gl = match presenter.clone_current_gl_for_webrender() {
            Ok(gl) => gl,
            Err(error) => {
                let primary = WebRenderWindowError::presentation(
                    WebRenderWindowFailureStage::InitializeRenderer,
                    &error,
                );
                let teardown = retire_partial_presenter(
                    presenter,
                    None,
                    None,
                    None,
                    &WindowRenderNotifier::default(),
                    limits,
                );
                return Err(WebRenderWindowStartupFailure::new(primary, teardown));
            }
        };
        let notifier = WindowRenderNotifier::default();
        let options = webrender_options(capabilities);
        let initialization = catch_unwind(AssertUnwindSafe(|| {
            create_webrender_instance(gl, Box::new(notifier.clone()), options, None)
        }));
        let (renderer, sender) = match initialization {
            Ok(Ok(values)) => values,
            Ok(Err(error)) => {
                if renderer_startup_disposition(&error)
                    == StartupFailureDisposition::AbortOwningProcess
                {
                    abort_unproven_startup(StartupFailureClass::ConstructorThreadFailure);
                }
                let primary = WebRenderWindowError::new(
                    WebRenderWindowFailureStage::InitializeRenderer,
                    WebRenderWindowErrorKind::Renderer,
                    format_args!("WebRender initialization failed: {error:?}"),
                );
                let teardown =
                    retire_partial_presenter(presenter, None, None, None, &notifier, limits);
                return Err(WebRenderWindowStartupFailure::new(primary, teardown));
            }
            Err(_) => abort_unproven_startup(StartupFailureClass::ConstructorPanic),
        };

        let Ok(api) = catch_unwind(AssertUnwindSafe(|| sender.create_api())) else {
            abort_unproven_startup(StartupFailureClass::ApiCreationPanic);
        };
        if let Err(error) = presenter.clone_current_gl_for_webrender() {
            let primary = WebRenderWindowError::presentation(
                WebRenderWindowFailureStage::InitializeRenderer,
                &error,
            );
            let teardown = retire_partial_presenter(
                presenter,
                Some(renderer),
                Some(api),
                None,
                &notifier,
                limits,
            );
            return Err(WebRenderWindowStartupFailure::new(primary, teardown));
        }
        let document_id = match catch_unwind(AssertUnwindSafe(|| {
            api.add_document(device_size(descriptor.size))
        })) {
            Ok(document_id) => document_id,
            Err(payload) => {
                let primary = panic_error(
                    WebRenderWindowFailureStage::InitializeRenderer,
                    payload.as_ref(),
                );
                let teardown = retire_partial_presenter(
                    presenter,
                    Some(renderer),
                    Some(api),
                    None,
                    &notifier,
                    limits,
                );
                return Err(WebRenderWindowStartupFailure::new(primary, teardown));
            }
        };
        let text_registry = match catch_unwind(AssertUnwindSafe(|| {
            TextFontRegistry::with_default_limits(&api)
        })) {
            Ok(registry) => registry,
            Err(payload) => {
                let primary = panic_error(
                    WebRenderWindowFailureStage::InitializeRenderer,
                    payload.as_ref(),
                );
                let teardown = retire_partial_presenter(
                    presenter,
                    Some(renderer),
                    Some(api),
                    Some(document_id),
                    &notifier,
                    limits,
                );
                return Err(WebRenderWindowStartupFailure::new(primary, teardown));
            }
        };
        let browser_pipelines = BrowserPipelines::new(api.get_namespace_id().0);
        let browser_resource_document = Document::new().version();
        Ok(Self {
            presenter: Some(presenter),
            renderer: Some(renderer),
            api: Some(api),
            document_id,
            notifier,
            text_registry,
            browser_pipelines,
            browser_contract: BrowserCompositorContract::default(),
            browser_resource_document,
            contract: WebRenderWindowContract::new_with_capabilities(descriptor, capabilities),
            active_stage: WebRenderWindowFailureStage::ValidateRequest,
            backend_shutdown_evidence: WebRenderTeardownEvidence::Unknown,
            renderer_deinitialization_evidence: WebRenderTeardownEvidence::Unknown,
            shutdown_complete: false,
        })
    }

    /// Exact value-only target snapshot required by the next frame/transition.
    #[must_use]
    pub const fn surface_snapshot(&self) -> WebRenderSurfaceSnapshot {
        self.contract.snapshot()
    }

    /// Verified immutable acceleration and reset facts for this owner.
    #[must_use]
    pub const fn capabilities(&self) -> LinuxPresentationCapabilities {
        self.contract.snapshot().capabilities()
    }

    /// Current renderer/presenter lifecycle.
    #[must_use]
    pub const fn state(&self) -> WebRenderWindowState {
        self.contract.state()
    }

    /// Whether another transaction may enter this owner.
    #[must_use]
    pub const fn is_usable(&self) -> bool {
        matches!(
            self.contract.state(),
            WebRenderWindowState::Active | WebRenderWindowState::Suspended
        ) && !self.shutdown_complete
    }

    /// Fixed caller-nonenlargeable resource and notification limits.
    #[must_use]
    pub const fn limits(&self) -> WebRenderWindowLimits {
        self.contract.limits()
    }

    /// Current renderer-scoped font template, instance, and byte counts.
    #[must_use]
    pub fn text_registry_statistics(&self) -> TextRegistryStatistics {
        self.text_registry.statistics()
    }

    /// Compares a value-only winit event identity without exporting native authority.
    #[must_use]
    pub fn matches_window_id(&self, id: winit::window::WindowId) -> bool {
        self.presenter
            .as_ref()
            .is_some_and(|presenter| presenter.matches_window_id(id))
    }

    /// Requests a native redraw event.
    pub fn request_redraw(&self) {
        if let Some(presenter) = self.presenter.as_ref() {
            presenter.request_redraw();
        }
    }

    /// Requests a value-only native inner extent without changing EGL or
    /// `WebRender` state ahead of the resulting checked resize event.
    #[must_use]
    pub fn request_inner_size(&self, size: PhysicalSize) -> Option<PhysicalSize> {
        self.presenter
            .as_ref()
            .and_then(|presenter| presenter.request_inner_size(size))
    }

    /// Confirms a synchronously reported window size against the exact retained
    /// EGL surface without changing the current native or `WebRender` surface
    /// contract.
    ///
    /// `Pending` and `ReadyForCheckedResize` are nonterminal and leave the
    /// current descriptor, renderer, browser composition, and revision
    /// untouched. The latter requires the caller to enter the ordinary checked
    /// resize transaction, which recreates and exact-verifies Wayland's EGL
    /// window surface before commit. A query, panic, or missing-owner fault is
    /// mapped to a typed terminal `WebRender` window error.
    ///
    /// # Errors
    ///
    /// Returns a terminal native/owner error when exact EGL extent confirmation
    /// cannot be performed safely.
    pub fn confirm_native_extent(
        &mut self,
        expected: PhysicalSize,
    ) -> Result<NativeExtentConfirmation, WebRenderWindowError> {
        self.active_stage = WebRenderWindowFailureStage::ResizeSurface;
        self.ensure_live_owners()?;
        let Some(presenter) = self.presenter.as_mut() else {
            return Err(self.latch_terminal(owner_missing()));
        };
        let result = catch_unwind(AssertUnwindSafe(|| {
            presenter.confirm_native_extent(expected)
        }));
        match result {
            Ok(Ok(confirmation)) => Ok(confirmation),
            Ok(Err(error)) => {
                let mapped = WebRenderWindowError::presentation(
                    WebRenderWindowFailureStage::ResizeSurface,
                    &error,
                );
                Err(self.latch_terminal(mapped))
            }
            Err(payload) => Err(self.latch_panic(payload.as_ref())),
        }
    }

    /// Enables or disables native IME event delivery for this exact window.
    pub fn set_ime_allowed(&self, allowed: bool) {
        if let Some(presenter) = self.presenter.as_ref() {
            presenter.set_ime_allowed(allowed);
        }
    }

    /// Updates the logical candidate-window rectangle without exposing winit
    /// or native window authority.
    pub fn set_ime_cursor_area(&self, area: LogicalRect) {
        if let Some(presenter) = self.presenter.as_ref() {
            presenter.set_ime_cursor_area(
                area.origin.x,
                area.origin.y,
                area.size.width,
                area.size.height,
            );
        }
    }

    /// Resolves a physical point only against the exact last successfully
    /// swapped browser composition.
    ///
    /// # Errors
    ///
    /// Rejects a foreign/stale surface snapshot, an uninitialized or stale
    /// composition, an accepted terminal failure, or a lost window owner.
    pub fn hit_test_browser(
        &self,
        point: PhysicalPoint,
        surface: WebRenderSurfaceSnapshot,
    ) -> Result<Option<BrowserHitTestResult>, WebRenderWindowError> {
        if !matches!(self.contract.state(), WebRenderWindowState::Active) {
            return Err(WebRenderWindowError::new(
                WebRenderWindowFailureStage::HitTest,
                WebRenderWindowErrorKind::TerminalState,
                "browser hit testing requires an active renderer-owned surface",
            ));
        }
        self.browser_contract.hit_test(point, surface)
    }

    /// Updates the exact native extent first, then publishes a new non-reusing
    /// surface revision and forces `WebRender`'s next full draw.
    ///
    /// # Errors
    ///
    /// Rejects a stale snapshot/resource limit before native mutation. A native
    /// resize, context, renderer, or panic fault is terminal.
    pub fn resize(
        &mut self,
        request: WebRenderWindowResizeRequest,
    ) -> Result<WebRenderSurfaceSnapshot, WebRenderWindowError> {
        self.active_stage = WebRenderWindowFailureStage::ResizeSurface;
        self.ensure_live_owners()?;
        match catch_unwind(AssertUnwindSafe(|| self.resize_inner(request))) {
            Ok(result) => result,
            Err(payload) => Err(self.latch_panic(payload.as_ref())),
        }
    }

    /// Removes the EGL surface after exact stale-snapshot validation while
    /// retaining the WebRender/context owners.
    ///
    /// # Errors
    ///
    /// Returns a stale-snapshot error or a terminal native/renderer failure.
    pub fn suspend(
        &mut self,
        expected: WebRenderSurfaceSnapshot,
    ) -> Result<WebRenderSurfaceSnapshot, WebRenderWindowError> {
        self.active_stage = WebRenderWindowFailureStage::SuspendSurface;
        self.ensure_live_owners()?;
        match catch_unwind(AssertUnwindSafe(|| self.suspend_inner(expected))) {
            Ok(result) => result,
            Err(payload) => Err(self.latch_panic(payload.as_ref())),
        }
    }

    /// Recreates the exact nonzero EGL surface before publishing a fresh
    /// revision and forcing a full `WebRender` draw.
    ///
    /// # Errors
    ///
    /// Returns a stale-snapshot error or a terminal native/renderer failure.
    pub fn resume(
        &mut self,
        expected: WebRenderSurfaceSnapshot,
    ) -> Result<WebRenderSurfaceSnapshot, WebRenderWindowError> {
        self.active_stage = WebRenderWindowFailureStage::ResumeSurface;
        self.ensure_live_owners()?;
        match catch_unwind(AssertUnwindSafe(|| self.resume_inner(expected))) {
            Ok(result) => result,
            Err(payload) => Err(self.latch_panic(payload.as_ref())),
        }
    }

    /// Updates logical scale only after exact snapshot validation and publishes
    /// a fresh revision. The current scene contract remains scale-one in device
    /// pixels; this method does not fabricate a native resize.
    ///
    /// # Errors
    ///
    /// Returns a stale-snapshot or native presenter failure.
    pub fn update_scale(
        &mut self,
        expected: WebRenderSurfaceSnapshot,
        scale: ScaleFactor,
    ) -> Result<WebRenderSurfaceSnapshot, WebRenderWindowError> {
        self.active_stage = WebRenderWindowFailureStage::ResizeSurface;
        self.ensure_live_owners()?;
        match catch_unwind(AssertUnwindSafe(|| {
            self.update_scale_inner(expected, scale)
        })) {
            Ok(result) => result,
            Err(payload) => Err(self.latch_panic(payload.as_ref())),
        }
    }

    /// Consumes one validated immutable scene plus its exact canonical shaped
    /// text inventory, renders it directly into the current native EGL back
    /// buffer, and submits one swap.
    ///
    /// The returned receipt distinguishes backend build, `WebRender` draw, and
    /// EGL swap submission. It never claims desktop-compositor acknowledgement.
    ///
    /// # Errors
    ///
    /// Rejects stale surface/document/pipeline/epoch/sequence identities,
    /// viewport and resource mismatches, scene/text validation failures,
    /// notification deadline/ordering failures, renderer/device/native errors,
    /// and contained panics. Every error after transaction acceptance is
    /// terminal for this owner.
    pub fn submit_scene(
        &mut self,
        scene: CompiledScene,
        texts: &[ShapedSceneText],
        request: WebRenderWindowFrameRequest,
    ) -> Result<WebRenderWindowFrameReceipt, WebRenderWindowError> {
        self.active_stage = WebRenderWindowFailureStage::ValidateRequest;
        self.ensure_live_owners()?;
        match catch_unwind(AssertUnwindSafe(|| {
            self.submit_scene_inner(scene, texts, request)
        })) {
            Ok(result) => result,
            Err(payload) => Err(self.latch_panic(payload.as_ref())),
        }
    }

    /// Atomically publishes exact page content plus independently revisioned
    /// Rust-authored browser chrome through one `WebRender` frame and one EGL
    /// swap.
    ///
    /// The page update is consumed even when validation or presentation fails.
    /// Callers which need another installation attempt must request an exact
    /// engine rerender. A rejection before transaction acceptance preserves
    /// the prior successful receipt and hit map. Any failure after acceptance
    /// permanently closes this owner and invalidates browser hit admission.
    ///
    /// # Errors
    ///
    /// Rejects foreign/stale page, chrome, surface, epoch, swap, resource, text,
    /// and pipeline identities; composition/notification deadlines; renderer,
    /// device, and native faults; or contained panics.
    pub fn submit_browser_frame(
        &mut self,
        page: BrowserPageUpdate,
        chrome: Option<BrowserChromeScene>,
        request: BrowserFrameRequest,
    ) -> Result<BrowserFrameReceipt, WebRenderWindowError> {
        self.active_stage = WebRenderWindowFailureStage::ValidateRequest;
        self.ensure_live_owners()?;
        let result = catch_unwind(AssertUnwindSafe(|| {
            self.submit_browser_frame_inner(page, chrome, request)
        }));
        match result {
            Ok(Ok(receipt)) => Ok(receipt),
            Ok(Err(error)) => {
                if self.browser_contract.accepted_in_flight() {
                    return Err(terminalize_accepted_browser_error(
                        &mut self.contract,
                        &mut self.browser_contract,
                        error,
                    ));
                }
                Err(error)
            }
            Err(payload) => {
                if self.browser_contract.accepted_in_flight() {
                    self.browser_contract.fail_after_acceptance();
                }
                Err(self.latch_panic(payload.as_ref()))
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    fn submit_browser_frame_inner(
        &mut self,
        page: BrowserPageUpdate,
        chrome: Option<BrowserChromeScene>,
        request: BrowserFrameRequest,
    ) -> Result<BrowserFrameReceipt, WebRenderWindowError> {
        if self.contract.state() != WebRenderWindowState::Active {
            return Err(WebRenderWindowError::new(
                WebRenderWindowFailureStage::ValidateRequest,
                WebRenderWindowErrorKind::Suspended,
                "browser composition requires an active nonzero native surface",
            ));
        }
        let candidate = self.browser_contract.validate_candidate(
            &page,
            chrome.as_ref(),
            request,
            self.browser_pipelines,
            self.contract.snapshot(),
            self.contract.last_epoch(),
            self.contract.last_sequence(),
            self.contract.submitted_frames(),
        )?;
        let geometry = chrome
            .as_ref()
            .map(BrowserChromeScene::geometry)
            .or_else(|| self.browser_contract.retained_geometry())
            .ok_or_else(|| {
                WebRenderWindowError::new(
                    WebRenderWindowFailureStage::ValidateRequest,
                    WebRenderWindowErrorKind::Contract,
                    "browser composition has no supplied or retained chrome geometry",
                )
            })?;
        let hit_map = chrome
            .as_ref()
            .map(BrowserChromeScene::hit_map)
            .or_else(|| self.browser_contract.retained_hit_map())
            .ok_or_else(|| {
                WebRenderWindowError::new(
                    WebRenderWindowFailureStage::ValidateRequest,
                    WebRenderWindowErrorKind::Contract,
                    "browser composition has no supplied or retained chrome hit map",
                )
            })?;

        let deadline = Instant::now()
            .checked_add(self.contract.limits().frame_timeout())
            .ok_or_else(|| {
                WebRenderWindowError::new(
                    WebRenderWindowFailureStage::ValidateRequest,
                    WebRenderWindowErrorKind::Contract,
                    "browser frame deadline overflowed",
                )
            })?;
        let direct_request = DirectFrameRequest::new(
            request.surface().surface(),
            request.surface().size(),
            request.sequence(),
        );
        if let Err(error) = self
            .presenter_ref()?
            .validate_webrender_frame(direct_request)
        {
            return Err(self.latch_terminal(admitted_native_error(
                WebRenderWindowFailureStage::ValidateRequest,
                &error,
            )));
        }

        let mut page_text_map = None;
        let mut page_display_list_bytes = 0_usize;
        if let BrowserPageUpdate::Install(page_scene) = &page {
            validate_browser_page_scene(page_scene, geometry, self.contract.limits())?;
            let descriptors = page_scene_text_descriptors(page_scene)?;
            page_text_map = Some(
                page_scene
                    .scene()
                    .validate_text_map(&descriptors)
                    .map_err(scene_error)?,
            );
            check_deadline(deadline, WebRenderWindowFailureStage::ComposeScene)?;
        }

        self.active_stage = WebRenderWindowFailureStage::ComposeScene;
        let resource_version = DocumentVersion::new(
            self.browser_resource_document.document_id(),
            request.sequence(),
        );
        let page_texts = match &page {
            BrowserPageUpdate::Install(page) => Some(page.texts()),
            BrowserPageUpdate::Retain | BrowserPageUpdate::ClearToBlank => None,
        };
        let (staged_texts, partition) =
            stage_browser_texts(resource_version, page_texts, chrome.as_ref())?;
        if staged_texts.len() > self.contract.limits().max_pending_text_runs() {
            return Err(WebRenderWindowError::new(
                WebRenderWindowFailureStage::ValidateRequest,
                WebRenderWindowErrorKind::ResourceLimit,
                "combined page/chrome shaped text exceeds the fixed window limit",
            ));
        }
        let prepared = {
            let api = self.api.as_ref().ok_or_else(owner_missing)?;
            self.text_registry
                .prepare_scene(api, resource_version, staged_texts.len(), &staged_texts)
                .map_err(text_error)?
        };
        partition.validate_entries(prepared.entries())?;
        check_deadline(deadline, WebRenderWindowFailureStage::ComposeScene)?;

        let mut page_built = None;
        if let BrowserPageUpdate::Install(page_scene) = page {
            let (identity, scene, texts) = page_scene.into_parts();
            debug_assert_eq!(texts.len(), partition.page_count());
            drop(texts);
            let text_map = page_text_map.take().ok_or_else(|| {
                WebRenderWindowError::new(
                    WebRenderWindowFailureStage::ComposeScene,
                    WebRenderWindowErrorKind::InternalDrift,
                    "page installation lost its prevalidated text map",
                )
            })?;
            let mut resolution = text_map
                .begin_resolution(prepared.renderer_namespace())
                .map_err(scene_error)?;
            for (index, entry) in prepared.entries()[..partition.page_count()]
                .iter()
                .enumerate()
            {
                resolution
                    .resolve_next(
                        identity.document_version(),
                        u32::try_from(index).map_err(|_| {
                            WebRenderWindowError::new(
                                WebRenderWindowFailureStage::ComposeScene,
                                WebRenderWindowErrorKind::ResourceLimit,
                                "page text resolution index exceeds u32 capacity",
                            )
                        })?,
                        entry
                            .runs()
                            .iter()
                            .map(|run| (run.font_instance(), run.glyphs())),
                    )
                    .map_err(scene_error)?;
            }
            let composed = scene
                .compose_text(resolution.finish().map_err(scene_error)?)
                .map_err(scene_error)?;
            if !composed.scene().pending_text().is_empty() {
                return Err(WebRenderWindowError::new(
                    WebRenderWindowFailureStage::ComposeScene,
                    WebRenderWindowErrorKind::Scene,
                    "browser page composition retained unresolved text",
                ));
            }
            page_display_list_bytes = composed.built_display_list().size_in_bytes();
            if page_display_list_bytes > self.contract.limits().max_display_list_bytes() {
                return Err(WebRenderWindowError::new(
                    WebRenderWindowFailureStage::ComposeScene,
                    WebRenderWindowErrorKind::ResourceLimit,
                    "composed browser page display list exceeds its fixed limit",
                ));
            }
            let (pipeline, display_list) = composed.into_webrender();
            if PipelineKey::new(pipeline.0, pipeline.1) != identity.pipeline() {
                return Err(self.latch_terminal(WebRenderWindowError::new(
                    WebRenderWindowFailureStage::ComposeScene,
                    WebRenderWindowErrorKind::InternalDrift,
                    "page text composition changed its early-validated pipeline",
                )));
            }
            page_built = Some((pipeline, display_list));
        }

        let chrome_built = match chrome.as_ref() {
            Some(scene) => Some(build_browser_chrome_display_list(
                scene,
                partition.chrome_entries(prepared.entries())?,
                self.browser_pipelines.chrome(),
                partition.page_count(),
            )?),
            None => None,
        };
        let root_built = build_browser_root_display_list(
            self.browser_pipelines,
            request.surface(),
            geometry,
            candidate.page,
        )?;
        check_deadline(deadline, WebRenderWindowFailureStage::ComposeScene)?;

        self.active_stage = WebRenderWindowFailureStage::SubmitTransaction;
        let frame_ready_before = self.notifier.frame_ready_count();
        let (built_request, built_waiter) =
            WindowStageWaiter::new(Checkpoint::FrameBuilt, &self.notifier);
        let (rendered_request, rendered_waiter) =
            WindowStageWaiter::new(Checkpoint::FrameRendered, &self.notifier);
        let retired_page_pipeline = retired_browser_page_pipeline(candidate);
        let mut transaction = Transaction::new();
        if let Some(pipeline) = retired_page_pipeline {
            transaction.remove_pipeline(pipeline);
        }
        if let Some((pipeline, display_list)) = page_built {
            transaction.set_display_list(Epoch(request.epoch()), (pipeline, display_list));
        }
        let chrome_display_list_bytes = chrome_built
            .as_ref()
            .map_or(0, |built| built.display_list.size_in_bytes());
        let chrome_primitives = chrome_built
            .as_ref()
            .map_or(0, |built| built.primitive_count);
        if let Some(chrome) = chrome_built {
            transaction.set_display_list(
                Epoch(request.epoch()),
                (chrome.pipeline, chrome.display_list),
            );
        }
        let root_display_list_bytes = root_built.display_list.size_in_bytes();
        let combined_display_list_bytes = page_display_list_bytes
            .checked_add(chrome_display_list_bytes)
            .and_then(|bytes| bytes.checked_add(root_display_list_bytes))
            .ok_or_else(|| {
                WebRenderWindowError::new(
                    WebRenderWindowFailureStage::ComposeScene,
                    WebRenderWindowErrorKind::ResourceLimit,
                    "combined browser display-list byte count overflowed",
                )
            })?;
        if combined_display_list_bytes > self.contract.limits().max_display_list_bytes() {
            return Err(WebRenderWindowError::new(
                WebRenderWindowFailureStage::ComposeScene,
                WebRenderWindowErrorKind::ResourceLimit,
                "combined page/chrome/root display lists exceed the fixed window limit",
            ));
        }
        transaction.set_document_view(DeviceIntRect::from_size(device_size(
            request.surface().size(),
        )));
        transaction.notify(built_request);
        transaction.notify(rendered_request);
        transaction.invalidate_rendered_frame(RenderReasons::SCENE);
        transaction.generate_frame(request.sequence(), true, true, RenderReasons::SCENE);
        drop(chrome);

        self.browser_contract.mark_accepted();
        let api = self.api.as_mut().ok_or_else(owner_missing)?;
        let submitted = prepared
            .submit(
                api,
                self.document_id,
                transaction,
                Epoch(request.epoch()),
                root_built.pipeline,
                root_built.display_list,
            )
            .map_err(text_error)?;
        if submitted != self.browser_pipelines.root() {
            return Err(self.latch_terminal(WebRenderWindowError::new(
                WebRenderWindowFailureStage::SubmitTransaction,
                WebRenderWindowErrorKind::Renderer,
                "browser transaction returned a foreign root pipeline identity",
            )));
        }
        self.contract.commit_browser_transaction(
            request.epoch(),
            PipelineKey::new(submitted.0, submitted.1),
        );
        check_accepted_deadline(
            &mut self.contract,
            deadline,
            WebRenderWindowFailureStage::SubmitTransaction,
        )?;

        let completion = self.complete_browser_frame(
            request,
            candidate,
            retired_page_pipeline,
            direct_request,
            deadline,
            frame_ready_before,
            built_waiter,
            rendered_waiter,
        )?;
        Ok(self.browser_contract.commit_success(
            candidate,
            request,
            hit_map,
            BrowserFrameAccounting {
                backend_publish_id: completion.backend_publish_id,
                rgba8_byte_equivalent: completion.rgba8_byte_equivalent,
                page_display_list_bytes,
                chrome_display_list_bytes,
                root_display_list_bytes,
                chrome_primitives,
            },
        ))
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    fn complete_browser_frame(
        &mut self,
        request: BrowserFrameRequest,
        candidate: BrowserCandidate,
        retired_page_pipeline: Option<PipelineId>,
        direct_request: DirectFrameRequest,
        deadline: Instant,
        frame_ready_before: u64,
        built_waiter: WindowStageWaiter,
        rendered_waiter: WindowStageWaiter,
    ) -> Result<CompletedBrowserFrame, WebRenderWindowError> {
        self.active_stage = WebRenderWindowFailureStage::AwaitFrameBuilt;
        if let Err(error) = built_waiter.wait_until(Checkpoint::FrameBuilt, deadline) {
            return Err(self.latch_terminal(notification_error(
                WebRenderWindowFailureStage::AwaitFrameBuilt,
                error,
            )));
        }

        self.active_stage = WebRenderWindowFailureStage::AwaitFrameReady;
        let evidence = match self
            .notifier
            .wait_for_frame_ready_after(frame_ready_before, deadline)
        {
            Ok(evidence) => evidence,
            Err(error) => {
                return Err(self.latch_terminal(notification_error(
                    WebRenderWindowFailureStage::AwaitFrameReady,
                    error,
                )));
            }
        };
        self.validate_frame_ready(frame_ready_before, evidence)?;
        if self.notifier.saw_unexpected_external_event() {
            return Err(self.latch_terminal(WebRenderWindowError::new(
                WebRenderWindowFailureStage::AwaitFrameReady,
                WebRenderWindowErrorKind::Backend,
                "WebRender emitted an unauthorized external renderer-thread event",
            )));
        }
        if self.notifier.overflowed() {
            return Err(self.latch_terminal(WebRenderWindowError::new(
                WebRenderWindowFailureStage::AwaitFrameReady,
                WebRenderWindowErrorKind::NotificationOverflow,
                "fixed-state renderer notification counter overflowed",
            )));
        }
        check_accepted_deadline(
            &mut self.contract,
            deadline,
            WebRenderWindowFailureStage::AwaitFrameReady,
        )?;

        self.active_stage = WebRenderWindowFailureStage::PrepareNativeFrame;
        let rgba8_byte_equivalent = {
            let presenter = self.presenter.as_mut().ok_or_else(owner_missing)?;
            match presenter.prepare_webrender_frame(direct_request) {
                Ok(bytes) => bytes,
                Err(error) => {
                    return Err(self.latch_terminal(WebRenderWindowError::presentation(
                        WebRenderWindowFailureStage::PrepareNativeFrame,
                        &error,
                    )));
                }
            }
        };
        check_accepted_deadline(
            &mut self.contract,
            deadline,
            WebRenderWindowFailureStage::PrepareNativeFrame,
        )?;

        self.active_stage = WebRenderWindowFailureStage::UpdateRenderer;
        self.renderer.as_mut().ok_or_else(owner_missing)?.update();
        check_accepted_deadline(
            &mut self.contract,
            deadline,
            WebRenderWindowFailureStage::UpdateRenderer,
        )?;
        if let Err(error) = self
            .presenter
            .as_mut()
            .ok_or_else(owner_missing)?
            .verify_webrender_gl()
        {
            return Err(self.latch_terminal(WebRenderWindowError::presentation(
                WebRenderWindowFailureStage::UpdateRenderer,
                &error,
            )));
        }
        check_accepted_deadline(
            &mut self.contract,
            deadline,
            WebRenderWindowFailureStage::UpdateRenderer,
        )?;

        self.active_stage = WebRenderWindowFailureStage::VerifyEpoch;
        // `Renderer::current_epoch` is an accumulating cache and deliberately
        // retains entries for removed pipelines. Every requested WebRender
        // frame instead publishes a full current epoch map plus the exact
        // removals drained for that frame. Consume that bounded snapshot so a
        // stale cached epoch cannot be confused with display reachability.
        let publication = self
            .renderer
            .as_mut()
            .ok_or_else(owner_missing)?
            .flush_pipeline_info();
        check_accepted_deadline(
            &mut self.contract,
            deadline,
            WebRenderWindowFailureStage::VerifyEpoch,
        )?;
        validate_browser_pipeline_publication(
            &publication,
            self.document_id,
            self.browser_pipelines,
            request.epoch(),
            candidate.chrome_epoch,
            candidate.page,
            candidate.page_epoch,
            retired_page_pipeline,
        )?;

        self.active_stage = WebRenderWindowFailureStage::RenderFrame;
        if let Err(errors) = self
            .renderer
            .as_mut()
            .ok_or_else(owner_missing)?
            .render(device_size(request.surface().size()), 0)
        {
            return Err(self.latch_terminal(WebRenderWindowError::new(
                WebRenderWindowFailureStage::RenderFrame,
                WebRenderWindowErrorKind::Renderer,
                format_args!("WebRender browser draw failed: {errors:?}"),
            )));
        }
        check_accepted_deadline(
            &mut self.contract,
            deadline,
            WebRenderWindowFailureStage::RenderFrame,
        )?;
        if let Err(error) = self
            .presenter
            .as_mut()
            .ok_or_else(owner_missing)?
            .verify_webrender_gl()
        {
            return Err(self.latch_terminal(WebRenderWindowError::presentation(
                WebRenderWindowFailureStage::RenderFrame,
                &error,
            )));
        }
        check_accepted_deadline(
            &mut self.contract,
            deadline,
            WebRenderWindowFailureStage::RenderFrame,
        )?;

        self.active_stage = WebRenderWindowFailureStage::AwaitFrameRendered;
        if let Err(error) = rendered_waiter.wait_until(Checkpoint::FrameRendered, deadline) {
            return Err(self.latch_terminal(notification_error(
                WebRenderWindowFailureStage::AwaitFrameRendered,
                error,
            )));
        }
        check_accepted_deadline(
            &mut self.contract,
            deadline,
            WebRenderWindowFailureStage::AwaitFrameRendered,
        )?;

        self.active_stage = WebRenderWindowFailureStage::SwapBuffers;
        check_accepted_deadline(
            &mut self.contract,
            deadline,
            WebRenderWindowFailureStage::SwapBuffers,
        )?;
        if let Err(error) = self
            .presenter
            .as_mut()
            .ok_or_else(owner_missing)?
            .swap_webrender_frame(direct_request)
        {
            return Err(self.latch_terminal(WebRenderWindowError::presentation(
                WebRenderWindowFailureStage::SwapBuffers,
                &error,
            )));
        }
        self.contract.commit_swap(request.sequence());
        check_accepted_deadline(
            &mut self.contract,
            deadline,
            WebRenderWindowFailureStage::SwapBuffers,
        )?;
        Ok(CompletedBrowserFrame {
            backend_publish_id: evidence.publish_id.0,
            rgba8_byte_equivalent,
        })
    }

    fn update_scale_inner(
        &mut self,
        expected: WebRenderSurfaceSnapshot,
        scale: ScaleFactor,
    ) -> Result<WebRenderSurfaceSnapshot, WebRenderWindowError> {
        let revision = self
            .contract
            .prepare_surface_transition(expected, WebRenderWindowFailureStage::ResizeSurface)?;
        let native_result = {
            let presenter = self.presenter.as_mut().ok_or_else(owner_missing)?;
            presenter
                .update_scale(expected.surface(), scale)
                .map(|()| presenter.descriptor())
        };
        let descriptor = match native_result {
            Ok(descriptor) => descriptor,
            Err(error) => {
                let mapped =
                    admitted_native_error(WebRenderWindowFailureStage::ResizeSurface, &error);
                return Err(self.latch_terminal(mapped));
            }
        };
        self.contract.commit_scale_transition(descriptor, revision);
        self.browser_contract.mark_surface_stale();
        Ok(self.contract.snapshot())
    }

    /// Explicitly releases font/document/backend resources, deinitializes
    /// `WebRender` while the exact context is current, and only then releases the
    /// nested native presenter.
    ///
    /// # Errors
    ///
    /// Returns the first authoritative shutdown failure together with the exact
    /// native release-or-retention outcome. Cleanup continues after retry-safe
    /// backend errors whenever native ownership can still be released safely.
    pub fn shutdown(
        mut self,
    ) -> Result<WebRenderWindowShutdownReport, WebRenderWindowShutdownFailure> {
        self.active_stage = WebRenderWindowFailureStage::ShutdownBackend;
        match catch_unwind(AssertUnwindSafe(|| self.cleanup())) {
            Ok(result) => result,
            Err(payload) => Err(self.retain_after_cleanup_panic(payload.as_ref())),
        }
    }

    fn resize_inner(
        &mut self,
        request: WebRenderWindowResizeRequest,
    ) -> Result<WebRenderSurfaceSnapshot, WebRenderWindowError> {
        let revision = self.contract.prepare_resize(request)?;
        let native_result = {
            let presenter = self.presenter.as_mut().ok_or_else(owner_missing)?;
            presenter
                .resize(request.expected().surface(), request.size())
                .map(|()| presenter.descriptor())
        };
        let descriptor = match native_result {
            Ok(descriptor) => descriptor,
            Err(error) => {
                let mapped =
                    admitted_native_error(WebRenderWindowFailureStage::ResizeSurface, &error);
                return Err(self.latch_terminal(mapped));
            }
        };
        self.contract
            .commit_surface_transition(descriptor, revision, false);
        self.browser_contract.mark_surface_stale();
        self.force_redraw_after_surface_transition()?;
        Ok(self.contract.snapshot())
    }

    fn suspend_inner(
        &mut self,
        expected: WebRenderSurfaceSnapshot,
    ) -> Result<WebRenderSurfaceSnapshot, WebRenderWindowError> {
        let revision = self.contract.prepare_suspend(expected)?;
        let native_result = {
            let presenter = self.presenter.as_mut().ok_or_else(owner_missing)?;
            presenter.suspend().map(|()| presenter.descriptor())
        };
        let descriptor = match native_result {
            Ok(descriptor) => descriptor,
            Err(error) => {
                let mapped =
                    admitted_native_error(WebRenderWindowFailureStage::SuspendSurface, &error);
                return Err(self.latch_terminal(mapped));
            }
        };
        self.contract
            .commit_surface_transition(descriptor, revision, true);
        self.browser_contract.mark_surface_stale();
        Ok(self.contract.snapshot())
    }

    fn resume_inner(
        &mut self,
        expected: WebRenderSurfaceSnapshot,
    ) -> Result<WebRenderSurfaceSnapshot, WebRenderWindowError> {
        let revision = self.contract.prepare_resume(expected)?;
        let native_result = {
            let presenter = self.presenter.as_mut().ok_or_else(owner_missing)?;
            presenter.resume().map(|()| presenter.descriptor())
        };
        let descriptor = match native_result {
            Ok(descriptor) => descriptor,
            Err(error) => {
                let mapped =
                    admitted_native_error(WebRenderWindowFailureStage::ResumeSurface, &error);
                return Err(self.latch_terminal(mapped));
            }
        };
        self.contract
            .commit_surface_transition(descriptor, revision, false);
        self.browser_contract.mark_surface_stale();
        self.force_redraw_after_surface_transition()?;
        Ok(self.contract.snapshot())
    }

    fn force_redraw_after_surface_transition(&mut self) -> Result<(), WebRenderWindowError> {
        let Some(renderer) = self.renderer.as_mut() else {
            let error = WebRenderWindowError::new(
                self.active_stage,
                WebRenderWindowErrorKind::TerminalState,
                "surface transition lost its internally owned WebRender renderer",
            );
            return Err(self.latch_terminal(error));
        };
        renderer.force_redraw();
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn submit_scene_inner(
        &mut self,
        scene: CompiledScene,
        texts: &[ShapedSceneText],
        request: WebRenderWindowFrameRequest,
    ) -> Result<WebRenderWindowFrameReceipt, WebRenderWindowError> {
        let compiled_pipeline = scene.pipeline();
        with_validated_pipeline(request, compiled_pipeline, || {
            self.submit_pipeline_validated_scene(scene, texts, request)
        })
    }

    #[allow(clippy::too_many_lines)]
    fn submit_pipeline_validated_scene(
        &mut self,
        scene: CompiledScene,
        texts: &[ShapedSceneText],
        request: WebRenderWindowFrameRequest,
    ) -> Result<WebRenderWindowFrameReceipt, WebRenderWindowError> {
        let deadline = Instant::now()
            .checked_add(self.contract.limits().frame_timeout())
            .ok_or_else(|| {
                WebRenderWindowError::new(
                    WebRenderWindowFailureStage::ValidateRequest,
                    WebRenderWindowErrorKind::Contract,
                    "frame deadline overflowed",
                )
            })?;
        let document_version = scene.document_version();
        let scene_items = scene.scene().items().len();
        let pending_text_count = scene.scene().pending_text().len();
        let (scene_width, scene_height) = scene_device_size(&scene)?;
        self.contract.validate_submission(
            request,
            document_version,
            scene_width,
            scene_height,
            scene_items,
            pending_text_count,
            scene.built_display_list().size_in_bytes(),
        )?;
        validate_shaped_text_count(texts.len(), pending_text_count)?;
        let direct_request = DirectFrameRequest::new(
            request.surface_snapshot().surface(),
            request.surface_snapshot().size(),
            request.sequence(),
        );
        let native_validation = self
            .presenter_ref()?
            .validate_webrender_frame(direct_request);
        if let Err(error) = native_validation {
            let mapped =
                admitted_native_error(WebRenderWindowFailureStage::ValidateRequest, &error);
            return Err(self.latch_terminal(mapped));
        }

        self.active_stage = WebRenderWindowFailureStage::ComposeScene;
        let mut descriptors = Vec::new();
        descriptors.try_reserve_exact(texts.len()).map_err(|_| {
            WebRenderWindowError::new(
                WebRenderWindowFailureStage::ComposeScene,
                WebRenderWindowErrorKind::ResourceLimit,
                format_args!("could not reserve {} text descriptors", texts.len()),
            )
        })?;
        descriptors.extend(texts.iter().map(|text| {
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
        }));
        let text_map = scene.validate_text_map(&descriptors).map_err(scene_error)?;
        drop(descriptors);
        check_deadline(deadline, WebRenderWindowFailureStage::ComposeScene)?;

        let previous_pipeline = self.contract.last_pipeline();
        let prepared = {
            let api = self.api.as_ref().ok_or_else(owner_missing)?;
            self.text_registry
                .prepare_scene(api, document_version, pending_text_count, texts)
                .map_err(text_error)?
        };
        check_deadline(deadline, WebRenderWindowFailureStage::ComposeScene)?;
        let mut resolution = text_map
            .begin_resolution(prepared.renderer_namespace())
            .map_err(scene_error)?;
        for entry in prepared.entries() {
            resolution
                .resolve_next(
                    entry.document_version(),
                    entry.pending_index(),
                    entry
                        .runs()
                        .iter()
                        .map(|run| (run.font_instance(), run.glyphs())),
                )
                .map_err(scene_error)?;
        }
        let resolved = resolution.finish().map_err(scene_error)?;
        let composed = scene.compose_text(resolved).map_err(scene_error)?;
        if !composed.scene().pending_text().is_empty() {
            return Err(WebRenderWindowError::new(
                WebRenderWindowFailureStage::ComposeScene,
                WebRenderWindowErrorKind::Scene,
                "composed scene retained unresolved text",
            ));
        }
        let display_list_bytes = composed.built_display_list().size_in_bytes();
        if display_list_bytes > self.contract.limits().max_display_list_bytes() {
            return Err(WebRenderWindowError::new(
                WebRenderWindowFailureStage::ComposeScene,
                WebRenderWindowErrorKind::ResourceLimit,
                format_args!(
                    "composed display-list bytes {display_list_bytes} exceeds fixed limit {}",
                    self.contract.limits().max_display_list_bytes()
                ),
            ));
        }
        let (pipeline_id, display_list) = composed.into_webrender();
        if PipelineKey::new(pipeline_id.0, pipeline_id.1) != request.pipeline() {
            return Err(self.latch_terminal(WebRenderWindowError::new(
                WebRenderWindowFailureStage::ComposeScene,
                WebRenderWindowErrorKind::InternalDrift,
                "text composition changed an early-validated pipeline identity",
            )));
        }
        check_deadline(deadline, WebRenderWindowFailureStage::ComposeScene)?;

        self.active_stage = WebRenderWindowFailureStage::SubmitTransaction;
        let frame_ready_before = self.notifier.frame_ready_count();
        let (built_request, built_waiter) =
            WindowStageWaiter::new(Checkpoint::FrameBuilt, &self.notifier);
        let (rendered_request, rendered_waiter) =
            WindowStageWaiter::new(Checkpoint::FrameRendered, &self.notifier);
        let mut transaction = Transaction::new();
        if let Some(previous) = previous_pipeline
            && previous != request.pipeline()
        {
            transaction.remove_pipeline(PipelineId(previous.source(), previous.pipeline()));
        }
        transaction.set_document_view(DeviceIntRect::from_size(device_size(
            request.surface_snapshot().size(),
        )));
        transaction.notify(built_request);
        transaction.notify(rendered_request);
        transaction.invalidate_rendered_frame(RenderReasons::SCENE);
        transaction.generate_frame(request.sequence(), true, true, RenderReasons::SCENE);
        let api = self.api.as_mut().ok_or_else(owner_missing)?;
        let submitted = match prepared.submit(
            api,
            self.document_id,
            transaction,
            Epoch(request.epoch()),
            pipeline_id,
            display_list,
        ) {
            Ok(submitted) => {
                // The legacy transaction has now replaced the document root.
                // A previously successful browser receipt/hit map is no longer
                // authoritative even if later rendering or swap fails.
                self.browser_contract.invalidate_for_legacy_acceptance();
                submitted
            }
            Err(error) => return Err(text_error(error)),
        };
        if submitted != pipeline_id {
            return Err(self.latch_terminal(WebRenderWindowError::new(
                WebRenderWindowFailureStage::SubmitTransaction,
                WebRenderWindowErrorKind::Renderer,
                "text transaction returned a different pipeline identity",
            )));
        }
        self.contract.commit_transaction(request);
        check_accepted_deadline(
            &mut self.contract,
            deadline,
            WebRenderWindowFailureStage::SubmitTransaction,
        )?;

        self.active_stage = WebRenderWindowFailureStage::AwaitFrameBuilt;
        if let Err(error) = built_waiter.wait_until(Checkpoint::FrameBuilt, deadline) {
            return Err(self.latch_terminal(notification_error(
                WebRenderWindowFailureStage::AwaitFrameBuilt,
                error,
            )));
        }
        self.active_stage = WebRenderWindowFailureStage::AwaitFrameReady;
        let evidence = match self
            .notifier
            .wait_for_frame_ready_after(frame_ready_before, deadline)
        {
            Ok(evidence) => evidence,
            Err(error) => {
                return Err(self.latch_terminal(notification_error(
                    WebRenderWindowFailureStage::AwaitFrameReady,
                    error,
                )));
            }
        };
        self.validate_frame_ready(frame_ready_before, evidence)?;
        if self.notifier.saw_unexpected_external_event() {
            return Err(self.latch_terminal(WebRenderWindowError::new(
                WebRenderWindowFailureStage::AwaitFrameReady,
                WebRenderWindowErrorKind::Backend,
                "WebRender emitted an unauthorized external renderer-thread event",
            )));
        }
        if self.notifier.overflowed() {
            return Err(self.latch_terminal(WebRenderWindowError::new(
                WebRenderWindowFailureStage::AwaitFrameReady,
                WebRenderWindowErrorKind::NotificationOverflow,
                "fixed-state renderer notification counter overflowed",
            )));
        }
        check_accepted_deadline(
            &mut self.contract,
            deadline,
            WebRenderWindowFailureStage::AwaitFrameReady,
        )?;

        self.active_stage = WebRenderWindowFailureStage::PrepareNativeFrame;
        let native_prepare = {
            let presenter = self.presenter.as_mut().ok_or_else(owner_missing)?;
            presenter.prepare_webrender_frame(direct_request)
        };
        let rgba8_bytes = match native_prepare {
            Ok(bytes) => bytes,
            Err(error) => {
                return Err(self.latch_terminal(WebRenderWindowError::presentation(
                    WebRenderWindowFailureStage::PrepareNativeFrame,
                    &error,
                )));
            }
        };
        check_accepted_deadline(
            &mut self.contract,
            deadline,
            WebRenderWindowFailureStage::PrepareNativeFrame,
        )?;
        self.active_stage = WebRenderWindowFailureStage::UpdateRenderer;
        self.renderer.as_mut().ok_or_else(owner_missing)?.update();
        check_accepted_deadline(
            &mut self.contract,
            deadline,
            WebRenderWindowFailureStage::UpdateRenderer,
        )?;
        let update_gl = {
            let presenter = self.presenter.as_mut().ok_or_else(owner_missing)?;
            presenter.verify_webrender_gl()
        };
        if let Err(error) = update_gl {
            return Err(self.latch_terminal(WebRenderWindowError::presentation(
                WebRenderWindowFailureStage::UpdateRenderer,
                &error,
            )));
        }
        check_accepted_deadline(
            &mut self.contract,
            deadline,
            WebRenderWindowFailureStage::UpdateRenderer,
        )?;

        self.active_stage = WebRenderWindowFailureStage::VerifyEpoch;
        let actual_epoch = self
            .renderer
            .as_ref()
            .ok_or_else(owner_missing)?
            .current_epoch(self.document_id, pipeline_id)
            .map(|epoch| epoch.0);
        check_accepted_deadline(
            &mut self.contract,
            deadline,
            WebRenderWindowFailureStage::VerifyEpoch,
        )?;
        if actual_epoch != Some(request.epoch()) {
            return Err(self.latch_terminal(WebRenderWindowError::new(
                WebRenderWindowFailureStage::VerifyEpoch,
                WebRenderWindowErrorKind::Renderer,
                format_args!(
                    "submitted epoch {} was not published for exact pipeline ({}, {}); actual {actual_epoch:?}",
                    request.epoch(),
                    pipeline_id.0,
                    pipeline_id.1
                ),
            )));
        }

        self.active_stage = WebRenderWindowFailureStage::RenderFrame;
        let render_result = self
            .renderer
            .as_mut()
            .ok_or_else(owner_missing)?
            .render(device_size(request.surface_snapshot().size()), 0);
        if let Err(errors) = render_result {
            return Err(self.latch_terminal(WebRenderWindowError::new(
                WebRenderWindowFailureStage::RenderFrame,
                WebRenderWindowErrorKind::Renderer,
                format_args!("WebRender draw failed: {errors:?}"),
            )));
        }
        check_accepted_deadline(
            &mut self.contract,
            deadline,
            WebRenderWindowFailureStage::RenderFrame,
        )?;
        let render_gl = {
            let presenter = self.presenter.as_mut().ok_or_else(owner_missing)?;
            presenter.verify_webrender_gl()
        };
        if let Err(error) = render_gl {
            return Err(self.latch_terminal(WebRenderWindowError::presentation(
                WebRenderWindowFailureStage::RenderFrame,
                &error,
            )));
        }
        check_accepted_deadline(
            &mut self.contract,
            deadline,
            WebRenderWindowFailureStage::RenderFrame,
        )?;
        self.active_stage = WebRenderWindowFailureStage::AwaitFrameRendered;
        if let Err(error) = rendered_waiter.wait_until(Checkpoint::FrameRendered, deadline) {
            return Err(self.latch_terminal(notification_error(
                WebRenderWindowFailureStage::AwaitFrameRendered,
                error,
            )));
        }
        check_accepted_deadline(
            &mut self.contract,
            deadline,
            WebRenderWindowFailureStage::AwaitFrameRendered,
        )?;

        self.active_stage = WebRenderWindowFailureStage::SwapBuffers;
        check_accepted_deadline(
            &mut self.contract,
            deadline,
            WebRenderWindowFailureStage::SwapBuffers,
        )?;
        let swap = {
            let presenter = self.presenter.as_mut().ok_or_else(owner_missing)?;
            presenter.swap_webrender_frame(direct_request)
        };
        if let Err(error) = swap {
            return Err(self.latch_terminal(WebRenderWindowError::presentation(
                WebRenderWindowFailureStage::SwapBuffers,
                &error,
            )));
        }
        finalize_successful_native_swap(
            &mut self.contract,
            deadline,
            request,
            evidence.publish_id.0,
            rgba8_bytes,
        )
    }

    fn validate_frame_ready(
        &mut self,
        observed: u64,
        evidence: FrameReadyEvidence,
    ) -> Result<(), WebRenderWindowError> {
        let expected_count = observed.checked_add(1).ok_or_else(|| {
            self.latch_terminal(WebRenderWindowError::new(
                WebRenderWindowFailureStage::AwaitFrameReady,
                WebRenderWindowErrorKind::NotificationOverflow,
                "frame-ready observation sequence is exhausted",
            ))
        })?;
        if evidence.count != expected_count
            || evidence.document_id != self.document_id
            || evidence.publish_id.0 == 0
            || !evidence.present
            || !evidence.render
            || !evidence.tracked
        {
            return Err(self.latch_terminal(WebRenderWindowError::new(
                WebRenderWindowFailureStage::AwaitFrameReady,
                WebRenderWindowErrorKind::Backend,
                format_args!(
                    "unexpected frame-ready evidence: count={} document={:?} publish={} present={} render={} tracked={}",
                    evidence.count,
                    evidence.document_id,
                    evidence.publish_id.0,
                    evidence.present,
                    evidence.render,
                    evidence.tracked
                ),
            )));
        }
        Ok(())
    }

    fn presenter_ref(&self) -> Result<&LinuxPresentedWindow, WebRenderWindowError> {
        self.presenter.as_ref().ok_or_else(owner_missing)
    }

    fn ensure_live_owners(&mut self) -> Result<(), WebRenderWindowError> {
        if self.presenter.is_some() && self.renderer.is_some() && self.api.is_some() {
            return Ok(());
        }
        let error = WebRenderWindowError::new(
            self.active_stage,
            WebRenderWindowErrorKind::TerminalState,
            "internally owned presenter, renderer, or API is absent",
        );
        Err(self.latch_terminal(error))
    }

    fn latch_terminal(&mut self, error: WebRenderWindowError) -> WebRenderWindowError {
        self.contract.lose(error.stage());
        error
    }

    fn latch_panic(&mut self, payload: &(dyn Any + Send)) -> WebRenderWindowError {
        let error = panic_error(self.active_stage, payload);
        self.contract.lose(self.active_stage);
        error
    }

    #[allow(clippy::too_many_lines)]
    fn cleanup(&mut self) -> Result<WebRenderWindowShutdownReport, WebRenderWindowShutdownFailure> {
        debug_assert!(!self.shutdown_complete);
        self.shutdown_complete = true;
        self.contract.shutdown();
        let mut primary = None;
        let mut text_release = RegistryRelease::default();
        let shutdown_deadline = Instant::now()
            .checked_add(self.contract.limits().shutdown_timeout())
            .unwrap_or_else(Instant::now);

        self.active_stage = WebRenderWindowFailureStage::ShutdownBackend;
        let mut release_transaction = Transaction::new();
        if let Some(api) = self.api.as_ref() {
            match self
                .text_registry
                .release_into(api, &mut release_transaction)
            {
                Ok(release) => text_release = release,
                Err(error) => {
                    primary.get_or_insert_with(|| {
                        text_error_at(WebRenderWindowFailureStage::ShutdownBackend, error)
                    });
                }
            }
        } else if self.renderer.is_some() {
            primary.get_or_insert_with(|| {
                WebRenderWindowError::new(
                    WebRenderWindowFailureStage::ShutdownBackend,
                    WebRenderWindowErrorKind::Backend,
                    "renderer exists without its internally owned WebRender API",
                )
            });
        }
        let document_id = self.document_id;
        let api_shutdown_ok = if let Some(api) = self.api.as_mut() {
            catch_unwind(AssertUnwindSafe(|| {
                if !release_transaction.is_empty() {
                    api.send_transaction(document_id, release_transaction);
                }
                api.delete_document(document_id);
                api.shut_down(false);
            }))
            .map_or_else(
                |payload| {
                    primary.get_or_insert_with(|| {
                        panic_error(
                            WebRenderWindowFailureStage::ShutdownBackend,
                            payload.as_ref(),
                        )
                    });
                    false
                },
                |()| true,
            )
        } else {
            self.renderer.is_none()
        };
        if api_shutdown_ok && self.renderer.is_some() {
            match self.notifier.wait_for_shutdown_until(shutdown_deadline) {
                Ok(()) => {
                    self.backend_shutdown_evidence = WebRenderTeardownEvidence::Confirmed;
                }
                Err(error) => {
                    primary.get_or_insert_with(|| {
                        notification_error(WebRenderWindowFailureStage::ShutdownBackend, error)
                    });
                }
            }
        }
        self.api.take();

        if !backend_ordering_established(self.renderer.is_some(), self.backend_shutdown_evidence) {
            let ordering_error = WebRenderWindowError::new(
                WebRenderWindowFailureStage::ShutdownBackend,
                WebRenderWindowErrorKind::Backend,
                "backend shutdown was not acknowledged; renderer and native owners retained",
            );
            let primary = primary.unwrap_or(ordering_error);
            if let Some(renderer) = self.renderer.take() {
                mem::forget(renderer);
            }
            let native_error = PresentationError::contract(
                PresentationFailureStage::ReleaseContext,
                PresentationErrorKind::Driver,
                "native owners retained because backend-to-renderer shutdown ordering was unproven",
            );
            let Some(presenter) = self.presenter.take() else {
                unreachable!("presenter disappeared before fail-closed retention")
            };
            let presentation = PresentationTeardownOutcome::RetainedAfterTeardownFailure(
                presenter.retain_after_webrender_failure(&native_error),
            );
            return Err(WebRenderWindowShutdownFailure::new(
                primary,
                self.backend_shutdown_evidence,
                self.renderer_deinitialization_evidence,
                presentation,
            ));
        }

        self.active_stage = WebRenderWindowFailureStage::DeinitializeRenderer;
        let context_activation = match self.presenter.as_mut() {
            Some(presenter) => presenter.make_current_for_webrender_teardown(),
            None => unreachable!("presenter ownership must exist before ordered teardown"),
        };
        if let Err(native_error) = context_activation {
            let mapped = WebRenderWindowError::presentation(
                WebRenderWindowFailureStage::DeinitializeRenderer,
                &native_error,
            );
            primary.get_or_insert(mapped.clone());
            if let Some(renderer) = self.renderer.take() {
                mem::forget(renderer);
            }
            let Some(presenter) = self.presenter.take() else {
                unreachable!("presenter disappeared during context activation")
            };
            let presentation = PresentationTeardownOutcome::RetainedAfterTeardownFailure(
                presenter.retain_after_webrender_failure(&native_error),
            );
            return Err(WebRenderWindowShutdownFailure::new(
                primary.unwrap_or(mapped),
                self.backend_shutdown_evidence,
                self.renderer_deinitialization_evidence,
                presentation,
            ));
        }

        match self.renderer.take() {
            Some(renderer) => match catch_unwind(AssertUnwindSafe(|| renderer.deinit())) {
                Ok(()) => {
                    self.renderer_deinitialization_evidence = WebRenderTeardownEvidence::Confirmed;
                }
                Err(payload) => {
                    let error = panic_error(
                        WebRenderWindowFailureStage::DeinitializeRenderer,
                        payload.as_ref(),
                    );
                    primary.get_or_insert(error.clone());
                    let native_error = PresentationError::contract(
                        PresentationFailureStage::ReleaseContext,
                        PresentationErrorKind::Driver,
                        "WebRender deinitialization panicked; native owners retained",
                    );
                    let Some(presenter) = self.presenter.take() else {
                        unreachable!("presenter disappeared during renderer deinit")
                    };
                    let presentation = PresentationTeardownOutcome::RetainedAfterTeardownFailure(
                        presenter.retain_after_webrender_failure(&native_error),
                    );
                    return Err(WebRenderWindowShutdownFailure::new(
                        primary.unwrap_or(error),
                        self.backend_shutdown_evidence,
                        self.renderer_deinitialization_evidence,
                        presentation,
                    ));
                }
            },
            None => {
                primary.get_or_insert_with(|| {
                    WebRenderWindowError::new(
                        WebRenderWindowFailureStage::DeinitializeRenderer,
                        WebRenderWindowErrorKind::InternalDrift,
                        "full window owner lost its renderer before deinitialization",
                    )
                });
            }
        }

        self.active_stage = WebRenderWindowFailureStage::ShutdownPresenter;
        let Some(presenter) = self.presenter.take() else {
            unreachable!("presenter ownership disappeared before final teardown")
        };
        let presentation_result = presenter.shutdown();
        let presentation = presentation_outcome(presentation_result);
        if let PresentationTeardownOutcome::RetainedAfterTeardownFailure(report) = presentation {
            primary.get_or_insert_with(|| {
                presentation_retention_error(
                    WebRenderWindowFailureStage::ShutdownPresenter,
                    report.failure_stage(),
                    report.failure_kind(),
                )
            });
        }
        if let Some(primary) = primary {
            return Err(WebRenderWindowShutdownFailure::new(
                primary,
                self.backend_shutdown_evidence,
                self.renderer_deinitialization_evidence,
                presentation,
            ));
        }
        let PresentationTeardownOutcome::WrappersReleased(presentation) = presentation else {
            unreachable!("retention establishes a primary error")
        };
        Ok(WebRenderWindowShutdownReport::new(
            self.backend_shutdown_evidence,
            self.renderer_deinitialization_evidence,
            text_release.font_templates(),
            text_release.font_instances(),
            text_release.font_bytes(),
            presentation,
        ))
    }

    fn retain_after_cleanup_panic(
        &mut self,
        payload: &(dyn Any + Send),
    ) -> WebRenderWindowShutdownFailure {
        let primary = panic_error(self.active_stage, payload);
        if let Some(renderer) = self.renderer.take() {
            mem::forget(renderer);
        }
        let native_error = PresentationError::contract(
            PresentationFailureStage::ReleaseContext,
            PresentationErrorKind::Driver,
            "panic escaped ordered WebRender teardown; uncertain native owners retained",
        );
        let Some(presenter) = self.presenter.take() else {
            unreachable!("teardown panic occurred after presenter ownership disappeared")
        };
        let presentation = PresentationTeardownOutcome::RetainedAfterTeardownFailure(
            presenter.retain_after_webrender_failure(&native_error),
        );
        self.shutdown_complete = true;
        self.contract.shutdown();
        WebRenderWindowShutdownFailure::new(
            primary,
            self.backend_shutdown_evidence,
            self.renderer_deinitialization_evidence,
            presentation,
        )
    }
}

impl Drop for WebRenderPresentedWindow {
    fn drop(&mut self) {
        if self.shutdown_complete {
            return;
        }
        if catch_unwind(AssertUnwindSafe(|| self.cleanup())).is_err() {
            if let Some(renderer) = self.renderer.take() {
                mem::forget(renderer);
            }
            if let Some(presenter) = self.presenter.take() {
                let error = PresentationError::contract(
                    PresentationFailureStage::ReleaseContext,
                    PresentationErrorKind::Driver,
                    "panic escaped WebRender window teardown during Drop",
                );
                let _ = presenter.retain_after_webrender_failure(&error);
            }
            self.shutdown_complete = true;
        }
    }
}

#[allow(clippy::too_many_lines)]
fn retire_partial_presenter(
    mut presenter: LinuxPresentedWindow,
    mut renderer: Option<Renderer>,
    api: Option<RenderApi>,
    document_id: Option<WebRenderDocumentId>,
    notifier: &WindowRenderNotifier,
    limits: WebRenderWindowLimits,
) -> Result<WebRenderWindowShutdownReport, WebRenderWindowShutdownFailure> {
    let renderer_started = renderer.is_some();
    let mut primary = None;
    let mut backend_shutdown = if renderer_started {
        WebRenderTeardownEvidence::Unknown
    } else {
        WebRenderTeardownEvidence::NotApplicable
    };
    let mut renderer_deinitialization = if renderer_started {
        WebRenderTeardownEvidence::Unknown
    } else {
        WebRenderTeardownEvidence::NotApplicable
    };
    if let Some(api) = api.as_ref() {
        let sent = catch_unwind(AssertUnwindSafe(|| {
            if let Some(document_id) = document_id {
                api.delete_document(document_id);
            }
            api.shut_down(false);
        }))
        .map_or_else(
            |payload| {
                primary = Some(panic_error(
                    WebRenderWindowFailureStage::ShutdownBackend,
                    payload.as_ref(),
                ));
                false
            },
            |()| true,
        );
        if sent {
            let deadline = Instant::now()
                .checked_add(limits.shutdown_timeout())
                .unwrap_or_else(Instant::now);
            match notifier.wait_for_shutdown_until(deadline) {
                Ok(()) => backend_shutdown = WebRenderTeardownEvidence::Confirmed,
                Err(error) => {
                    primary.get_or_insert_with(|| {
                        notification_error(WebRenderWindowFailureStage::ShutdownBackend, error)
                    });
                }
            }
        }
    } else if renderer_started {
        primary = Some(WebRenderWindowError::new(
            WebRenderWindowFailureStage::ShutdownBackend,
            WebRenderWindowErrorKind::Backend,
            "partial renderer existed without an API capable of requesting backend shutdown",
        ));
    }
    drop(api);

    if !backend_ordering_established(renderer_started, backend_shutdown) {
        let ordering_error = WebRenderWindowError::new(
            WebRenderWindowFailureStage::ShutdownBackend,
            WebRenderWindowErrorKind::Backend,
            "partial backend shutdown was not acknowledged; owners retained",
        );
        let primary = primary.unwrap_or(ordering_error);
        if let Some(renderer) = renderer.take() {
            mem::forget(renderer);
        }
        let native_error = PresentationError::contract(
            PresentationFailureStage::ReleaseContext,
            PresentationErrorKind::Driver,
            "partial native owners retained because backend ordering was unproven",
        );
        let presentation = PresentationTeardownOutcome::RetainedAfterTeardownFailure(
            presenter.retain_after_webrender_failure(&native_error),
        );
        return Err(WebRenderWindowShutdownFailure::new(
            primary,
            backend_shutdown,
            renderer_deinitialization,
            presentation,
        ));
    }

    if let Some(renderer) = renderer {
        if let Err(error) = presenter.make_current_for_webrender_teardown() {
            let mapped = WebRenderWindowError::presentation(
                WebRenderWindowFailureStage::DeinitializeRenderer,
                &error,
            );
            primary.get_or_insert(mapped.clone());
            mem::forget(renderer);
            let presentation = PresentationTeardownOutcome::RetainedAfterTeardownFailure(
                presenter.retain_after_webrender_failure(&error),
            );
            return Err(WebRenderWindowShutdownFailure::new(
                primary.unwrap_or(mapped),
                backend_shutdown,
                renderer_deinitialization,
                presentation,
            ));
        }
        match catch_unwind(AssertUnwindSafe(|| renderer.deinit())) {
            Ok(()) => {
                renderer_deinitialization = WebRenderTeardownEvidence::Confirmed;
            }
            Err(payload) => {
                let error = panic_error(
                    WebRenderWindowFailureStage::DeinitializeRenderer,
                    payload.as_ref(),
                );
                primary.get_or_insert(error.clone());
                let native_error = PresentationError::contract(
                    PresentationFailureStage::ReleaseContext,
                    PresentationErrorKind::Driver,
                    "partial WebRender deinitialization panicked",
                );
                let presentation = PresentationTeardownOutcome::RetainedAfterTeardownFailure(
                    presenter.retain_after_webrender_failure(&native_error),
                );
                return Err(WebRenderWindowShutdownFailure::new(
                    primary.unwrap_or(error),
                    backend_shutdown,
                    renderer_deinitialization,
                    presentation,
                ));
            }
        }
    }

    let presentation = presentation_outcome(presenter.shutdown());
    if let PresentationTeardownOutcome::RetainedAfterTeardownFailure(report) = presentation {
        primary.get_or_insert_with(|| {
            presentation_retention_error(
                WebRenderWindowFailureStage::ShutdownPresenter,
                report.failure_stage(),
                report.failure_kind(),
            )
        });
    }
    if let Some(primary) = primary {
        return Err(WebRenderWindowShutdownFailure::new(
            primary,
            backend_shutdown,
            renderer_deinitialization,
            presentation,
        ));
    }
    let PresentationTeardownOutcome::WrappersReleased(presentation) = presentation else {
        unreachable!("retention establishes a primary error")
    };
    Ok(WebRenderWindowShutdownReport::new(
        backend_shutdown,
        renderer_deinitialization,
        0,
        0,
        0,
        presentation,
    ))
}

fn scene_device_size(scene: &CompiledScene) -> Result<(u32, u32), WebRenderWindowError> {
    let viewport = scene.scene().viewport();
    if viewport.width() % APP_UNITS_PER_CSS_PIXEL != 0
        || viewport.height() % APP_UNITS_PER_CSS_PIXEL != 0
    {
        return Err(WebRenderWindowError::new(
            WebRenderWindowFailureStage::ValidateRequest,
            WebRenderWindowErrorKind::SizeMismatch,
            format_args!(
                "scene viewport {}x{} app units is not integral at scale one",
                viewport.width(),
                viewport.height()
            ),
        ));
    }
    let width = u32::try_from(viewport.width() / APP_UNITS_PER_CSS_PIXEL).map_err(|_| {
        WebRenderWindowError::new(
            WebRenderWindowFailureStage::ValidateRequest,
            WebRenderWindowErrorKind::SizeMismatch,
            "scene viewport width is not a positive device-pixel count",
        )
    })?;
    let height = u32::try_from(viewport.height() / APP_UNITS_PER_CSS_PIXEL).map_err(|_| {
        WebRenderWindowError::new(
            WebRenderWindowFailureStage::ValidateRequest,
            WebRenderWindowErrorKind::SizeMismatch,
            "scene viewport height is not a positive device-pixel count",
        )
    })?;
    Ok((width, height))
}

fn validate_browser_page_scene(
    page: &BrowserPageScene,
    geometry: crate::BrowserChromeGeometry,
    limits: WebRenderWindowLimits,
) -> Result<(), WebRenderWindowError> {
    let (width, height) = scene_device_size(page.scene())?;
    if (width, height) != (geometry.content().width(), geometry.content().height()) {
        return Err(WebRenderWindowError::new(
            WebRenderWindowFailureStage::ValidateRequest,
            WebRenderWindowErrorKind::SizeMismatch,
            format_args!(
                "page viewport {width}x{height} differs from exact clipped content target {}x{}",
                geometry.content().width(),
                geometry.content().height(),
            ),
        ));
    }
    for (resource, observed, limit) in [
        (
            "page scene items",
            page.scene().scene().items().len(),
            limits.max_scene_items(),
        ),
        (
            "page pending text runs",
            page.scene().scene().pending_text().len(),
            limits.max_pending_text_runs(),
        ),
        (
            "page display-list bytes",
            page.scene().built_display_list().size_in_bytes(),
            limits.max_display_list_bytes(),
        ),
    ] {
        if observed > limit {
            return Err(WebRenderWindowError::new(
                WebRenderWindowFailureStage::ValidateRequest,
                WebRenderWindowErrorKind::ResourceLimit,
                format_args!("{resource} {observed} exceeds fixed limit {limit}"),
            ));
        }
    }
    Ok(())
}

fn page_scene_text_descriptors(
    page: &BrowserPageScene,
) -> Result<Vec<SceneTextDescriptor<'_>>, WebRenderWindowError> {
    let mut descriptors = Vec::new();
    descriptors
        .try_reserve_exact(page.texts().len())
        .map_err(|_| {
            WebRenderWindowError::new(
                WebRenderWindowFailureStage::ComposeScene,
                WebRenderWindowErrorKind::ResourceLimit,
                "could not reserve page text descriptors",
            )
        })?;
    descriptors.extend(page.texts().iter().map(|text| {
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
    }));
    Ok(descriptors)
}

fn validate_shaped_text_count(
    observed: usize,
    expected: usize,
) -> Result<(), WebRenderWindowError> {
    if observed == expected {
        return Ok(());
    }
    Err(WebRenderWindowError::new(
        WebRenderWindowFailureStage::ValidateRequest,
        WebRenderWindowErrorKind::Text,
        format_args!(
            "shaped text count {observed} differs from exact pending scene count {expected}"
        ),
    ))
}

fn retired_browser_page_pipeline(candidate: BrowserCandidate) -> Option<PipelineId> {
    if !candidate.page_replaced {
        return None;
    }
    let BrowserPageSnapshot::Scene(previous) = candidate.previous_page else {
        return None;
    };
    if matches!(candidate.page, BrowserPageSnapshot::Scene(next) if next.pipeline() == previous.pipeline())
    {
        return None;
    }
    Some(PipelineId(
        previous.pipeline().source(),
        previous.pipeline().pipeline(),
    ))
}

#[allow(clippy::too_many_arguments)]
fn validate_browser_pipeline_publication(
    publication: &PipelineInfo,
    document_id: WebRenderDocumentId,
    pipelines: BrowserPipelines,
    expected_root_epoch: u32,
    expected_chrome_epoch: u32,
    expected_page: BrowserPageSnapshot,
    expected_page_epoch: Option<u32>,
    expected_retired_page: Option<PipelineId>,
) -> Result<(), WebRenderWindowError> {
    let epoch = |pipeline| {
        publication
            .epochs
            .get(&(pipeline, document_id))
            .map(|epoch| epoch.0)
    };
    let root_epoch = epoch(pipelines.root());
    let chrome_epoch = epoch(pipelines.chrome());
    let (page_pipeline, page_epoch) = match expected_page {
        BrowserPageSnapshot::Blank => (None, None),
        BrowserPageSnapshot::Scene(page) => {
            let pipeline = PipelineId(page.pipeline().source(), page.pipeline().pipeline());
            (Some(pipeline), epoch(pipeline))
        }
    };
    let retired_cached_epoch = expected_retired_page.and_then(epoch);
    let expected_removal = expected_retired_page.map(|pipeline| (pipeline, document_id));
    let removals_match = expected_removal
        .map_or(publication.removed_pipelines.is_empty(), |expected| {
            publication.removed_pipelines.as_slice() == [expected]
        });
    if root_epoch == Some(expected_root_epoch)
        && chrome_epoch == Some(expected_chrome_epoch)
        && page_epoch == expected_page_epoch
        && page_pipeline.is_some() == expected_page_epoch.is_some()
        && removals_match
    {
        return Ok(());
    }
    Err(WebRenderWindowError::new(
        WebRenderWindowFailureStage::VerifyEpoch,
        WebRenderWindowErrorKind::Renderer,
        format_args!(
            "browser pipeline publication mismatch: root={root_epoch:?}/{expected_root_epoch} chrome={chrome_epoch:?}/{expected_chrome_epoch} page={page_epoch:?}/{expected_page_epoch:?} retired_cache={retired_cached_epoch:?} removals={:?}/{expected_removal:?}",
            publication.removed_pipelines,
        ),
    ))
}

fn with_validated_pipeline<T>(
    request: WebRenderWindowFrameRequest,
    compiled_pipeline: PipelineKey,
    on_exact_pipeline: impl FnOnce() -> Result<T, WebRenderWindowError>,
) -> Result<T, WebRenderWindowError> {
    WebRenderWindowContract::validate_pipeline(request, compiled_pipeline)?;
    on_exact_pipeline()
}

fn admitted_native_error(
    stage: WebRenderWindowFailureStage,
    error: &PresentationError,
) -> WebRenderWindowError {
    let mapped = WebRenderWindowError::presentation(stage, error);
    if mapped.is_terminal() {
        return mapped;
    }
    WebRenderWindowError::new(
        stage,
        WebRenderWindowErrorKind::InternalDrift,
        format_args!(
            "outer contract admitted the exact operation but native presenter rejected {:?}/{:?}: {}",
            error.stage(),
            error.kind(),
            error.detail()
        ),
    )
}

fn terminalize_accepted_browser_error(
    contract: &mut WebRenderWindowContract,
    browser: &mut BrowserCompositorContract,
    error: WebRenderWindowError,
) -> WebRenderWindowError {
    debug_assert!(browser.accepted_in_flight());
    browser.fail_after_acceptance();
    let terminal = if error.is_terminal() {
        error
    } else {
        WebRenderWindowError::new(
            error.stage(),
            WebRenderWindowErrorKind::InternalDrift,
            format_args!(
                "accepted browser transaction returned nonterminal {:?}: {}",
                error.kind(),
                error.detail(),
            ),
        )
    };
    contract.lose(terminal.stage());
    terminal
}

fn check_accepted_deadline(
    contract: &mut WebRenderWindowContract,
    deadline: Instant,
    stage: WebRenderWindowFailureStage,
) -> Result<(), WebRenderWindowError> {
    check_deadline(deadline, stage).inspect_err(|_| {
        contract.lose(stage);
    })
}

fn finalize_successful_native_swap(
    contract: &mut WebRenderWindowContract,
    deadline: Instant,
    request: WebRenderWindowFrameRequest,
    backend_publish_id: u64,
    rgba8_byte_equivalent: u64,
) -> Result<WebRenderWindowFrameReceipt, WebRenderWindowError> {
    contract.commit_swap(request.sequence());
    check_accepted_deadline(contract, deadline, WebRenderWindowFailureStage::SwapBuffers)?;
    Ok(WebRenderWindowFrameReceipt::new(
        request,
        backend_publish_id,
        rgba8_byte_equivalent,
    ))
}

const fn backend_ordering_established(
    renderer_started: bool,
    backend_shutdown: WebRenderTeardownEvidence,
) -> bool {
    !renderer_started || matches!(backend_shutdown, WebRenderTeardownEvidence::Confirmed)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StartupFailureClass {
    PreWorkerRendererRejection,
    ConstructorThreadFailure,
    ConstructorPanic,
    ApiCreationPanic,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StartupFailureDisposition {
    ReturnStructuredFailure,
    AbortOwningProcess,
}

const fn startup_failure_disposition(class: StartupFailureClass) -> StartupFailureDisposition {
    match class {
        StartupFailureClass::PreWorkerRendererRejection => {
            StartupFailureDisposition::ReturnStructuredFailure
        }
        StartupFailureClass::ConstructorThreadFailure
        | StartupFailureClass::ConstructorPanic
        | StartupFailureClass::ApiCreationPanic => StartupFailureDisposition::AbortOwningProcess,
    }
}

fn renderer_startup_disposition(error: &RendererError) -> StartupFailureDisposition {
    let class = match error {
        RendererError::Thread(_) => StartupFailureClass::ConstructorThreadFailure,
        RendererError::Shader(_)
        | RendererError::MaxTextureSize
        | RendererError::SoftwareRasterizer
        | RendererError::OutOfMemory => StartupFailureClass::PreWorkerRendererRejection,
    };
    startup_failure_disposition(class)
}

fn webrender_options(capabilities: LinuxPresentationCapabilities) -> WebRenderOptions {
    WebRenderOptions {
        clear_color: ColorF::new(1.0, 1.0, 1.0, 1.0),
        enable_dithering: false,
        enable_subpixel_aa: false,
        max_internal_texture_size: Some(MAX_RENDER_TASK_SIZE),
        testing: false,
        enable_gpu_markers: false,
        enable_debugger: false,
        panic_on_gl_error: false,
        reject_software_rasterizer: capabilities.acceleration()
            == LinuxAccelerationClass::Accelerated,
        ..WebRenderOptions::default()
    }
}

/// Terminates without formatting, allocation, unwinding, or fallible I/O.
///
/// In particular, do not add diagnostics before `abort`: startup reaches this
/// path only when imported worker cleanup cannot be proved, and even an
/// unusable standard-error stream must not divert control into a panic.
#[cold]
fn abort_unproven_startup(_class: StartupFailureClass) -> ! {
    process::abort()
}

fn device_size(size: PhysicalSize) -> DeviceIntSize {
    DeviceIntSize::new(size.width.cast_signed(), size.height.cast_signed())
}

fn scene_error(error: SceneBuildError) -> WebRenderWindowError {
    WebRenderWindowError::new(
        WebRenderWindowFailureStage::ComposeScene,
        WebRenderWindowErrorKind::Scene,
        error,
    )
}

fn text_error(error: TextRenderError) -> WebRenderWindowError {
    text_error_at(WebRenderWindowFailureStage::ComposeScene, error)
}

fn text_error_at(
    stage: WebRenderWindowFailureStage,
    error: TextRenderError,
) -> WebRenderWindowError {
    WebRenderWindowError::new(stage, WebRenderWindowErrorKind::Text, error)
}

fn notification_error(
    stage: WebRenderWindowFailureStage,
    error: NotificationWaitError,
) -> WebRenderWindowError {
    let kind = match error {
        NotificationWaitError::Timeout => WebRenderWindowErrorKind::Timeout,
        NotificationWaitError::TransactionDropped => WebRenderWindowErrorKind::TransactionDropped,
        NotificationWaitError::Disconnected
        | NotificationWaitError::WrongCheckpoint
        | NotificationWaitError::UnexpectedExternalEvent => WebRenderWindowErrorKind::Backend,
        NotificationWaitError::Overflow => WebRenderWindowErrorKind::NotificationOverflow,
    };
    WebRenderWindowError::new(stage, kind, format_args!("notification failure: {error:?}"))
}

fn check_deadline(
    deadline: Instant,
    stage: WebRenderWindowFailureStage,
) -> Result<(), WebRenderWindowError> {
    if deadline.checked_duration_since(Instant::now()).is_some() {
        Ok(())
    } else {
        Err(WebRenderWindowError::new(
            stage,
            WebRenderWindowErrorKind::Timeout,
            "frame exceeded the one total build/render deadline",
        ))
    }
}

fn owner_missing() -> WebRenderWindowError {
    WebRenderWindowError::new(
        WebRenderWindowFailureStage::ShutdownPresenter,
        WebRenderWindowErrorKind::TerminalState,
        "internally owned renderer or presenter is absent",
    )
}

fn panic_error(
    stage: WebRenderWindowFailureStage,
    payload: &(dyn Any + Send),
) -> WebRenderWindowError {
    WebRenderWindowError::new(
        stage,
        WebRenderWindowErrorKind::Panic,
        bounded_panic_payload(payload),
    )
}

fn bounded_panic_payload(payload: &(dyn Any + Send)) -> String {
    const MAX_PANIC_BYTES: usize = 2_048;
    let detail = if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic at WebRender window boundary".to_owned()
    };
    if detail.len() <= MAX_PANIC_BYTES {
        return detail;
    }
    let mut boundary = MAX_PANIC_BYTES - 3;
    while !detail.is_char_boundary(boundary) {
        boundary -= 1;
    }
    let mut bounded = detail[..boundary].to_owned();
    bounded.push_str("...");
    bounded
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::fs::OpenOptions;
    use std::io::{self, Write};
    use std::os::unix::process::ExitStatusExt;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use webrender::{PipelineInfo, RendererError, ShaderError};
    use webrender_api::{DocumentId as WebRenderDocumentId, Epoch, IdNamespace, PipelineId};
    use wild_buzzard_dom::Document;
    use wild_buzzard_platform::{
        PhysicalSize, PixelFormat, ScaleFactor, SurfaceDescriptor, SurfaceIdAllocator,
        SurfaceNamespace, SurfaceRole,
    };
    use wild_buzzard_renderer::PipelineKey;
    use wild_buzzard_text_webrender::TextRegistryStatistics;

    use super::{
        StartupFailureClass, StartupFailureDisposition, abort_unproven_startup,
        admitted_native_error, backend_ordering_established, check_accepted_deadline,
        finalize_successful_native_swap, renderer_startup_disposition, startup_failure_disposition,
        terminalize_accepted_browser_error, validate_browser_pipeline_publication,
        validate_shaped_text_count, webrender_options, with_validated_pipeline,
    };
    use crate::browser_compositor::{
        BrowserCompositorContract, BrowserPageSnapshot, BrowserPipelines,
    };
    use crate::window_contract::WebRenderWindowContract;
    use crate::{
        LinuxAccelerationClass, LinuxPresentationCapabilities, LinuxResetProtection,
        PresentationError, PresentationErrorKind, PresentationFailureStage,
        WebRenderTeardownEvidence, WebRenderWindowErrorKind, WebRenderWindowFailureStage,
        WebRenderWindowFrameRequest, WebRenderWindowState,
    };

    fn descriptor() -> SurfaceDescriptor {
        let mut allocator =
            SurfaceIdAllocator::new(SurfaceNamespace::new(8_411).expect("nonzero namespace"));
        SurfaceDescriptor {
            id: allocator.allocate().expect("surface identity"),
            size: PhysicalSize::new(640, 480).expect("bounded size"),
            scale: ScaleFactor::new(1.0).expect("valid scale"),
            format: PixelFormat::Rgba8Srgb,
            role: SurfaceRole::Window,
        }
    }

    fn request(
        contract: &WebRenderWindowContract,
        document: &Document,
        pipeline: PipelineKey,
        sequence: u64,
    ) -> WebRenderWindowFrameRequest {
        WebRenderWindowFrameRequest::new(
            contract.snapshot(),
            document.version(),
            pipeline,
            1,
            sequence,
        )
    }

    #[test]
    fn webrender_software_rejection_is_bound_only_to_acceleration_class() {
        for reset in [
            LinuxResetProtection::LoseContextOnReset,
            LinuxResetProtection::Unavailable,
        ] {
            assert!(
                webrender_options(LinuxPresentationCapabilities::new(
                    LinuxAccelerationClass::Accelerated,
                    reset,
                ))
                .reject_software_rasterizer
            );
            assert!(
                !webrender_options(LinuxPresentationCapabilities::new(
                    LinuxAccelerationClass::Software,
                    reset,
                ))
                .reject_software_rasterizer
            );
        }
    }

    #[test]
    fn hostile_shaped_text_count_is_rejected_before_descriptor_reservation() {
        let error = validate_shaped_text_count(usize::MAX, 3)
            .expect_err("oversized foreign text slice must be rejected");
        assert_eq!(error.kind(), WebRenderWindowErrorKind::Text);
    }

    #[test]
    fn exact_shaped_text_count_is_admitted() {
        validate_shaped_text_count(3, 3).expect("exact shaped text inventory");
    }

    fn blank_browser_publication(
        document_id: WebRenderDocumentId,
        pipelines: BrowserPipelines,
        retired: PipelineId,
        removals: Vec<(PipelineId, WebRenderDocumentId)>,
    ) -> PipelineInfo {
        let mut publication = PipelineInfo::default();
        publication
            .epochs
            .insert((pipelines.root(), document_id), Epoch(3));
        publication
            .epochs
            .insert((pipelines.chrome(), document_id), Epoch(3));
        // `Renderer::current_epoch` historically retained this stale value.
        // Exact backend removal evidence, not cache absence, proves retirement.
        publication.epochs.insert((retired, document_id), Epoch(2));
        publication.removed_pipelines = removals;
        publication
    }

    #[test]
    fn stale_retired_epoch_with_exact_backend_removal_is_admitted() {
        let document_id = WebRenderDocumentId::new(IdNamespace(91), 1);
        let pipelines = BrowserPipelines::new(91);
        let retired = PipelineId(91, 7);
        let publication = blank_browser_publication(
            document_id,
            pipelines,
            retired,
            vec![(retired, document_id)],
        );

        validate_browser_pipeline_publication(
            &publication,
            document_id,
            pipelines,
            3,
            3,
            BrowserPageSnapshot::Blank,
            None,
            Some(retired),
        )
        .expect("exact removal makes the stale renderer epoch cache nonauthoritative");
    }

    #[test]
    fn missing_or_wrong_backend_page_removal_is_rejected() {
        let document_id = WebRenderDocumentId::new(IdNamespace(91), 1);
        let pipelines = BrowserPipelines::new(91);
        let retired = PipelineId(91, 7);
        let wrong = PipelineId(91, 8);
        for removals in [
            Vec::new(),
            vec![(wrong, document_id)],
            vec![(retired, document_id), (wrong, document_id)],
        ] {
            let publication = blank_browser_publication(document_id, pipelines, retired, removals);
            let error = validate_browser_pipeline_publication(
                &publication,
                document_id,
                pipelines,
                3,
                3,
                BrowserPageSnapshot::Blank,
                None,
                Some(retired),
            )
            .expect_err("backend removal must be present, exact, and exclusive");
            assert_eq!(error.stage(), WebRenderWindowFailureStage::VerifyEpoch);
            assert_eq!(error.kind(), WebRenderWindowErrorKind::Renderer);
            assert!(error.is_terminal());
        }
    }

    #[test]
    fn renderer_owners_require_explicit_backend_shutdown_acknowledgement() {
        assert!(!backend_ordering_established(
            true,
            WebRenderTeardownEvidence::Unknown
        ));
        assert!(!backend_ordering_established(
            true,
            WebRenderTeardownEvidence::NotApplicable
        ));
        assert!(backend_ordering_established(
            true,
            WebRenderTeardownEvidence::Confirmed
        ));
        assert!(backend_ordering_established(
            false,
            WebRenderTeardownEvidence::NotApplicable
        ));
    }

    #[test]
    fn startup_failure_policy_never_returns_with_unproven_workers() {
        assert_eq!(
            startup_failure_disposition(StartupFailureClass::PreWorkerRendererRejection),
            StartupFailureDisposition::ReturnStructuredFailure
        );
        for class in [
            StartupFailureClass::ConstructorThreadFailure,
            StartupFailureClass::ConstructorPanic,
            StartupFailureClass::ApiCreationPanic,
        ] {
            assert_eq!(
                startup_failure_disposition(class),
                StartupFailureDisposition::AbortOwningProcess
            );
        }
        for error in [
            RendererError::Shader(ShaderError::Compilation(
                "injected".to_owned(),
                "rejected".to_owned(),
            )),
            RendererError::MaxTextureSize,
            RendererError::SoftwareRasterizer,
            RendererError::OutOfMemory,
        ] {
            assert_eq!(
                renderer_startup_disposition(&error),
                StartupFailureDisposition::ReturnStructuredFailure
            );
        }
        assert_eq!(
            renderer_startup_disposition(&RendererError::Thread(io::Error::other(
                "injected post-spawn thread failure"
            ))),
            StartupFailureDisposition::AbortOwningProcess
        );
    }

    #[test]
    fn abort_unproven_startup_reaches_sigabrt_with_unusable_stderr() {
        const CHILD_ENV: &str = "WILDBUZZARD_ABORT_UNPROVEN_STARTUP_CHILD";
        const CHILD_TEST: &str = concat!(
            "webrender_window::tests::",
            "abort_unproven_startup_reaches_sigabrt_with_unusable_stderr"
        );
        const PANIC_EXIT_CODE: i32 = 86;
        const LINUX_SIGABRT: i32 = 6;

        if std::env::var_os(CHILD_ENV).is_some() {
            std::panic::set_hook(Box::new(|_| std::process::exit(PANIC_EXIT_CODE)));
            abort_unproven_startup(StartupFailureClass::ConstructorThreadFailure);
        }

        let mut unusable_stderr = OpenOptions::new()
            .write(true)
            .open("/dev/full")
            .expect("the supported Linux target provides /dev/full");
        assert!(
            unusable_stderr
                .write_all(b"stderr rejection probe")
                .is_err(),
            "/dev/full must reject writes for this regression"
        );

        let status = Command::new(std::env::current_exe().expect("current unit-test executable"))
            .arg("--exact")
            .arg(CHILD_TEST)
            .arg("--nocapture")
            .env(CHILD_ENV, "1")
            .current_dir(std::env::temp_dir())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(unusable_stderr))
            .status()
            .expect("abort regression child must launch");

        assert!(
            status.code().is_none(),
            "abort child returned an ordinary exit code: {status:?}"
        );
        assert_eq!(
            status.signal(),
            Some(LINUX_SIGABRT),
            "abort child did not terminate through process::abort: {status:?}"
        );
    }

    #[test]
    fn pipeline_mismatch_precedes_compose_and_allocation_callbacks() {
        let contract = WebRenderWindowContract::new(descriptor());
        let document = Document::new();
        let requested = PipelineKey::new(31, 7);
        let request = request(&contract, &document, requested, 1);
        let compose_calls = Cell::new(0_u32);
        let api_allocations = Cell::new(0_u32);
        let before = TextRegistryStatistics::default();

        let error = with_validated_pipeline(request, PipelineKey::new(31, 8), || {
            compose_calls.set(compose_calls.get() + 1);
            api_allocations.set(api_allocations.get() + 1);
            Ok(())
        })
        .expect_err("foreign compiled pipeline must reject before continuation");

        assert_eq!(error.stage(), WebRenderWindowFailureStage::ValidateRequest);
        assert_eq!(error.kind(), WebRenderWindowErrorKind::PipelineMismatch);
        assert_eq!(compose_calls.get(), 0);
        assert_eq!(api_allocations.get(), 0);
        assert_eq!(before, TextRegistryStatistics::default());
    }

    #[test]
    fn contradictory_native_admission_is_terminal_internal_drift() {
        let native = PresentationError::contract(
            PresentationFailureStage::ValidateSurface,
            PresentationErrorKind::SurfaceMismatch,
            "injected native identity drift",
        );
        let error = admitted_native_error(WebRenderWindowFailureStage::ValidateRequest, &native);
        assert_eq!(error.kind(), WebRenderWindowErrorKind::InternalDrift);
        assert!(error.is_terminal());
    }

    #[test]
    fn every_post_accept_browser_error_is_externally_terminal_and_loses_owner() {
        let mut contract = WebRenderWindowContract::new(descriptor());
        let mut browser = BrowserCompositorContract::default();
        browser.mark_accepted();
        let injected = super::WebRenderWindowError::new(
            WebRenderWindowFailureStage::ComposeScene,
            WebRenderWindowErrorKind::Text,
            "injected nonterminal text error after transaction acceptance",
        );

        let error = terminalize_accepted_browser_error(&mut contract, &mut browser, injected);

        assert_eq!(error.kind(), WebRenderWindowErrorKind::InternalDrift);
        assert!(error.is_terminal());
        assert_eq!(
            contract.state(),
            WebRenderWindowState::Lost(WebRenderWindowFailureStage::ComposeScene)
        );
        assert!(!browser.accepted_in_flight());
        assert!(
            browser
                .hit_test(
                    wild_buzzard_platform::PhysicalPoint { x: 0, y: 0 },
                    contract.snapshot(),
                )
                .is_err()
        );
    }

    #[test]
    fn late_pre_swap_boundaries_latch_exact_stage_without_sequence_commit() {
        let expired = Instant::now()
            .checked_sub(Duration::from_millis(1))
            .expect("one millisecond is representable");
        for stage in [
            WebRenderWindowFailureStage::PrepareNativeFrame,
            WebRenderWindowFailureStage::UpdateRenderer,
            WebRenderWindowFailureStage::RenderFrame,
            WebRenderWindowFailureStage::SwapBuffers,
        ] {
            let mut contract = WebRenderWindowContract::new(descriptor());
            let error = check_accepted_deadline(&mut contract, expired, stage)
                .expect_err("injected slow synchronous boundary must expire");
            assert_eq!(error.stage(), stage);
            assert_eq!(error.kind(), WebRenderWindowErrorKind::Timeout);
            assert_eq!(contract.state(), WebRenderWindowState::Lost(stage));
            assert_eq!(contract.last_sequence_for_test(), None);
            assert_eq!(contract.submitted_frames_for_test(), 0);
        }
    }

    #[test]
    fn late_successful_swap_commits_accounting_then_returns_no_receipt() {
        let mut contract = WebRenderWindowContract::new(descriptor());
        let document = Document::new();
        let request = request(&contract, &document, PipelineKey::new(44, 2), 9);
        contract.commit_transaction(request);
        let expired = Instant::now()
            .checked_sub(Duration::from_millis(1))
            .expect("one millisecond is representable");

        let error = finalize_successful_native_swap(&mut contract, expired, request, 1, 1_228_800)
            .expect_err("late accepted native swap must not publish a receipt");

        assert_eq!(error.stage(), WebRenderWindowFailureStage::SwapBuffers);
        assert_eq!(error.kind(), WebRenderWindowErrorKind::Timeout);
        assert_eq!(
            contract.state(),
            WebRenderWindowState::Lost(WebRenderWindowFailureStage::SwapBuffers)
        );
        assert_eq!(contract.last_sequence_for_test(), Some(9));
        assert_eq!(contract.submitted_frames_for_test(), 1);
    }
}
