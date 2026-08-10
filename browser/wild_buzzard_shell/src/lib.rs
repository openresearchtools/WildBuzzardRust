//! Same-process Rust browser-product integration for one Linux top-level window.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use wild_buzzard_engine::{
    DocumentOperationFailure, DocumentOperationId, EngineLimits, MAX_NAVIGATION_URL_BYTES,
    NavigationId, StaticPageConfig,
};
use wild_buzzard_linux::{
    BrowserAddressSelection, BrowserChromeFocus, BrowserChromeGeometry, BrowserChromeRevision,
    BrowserChromeScene, BrowserChromeState, BrowserChromeTab, BrowserFrameReceipt,
    BrowserFrameRequest, BrowserHitTarget, BrowserNavigationIdentity, BrowserPageSnapshot,
    BrowserPageUpdate, BrowserTabIdentity, ControlError, LinuxBackend, LinuxPresentationMode,
    LinuxPresentationShutdown, LinuxShellConfig, LinuxShutdownReport, LinuxStopReason,
    LinuxWakeHandle, LinuxWakeStatus, LinuxWindowControl, LinuxWindowEvent, LinuxWindowHandler,
    LinuxWindowShell, MAX_BROWSER_CHROME_GLYPHS, MAX_BROWSER_CHROME_RUNS, MAX_BROWSER_CHROME_TABS,
    MAX_BROWSER_CHROME_TEXT_BYTES, MAX_BROWSER_CHROME_TEXTS, PhysicalPoint, PhysicalSize,
    SurfaceNamespace, WebRenderSurfaceSnapshot,
};
use wild_buzzard_platform::{InputEvent, PointerPhase};
use wild_buzzard_text::{TextLimits, TextRequest, TextShutdownReport, TextSystem};
use wild_buzzard_ui::{
    BrowserCommandOutcome, BrowserSession, BrowserTabId, BrowserWindowId, EngineDocumentVersion,
    EnginePortExecutorShutdown, EnginePortShutdownStatus, EnginePortStopReason, EnginePumpOutcome,
    LinuxEventOutcome, NavigationEnginePort, SessionLifecycle, SessionLimits,
};

const POLL_INTERVAL: Duration = Duration::from_millis(10);
const SMOKE_HOLD: Duration = Duration::from_secs(3);
const TAB_FONT_SIZE_PX: f32 = 14.0;
const ADDRESS_FONT_SIZE_PX: f32 = 16.0;
const STATUS_FONT_SIZE_PX: f32 = 13.0;
const PRIMARY_POINTER_BUTTON: u16 = 1;
const MAX_CONSECUTIVE_PREACCEPT_REJECTIONS: u8 = 8;
// Chrome is a projection of canonical session state. These visual limits make
// every session state admitted by this shell representable by the compositor.
const MAX_TAB_LABEL_BYTES: usize = 32;
const MAX_ADDRESS_LABEL_BYTES: usize = 1_792;
const MAX_STATUS_LABEL_BYTES: usize = 256;
const MAX_CHROME_LABEL_BYTES: usize = MAX_BROWSER_CHROME_TABS * MAX_TAB_LABEL_BYTES
    + MAX_ADDRESS_LABEL_BYTES
    + MAX_STATUS_LABEL_BYTES;
// Navigation identities are never reused. The lookup retains only exact live
// engine navigations; old page scenes and receipts copy the value they need.
const MAX_GRAPHICS_NAVIGATIONS: usize = 4_096;

const _: () = assert!(MAX_CHROME_LABEL_BYTES <= MAX_BROWSER_CHROME_TEXT_BYTES);
const _: () = assert!(MAX_CHROME_LABEL_BYTES <= MAX_BROWSER_CHROME_RUNS);
const _: () = assert!(MAX_CHROME_LABEL_BYTES <= MAX_BROWSER_CHROME_GLYPHS);
const _: () = assert!(MAX_BROWSER_CHROME_TABS + 2 <= MAX_BROWSER_CHROME_TEXTS);

fn shell_session_limits() -> SessionLimits {
    SessionLimits::new(
        1,
        MAX_BROWSER_CHROME_TABS,
        MAX_BROWSER_CHROME_TABS,
        MAX_BROWSER_CHROME_TABS,
        50,
        64 * 1024 * 1024,
        256 * 1024 * 1024,
        MAX_NAVIGATION_URL_BYTES,
        256,
    )
    .expect("browser-shell limits are within the UI hard ceilings")
}

fn bounded_utf8_prefix(value: &str, maximum: usize) -> &str {
    if value.len() <= maximum {
        return value;
    }
    let mut end = maximum;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

fn retry_browser_frame_after(error: ControlError) -> bool {
    error.browser_presentation_terminal() == Some(false)
}

const fn command_requests_native_exit(outcome: BrowserCommandOutcome) -> bool {
    matches!(
        outcome,
        BrowserCommandOutcome::SessionClosed { .. }
            | BrowserCommandOutcome::WindowClosed { .. }
            | BrowserCommandOutcome::TabClosed {
                window_closed: true,
                ..
            }
    )
}

const fn routed_outcome_requests_native_exit(outcome: &LinuxEventOutcome) -> bool {
    matches!(
        outcome,
        LinuxEventOutcome::Command(command) if command_requests_native_exit(*command)
    )
}

const fn command_requires_engine_poll(outcome: BrowserCommandOutcome) -> bool {
    matches!(outcome, BrowserCommandOutcome::NavigationQueued { .. })
        || matches!(
            outcome,
            BrowserCommandOutcome::TabClosed {
                window_closed: false,
                ..
            } | BrowserCommandOutcome::WindowClosed { .. }
        )
}

const fn routed_outcome_requires_engine_poll(outcome: &LinuxEventOutcome) -> bool {
    matches!(
        outcome,
        LinuxEventOutcome::Command(command) if command_requires_engine_poll(*command)
    )
}

const fn routed_outcome_mutates_chrome(outcome: &LinuxEventOutcome) -> bool {
    matches!(outcome, LinuxEventOutcome::AddressEdited { .. })
        || matches!(
            outcome,
            LinuxEventOutcome::Command(command)
                if !matches!(command, BrowserCommandOutcome::NoChange)
        )
}

const fn input_requires_redraw(outcome: &LinuxEventOutcome, page_hit_applied: bool) -> bool {
    page_hit_applied || !matches!(outcome, LinuxEventOutcome::ContentInputUnrouted { .. })
}

const fn pointer_has_chrome_action_authority(
    pointer: &wild_buzzard_platform::PointerEvent,
) -> bool {
    // The normalized event currently exposes the aggregate button set, not
    // the button which changed. Exact-primary is the only unambiguous chrome
    // activation authority; mixed chords fail closed.
    matches!(pointer.phase, PointerPhase::Down) && pointer.buttons == PRIMARY_POINTER_BUTTON
}

const fn viewport_matches_engine(
    engine: Option<PhysicalSize>,
    content: Option<PhysicalSize>,
) -> bool {
    match (engine, content) {
        (Some(engine), Some(content)) => {
            engine.width == content.width && engine.height == content.height
        }
        _ => false,
    }
}

const fn rerender_terminal_requires_suppression(
    failure: Option<DocumentOperationFailure>,
    frame_present: bool,
) -> bool {
    failure.is_some() || !frame_present
}

const fn native_stop_is_admitted(
    reason: LinuxStopReason,
    smoke_requested: bool,
    smoke_completed: bool,
) -> bool {
    if smoke_requested {
        is_completed_smoke_exit(reason, smoke_completed)
    } else {
        matches!(
            reason,
            LinuxStopReason::Requested | LinuxStopReason::CloseRequested
        )
    }
}

const fn engine_shutdown_is_admitted(status: EnginePortShutdownStatus) -> bool {
    matches!(status.reason(), EnginePortStopReason::Requested)
        && matches!(status.executor(), EnginePortExecutorShutdown::Clean)
}

const fn surface_is_drawable(size: PhysicalSize) -> bool {
    size.width != 0 && size.height != 0
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PageFallback {
    ClearAndRerender,
    Retain { need_rerender: bool },
}

const fn select_page_fallback(
    presented_scene: bool,
    scene_invalidated: bool,
    awaiting_live_frame: bool,
) -> PageFallback {
    if presented_scene && scene_invalidated {
        PageFallback::ClearAndRerender
    } else {
        PageFallback::Retain {
            need_rerender: !presented_scene && awaiting_live_frame,
        }
    }
}

const fn materialize_page_fallback(
    fallback: PageFallback,
    presented: BrowserPageSnapshot,
) -> (BrowserPageUpdate, BrowserPageSnapshot, bool) {
    match fallback {
        PageFallback::ClearAndRerender => (
            BrowserPageUpdate::ClearToBlank,
            BrowserPageSnapshot::Blank,
            true,
        ),
        PageFallback::Retain { need_rerender } => {
            (BrowserPageUpdate::Retain, presented, need_rerender)
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BrowserTeardownClass {
    BrowserWrappersReleased {
        backend_acknowledged: bool,
        renderer_deinitialized: bool,
    },
    BrowserTeardownFailed,
    Other,
}

const fn classify_browser_teardown(shutdown: LinuxPresentationShutdown) -> BrowserTeardownClass {
    match shutdown {
        LinuxPresentationShutdown::BrowserWrappersReleased(report) => {
            BrowserTeardownClass::BrowserWrappersReleased {
                backend_acknowledged: report.backend_acknowledged(),
                renderer_deinitialized: report.renderer_deinitialized(),
            }
        }
        LinuxPresentationShutdown::BrowserTeardownFailed(_) => {
            BrowserTeardownClass::BrowserTeardownFailed
        }
        _ => BrowserTeardownClass::Other,
    }
}

const fn browser_teardown_is_admitted(class: BrowserTeardownClass) -> bool {
    matches!(
        class,
        BrowserTeardownClass::BrowserWrappersReleased {
            backend_acknowledged: true,
            renderer_deinitialized: true,
        }
    )
}

/// Returns whether a smoke program reached its terminal hold and caused the
/// native shell to stop through the exact local exit-request path.
#[must_use]
pub const fn is_completed_smoke_exit(reason: LinuxStopReason, smoke_completed: bool) -> bool {
    smoke_completed && matches!(reason, LinuxStopReason::Requested)
}

/// Optional deterministic same-binary smoke program.
#[derive(Clone, Debug)]
pub struct BrowserSmokeConfig {
    pub second_url: Box<str>,
    pub hard_deadline: Duration,
}

/// Stable evidence returned after the native shell and engine have stopped.
#[derive(Clone, Copy, Debug)]
pub struct BrowserRunReport {
    pub native: LinuxShutdownReport,
    pub engine: EnginePortShutdownStatus,
    pub text: TextShutdownReport,
    pub successful_compositions: u64,
    pub last_receipt: Option<BrowserFrameReceipt>,
    pub smoke_completed: bool,
}

/// Fatal startup, event, shaping, or exact-contract failure.
#[derive(Debug)]
pub struct BrowserShellError(String);

impl BrowserShellError {
    fn new(detail: impl fmt::Display) -> Self {
        Self(detail.to_string())
    }
}

impl fmt::Display for BrowserShellError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for BrowserShellError {}

/// Runs the real Rust browser on the calling main thread.
///
/// The native event loop is created before the engine worker. A separate
/// payload-free helper only coalesces wake requests; all session, text,
/// compositor, and native-window authority remains on this thread.
///
/// # Errors
///
/// Returns [`BrowserShellError`] for startup, engine/session/text, native
/// event, compositor, teardown-evidence, or helper-thread failure.
pub fn run_browser(
    backend: Option<LinuxBackend>,
    initial_url: Option<Box<str>>,
    smoke: Option<BrowserSmokeConfig>,
) -> Result<BrowserRunReport, BrowserShellError> {
    let smoke_requested = smoke.is_some();
    let namespace = SurfaceNamespace::new(0x5742_0006)
        .ok_or_else(|| BrowserShellError::new("browser surface namespace is zero"))?;
    let mut config = LinuxShellConfig::wild_buzzard_default(namespace);
    config.presentation_mode = LinuxPresentationMode::BrowserCompositor;
    if let Some(backend) = backend {
        config.backend = match backend {
            LinuxBackend::Wayland => wild_buzzard_linux::LinuxBackendPreference::Wayland,
            LinuxBackend::X11 => wild_buzzard_linux::LinuxBackendPreference::X11,
        };
    }

    let shell = LinuxWindowShell::new(config).map_err(BrowserShellError::new)?;
    let wake = shell.wake_handle();
    let running = Arc::new(AtomicBool::new(true));
    let polling = Arc::new(AtomicBool::new(smoke.is_some()));
    let poll_thread = spawn_poll_thread(wake, Arc::clone(&running), Arc::clone(&polling))?;
    let mut handler = BrowserHandler::new(initial_url, smoke, polling);
    let native = shell.run(&mut handler).map_err(BrowserShellError::new);
    running.store(false, Ordering::Release);
    poll_thread
        .join()
        .map_err(|_| BrowserShellError::new("payload-free engine wake thread panicked"))?;
    let native = native?;
    if let Some(failure) = handler.failure.take() {
        return Err(BrowserShellError::new(failure));
    }
    if !native_stop_is_admitted(native.reason, smoke_requested, handler.smoke_completed) {
        return Err(BrowserShellError::new(format_args!(
            "browser native shell stopped through a non-normal path: {:?}",
            native.reason
        )));
    }
    let teardown_class = classify_browser_teardown(native.presentation);
    if !browser_teardown_is_admitted(teardown_class) {
        match teardown_class {
            BrowserTeardownClass::BrowserWrappersReleased { .. } => {
                return Err(BrowserShellError::new(
                    "browser compositor teardown lacked confirmed backend or renderer evidence",
                ));
            }
            BrowserTeardownClass::BrowserTeardownFailed => {
                return Err(BrowserShellError::new(
                    "browser compositor reported an ordered teardown failure",
                ));
            }
            BrowserTeardownClass::Other => {
                return Err(BrowserShellError::new(format_args!(
                    "browser compositor did not release every ordered owner normally: {:?}",
                    native.presentation
                )));
            }
        }
    }
    let LinuxPresentationShutdown::BrowserWrappersReleased(presentation) = native.presentation
    else {
        return Err(BrowserShellError::new(
            "admitted browser teardown classification disagreed with its exact variant",
        ));
    };
    if !presentation.backend_acknowledged() || !presentation.renderer_deinitialized() {
        return Err(BrowserShellError::new(
            "admitted browser teardown classification disagreed with its exact evidence",
        ));
    }
    if presentation.presentation().submitted_frames() != handler.successful_compositions {
        return Err(BrowserShellError::new(
            "browser/native successful-frame accounting disagreed at shutdown",
        ));
    }
    let engine = handler
        .engine_shutdown
        .ok_or_else(|| BrowserShellError::new("engine shutdown evidence was not recorded"))?;
    if !engine_shutdown_is_admitted(engine) {
        return Err(BrowserShellError::new(format_args!(
            "browser engine did not stop through requested clean executor shutdown: {engine:?}"
        )));
    }
    let text = handler
        .text_shutdown
        .ok_or_else(|| BrowserShellError::new("chrome text shutdown evidence was not recorded"))?;
    Ok(BrowserRunReport {
        native,
        engine,
        text,
        successful_compositions: handler.successful_compositions,
        last_receipt: handler.presented.receipt,
        smoke_completed: handler.smoke_completed,
    })
}

fn spawn_poll_thread(
    wake: LinuxWakeHandle,
    running: Arc<AtomicBool>,
    polling: Arc<AtomicBool>,
) -> Result<thread::JoinHandle<()>, BrowserShellError> {
    thread::Builder::new()
        .name("wild-buzzard-engine-wake".to_owned())
        .spawn(move || {
            while running.load(Ordering::Acquire) {
                if polling.load(Ordering::Acquire) && matches!(wake.wake(), LinuxWakeStatus::Closed)
                {
                    break;
                }
                thread::sleep(POLL_INTERVAL);
            }
        })
        .map_err(BrowserShellError::new)
}

#[derive(Clone, Copy)]
struct PresentedState {
    active_tab: Option<BrowserTabId>,
    page: BrowserPageSnapshot,
    receipt: Option<BrowserFrameReceipt>,
    last_page_revision: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RerenderPending {
    tab: BrowserTabId,
    navigation: NavigationId,
    document: EngineDocumentVersion,
    operation: DocumentOperationId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RerenderSuppression {
    navigation: NavigationId,
    document: EngineDocumentVersion,
    failure: Option<wild_buzzard_engine::DocumentOperationFailure>,
}

impl Default for PresentedState {
    fn default() -> Self {
        Self {
            active_tab: None,
            page: BrowserPageSnapshot::Blank,
            receipt: None,
            last_page_revision: None,
        }
    }
}

enum SmokeStage {
    Disabled,
    AwaitFirstPage { second_url: Box<str> },
    AwaitSecondPage,
    AwaitFirstPageAgain,
    AwaitResizeAway { initial: PhysicalSize },
    AwaitResizeBack { initial: PhysicalSize },
    AwaitFinalPage,
    AwaitFinalChromeAfterClose,
    Holding { until: Instant },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DrawOutcome {
    Deferred,
    Submitted,
    PreacceptRejected,
}

#[cfg(test)]
const fn smoke_transition_has_receipt(outcome: DrawOutcome) -> bool {
    matches!(outcome, DrawOutcome::Submitted)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SmokeResizeAction {
    None,
    RequestInnerSize(PhysicalSize),
    RequestRerender,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SurfaceAdmission {
    AwaitingReady,
    Presentable,
    ExplicitlySuspended,
    ZeroSized,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ViewportCompatibility {
    Compatible,
    Incompatible,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BrowserSurfaceState {
    admission: SurfaceAdmission,
    viewport: ViewportCompatibility,
    stale: bool,
}

impl Default for BrowserSurfaceState {
    fn default() -> Self {
        Self {
            admission: SurfaceAdmission::AwaitingReady,
            viewport: ViewportCompatibility::Compatible,
            stale: false,
        }
    }
}

impl BrowserSurfaceState {
    const fn mark_ready(&mut self) {
        self.admission = SurfaceAdmission::Presentable;
        self.viewport = ViewportCompatibility::Compatible;
    }

    const fn suspend(&mut self) {
        self.admission = SurfaceAdmission::ExplicitlySuspended;
        self.viewport = ViewportCompatibility::Incompatible;
        self.stale = true;
    }

    const fn record_transition(&mut self, resumed: bool, size: PhysicalSize) -> bool {
        self.admission =
            if matches!(self.admission, SurfaceAdmission::ExplicitlySuspended) && !resumed {
                SurfaceAdmission::ExplicitlySuspended
            } else if surface_is_drawable(size) {
                SurfaceAdmission::Presentable
            } else {
                SurfaceAdmission::ZeroSized
            };
        self.stale = true;
        if !matches!(self.admission, SurfaceAdmission::Presentable) {
            self.viewport = ViewportCompatibility::Incompatible;
        }
        matches!(self.admission, SurfaceAdmission::Presentable)
    }

    const fn can_draw(self, size: PhysicalSize) -> bool {
        matches!(self.admission, SurfaceAdmission::Presentable) && surface_is_drawable(size)
    }

    const fn is_presentable(self) -> bool {
        matches!(self.admission, SurfaceAdmission::Presentable)
    }

    const fn viewport_compatible(self) -> bool {
        matches!(self.viewport, ViewportCompatibility::Compatible)
    }

    const fn set_viewport_compatible(&mut self, compatible: bool) {
        self.viewport = if compatible {
            ViewportCompatibility::Compatible
        } else {
            ViewportCompatibility::Incompatible
        };
    }

    const fn mark_presented(&mut self) {
        self.stale = false;
    }
}

struct BrowserHandler {
    session: Option<BrowserSession<NavigationEnginePort>>,
    text: Option<TextSystem>,
    browser_window: BrowserWindowId,
    initial_url: Option<Box<str>>,
    initial_surface: Option<WebRenderSurfaceSnapshot>,
    engine_viewport: Option<PhysicalSize>,
    surface: BrowserSurfaceState,
    presented: PresentedState,
    graphics_navigations: BTreeMap<NavigationId, BrowserNavigationIdentity>,
    next_graphics_navigation: Option<u64>,
    next_chrome_revision: Option<u64>,
    next_epoch: Option<u32>,
    next_sequence: Option<u64>,
    rerender_pending: BTreeMap<BrowserTabId, RerenderPending>,
    rerender_suppressed: BTreeMap<BrowserTabId, RerenderSuppression>,
    polling: Arc<AtomicBool>,
    smoke_stage: SmokeStage,
    smoke_deadline: Option<Instant>,
    successful_compositions: u64,
    consecutive_preaccept_rejections: u8,
    smoke_completed: bool,
    engine_shutdown: Option<EnginePortShutdownStatus>,
    text_shutdown: Option<TextShutdownReport>,
    failure: Option<String>,
}

impl BrowserHandler {
    fn new(
        initial_url: Option<Box<str>>,
        smoke: Option<BrowserSmokeConfig>,
        polling: Arc<AtomicBool>,
    ) -> Self {
        let (smoke_stage, smoke_deadline) = match smoke {
            Some(smoke) => (
                SmokeStage::AwaitFirstPage {
                    second_url: smoke.second_url,
                },
                Instant::now().checked_add(smoke.hard_deadline),
            ),
            None => (SmokeStage::Disabled, None),
        };
        Self {
            session: None,
            text: None,
            browser_window: BrowserWindowId::new(1).expect("initial browser window is nonzero"),
            initial_url,
            initial_surface: None,
            engine_viewport: None,
            surface: BrowserSurfaceState::default(),
            presented: PresentedState::default(),
            graphics_navigations: BTreeMap::new(),
            next_graphics_navigation: Some(1),
            next_chrome_revision: Some(1),
            next_epoch: Some(1),
            next_sequence: Some(1),
            rerender_pending: BTreeMap::new(),
            rerender_suppressed: BTreeMap::new(),
            polling,
            smoke_stage,
            smoke_deadline,
            successful_compositions: 0,
            consecutive_preaccept_rejections: 0,
            smoke_completed: false,
            engine_shutdown: None,
            text_shutdown: None,
            failure: None,
        }
    }

    fn fail(&mut self, detail: impl fmt::Display, control: &mut LinuxWindowControl<'_>) {
        if self.failure.is_none() {
            self.failure = Some(detail.to_string());
        }
        self.polling.store(false, Ordering::Release);
        control.request_exit();
    }

    const fn invalidate_hit_authority(&mut self) {
        self.presented.receipt = None;
    }

    fn record_preaccept_rejection(&mut self) -> Result<(), BrowserShellError> {
        if self.consecutive_preaccept_rejections >= MAX_CONSECUTIVE_PREACCEPT_REJECTIONS {
            return Err(BrowserShellError::new(format_args!(
                "browser compositor repeated a preaccept rejection more than \
                 {MAX_CONSECUTIVE_PREACCEPT_REJECTIONS} times"
            )));
        }
        self.consecutive_preaccept_rejections += 1;
        Ok(())
    }

    const fn record_successful_composition(&mut self) {
        self.consecutive_preaccept_rejections = 0;
    }

    const fn unfinished_smoke_requires_polling(&self) -> bool {
        self.smoke_deadline.is_some() && !self.smoke_completed
    }

    const fn mark_explicitly_suspended(&mut self) {
        self.surface.suspend();
    }

    const fn record_surface_transition(&mut self, resumed: bool, size: PhysicalSize) -> bool {
        self.surface.record_transition(resumed, size)
    }

    const fn can_draw_surface(&self, size: PhysicalSize) -> bool {
        self.surface.can_draw(size)
    }

    fn start_session(
        &mut self,
        ready: LinuxWindowEvent,
        control: &LinuxWindowControl<'_>,
    ) -> Result<(), BrowserShellError> {
        let surface = control
            .browser_surface_snapshot()
            .map_err(BrowserShellError::new)?;
        let geometry =
            BrowserChromeGeometry::for_surface(surface).map_err(BrowserShellError::new)?;
        let content = geometry.content().size().ok_or_else(|| {
            BrowserShellError::new("initial surface has no nonzero browser content extent")
        })?;
        if content.width == 0 || content.height == 0 {
            return Err(BrowserShellError::new(
                "initial native surface leaves no nonzero page viewport below browser chrome",
            ));
        }
        let page_config = StaticPageConfig {
            viewport_width: content.width,
            viewport_height: content.height,
            ..StaticPageConfig::default()
        };
        let port =
            NavigationEnginePort::spawn_for_presentation(page_config, EngineLimits::default())
                .map_err(BrowserShellError::new)?;
        let mut session =
            BrowserSession::new(port, shell_session_limits()).map_err(BrowserShellError::new)?;
        session
            .handle_linux_event(self.browser_window, ready)
            .map_err(BrowserShellError::new)?;
        self.initial_surface = Some(surface);
        self.engine_viewport = Some(content);
        self.surface.mark_ready();
        self.text =
            Some(TextSystem::new_linux(TextLimits::default()).map_err(BrowserShellError::new)?);
        if let Some(url) = self.initial_url.take() {
            let tab = session
                .window_snapshot(self.browser_window)
                .map_err(BrowserShellError::new)?
                .active;
            session
                .navigate_new(tab, &url)
                .map_err(BrowserShellError::new)?;
            self.polling.store(true, Ordering::Release);
        }
        self.session = Some(session);
        self.sync_native_ime(control)?;
        control.request_redraw().map_err(BrowserShellError::new)?;
        Ok(())
    }

    fn session_mut(
        &mut self,
    ) -> Result<&mut BrowserSession<NavigationEnginePort>, BrowserShellError> {
        self.session
            .as_mut()
            .ok_or_else(|| BrowserShellError::new("browser session is not initialized"))
    }

    fn active_tab(&self) -> Result<BrowserTabId, BrowserShellError> {
        self.session
            .as_ref()
            .ok_or_else(|| BrowserShellError::new("browser session is not initialized"))?
            .window_snapshot(self.browser_window)
            .map(|window| window.active)
            .map_err(BrowserShellError::new)
    }

    fn current_window_contains_tab(&self, tab: BrowserTabId) -> Result<bool, BrowserShellError> {
        self.session
            .as_ref()
            .ok_or_else(|| BrowserShellError::new("browser session is not initialized"))?
            .window_snapshot(self.browser_window)
            .map(|window| window.tabs.contains(&tab))
            .map_err(BrowserShellError::new)
    }

    fn active_tab_allows_ime(&self) -> Result<bool, BrowserShellError> {
        let tab = self.active_tab()?;
        self.session
            .as_ref()
            .ok_or_else(|| BrowserShellError::new("browser session is not initialized"))?
            .tab_snapshot(tab)
            .map(|snapshot| snapshot.address_focused)
            .map_err(BrowserShellError::new)
    }

    fn sync_native_ime(&self, control: &LinuxWindowControl<'_>) -> Result<(), BrowserShellError> {
        control
            .set_ime_allowed(self.active_tab_allows_ime()?)
            .map_err(BrowserShellError::new)
    }

    fn allocate_graphics_navigation(
        &mut self,
        navigation: NavigationId,
    ) -> Result<BrowserNavigationIdentity, BrowserShellError> {
        let live = self.live_navigations()?;
        if !live.contains(&navigation) {
            return Err(BrowserShellError::new(
                "cannot allocate graphics identity for a non-live navigation",
            ));
        }
        // Presented pixels and receipts copy the opaque graphics identity by
        // value. Only a live engine navigation needs this lookup for a future
        // rerender, so terminal replacements must not accumulate here.
        self.graphics_navigations
            .retain(|candidate, _| live.contains(candidate));
        if let Some(identity) = self.graphics_navigations.get(&navigation).copied() {
            return Ok(identity);
        }
        if self.graphics_navigations.len() >= MAX_GRAPHICS_NAVIGATIONS {
            return Err(BrowserShellError::new(
                "graphics navigation identity registry reached its hard process limit",
            ));
        }
        let raw = self
            .next_graphics_navigation
            .ok_or_else(|| BrowserShellError::new("graphics navigation identity exhausted"))?;
        let identity = BrowserNavigationIdentity::new(raw)
            .ok_or_else(|| BrowserShellError::new("graphics navigation identity was zero"))?;
        self.next_graphics_navigation = raw.checked_add(1);
        self.graphics_navigations.insert(navigation, identity);
        Ok(identity)
    }

    fn live_navigations(&self) -> Result<BTreeSet<NavigationId>, BrowserShellError> {
        let session = self
            .session
            .as_ref()
            .ok_or_else(|| BrowserShellError::new("browser session is not initialized"))?;
        let window = session
            .window_snapshot(self.browser_window)
            .map_err(BrowserShellError::new)?;
        let mut live = BTreeSet::new();
        for tab in window.tabs.iter().copied() {
            if let Some(navigation) = session
                .tab_snapshot(tab)
                .map_err(BrowserShellError::new)?
                .live_navigation
            {
                live.insert(navigation);
            }
        }
        Ok(live)
    }

    fn reserve_frame_labels(
        &mut self,
    ) -> Result<(BrowserChromeRevision, u32, u64), BrowserShellError> {
        let chrome_raw = self
            .next_chrome_revision
            .ok_or_else(|| BrowserShellError::new("browser chrome revision exhausted"))?;
        let epoch = self
            .next_epoch
            .filter(|epoch| *epoch != u32::MAX)
            .ok_or_else(|| BrowserShellError::new("browser root epoch exhausted"))?;
        let sequence = self
            .next_sequence
            .ok_or_else(|| BrowserShellError::new("browser swap sequence exhausted"))?;
        let chrome = BrowserChromeRevision::new(chrome_raw)
            .ok_or_else(|| BrowserShellError::new("browser chrome revision was zero"))?;
        self.next_chrome_revision = chrome_raw.checked_add(1);
        self.next_epoch = epoch.checked_add(1);
        self.next_sequence = sequence.checked_add(1);
        Ok((chrome, epoch, sequence))
    }

    fn shape(
        &mut self,
        text: &str,
        size: f32,
    ) -> Result<Arc<wild_buzzard_text::ShapedText>, BrowserShellError> {
        self.text
            .as_mut()
            .ok_or_else(|| BrowserShellError::new("chrome text system is not initialized"))?
            .shape(&TextRequest::new(text, size))
            .map_err(BrowserShellError::new)
    }

    fn build_chrome(
        &mut self,
        surface: WebRenderSurfaceSnapshot,
        revision: BrowserChromeRevision,
    ) -> Result<BrowserChromeScene, BrowserShellError> {
        let window = self
            .session
            .as_ref()
            .ok_or_else(|| BrowserShellError::new("browser session is not initialized"))?
            .window_snapshot(self.browser_window)
            .map_err(BrowserShellError::new)?;
        let mut snapshots = Vec::new();
        snapshots
            .try_reserve_exact(window.tabs.len())
            .map_err(BrowserShellError::new)?;
        for tab in window.tabs.iter().copied() {
            let snapshot = self
                .session
                .as_ref()
                .expect("session checked above")
                .tab_snapshot(tab)
                .map_err(BrowserShellError::new)?;
            snapshots.push(snapshot);
        }
        let active = snapshots
            .iter()
            .find(|tab| tab.id == window.active)
            .ok_or_else(|| BrowserShellError::new("active tab is absent from its window"))?
            .clone();
        let mut tabs = Vec::new();
        tabs.try_reserve_exact(snapshots.len())
            .map_err(BrowserShellError::new)?;
        for snapshot in &snapshots {
            let title = if snapshot.address.is_empty() {
                "New Tab"
            } else {
                bounded_utf8_prefix(&snapshot.address, MAX_TAB_LABEL_BYTES)
            };
            let shaped = self.shape(title, TAB_FONT_SIZE_PX)?;
            let identity = BrowserTabIdentity::new(snapshot.id.get())
                .ok_or_else(|| BrowserShellError::new("browser tab identity was zero"))?;
            tabs.push(BrowserChromeTab::new(identity, shaped).with_loading(snapshot.loading));
        }
        let active_identity = BrowserTabIdentity::new(active.id.get())
            .ok_or_else(|| BrowserShellError::new("active browser tab identity was zero"))?;
        let address_text = if active.address.is_empty() {
            " "
        } else {
            bounded_utf8_prefix(&active.address, MAX_ADDRESS_LABEL_BYTES)
        };
        let address = self.shape(address_text, ADDRESS_FONT_SIZE_PX)?;
        let status_text = if !self.surface.viewport_compatible() {
            Some("Page cleared: viewport reflow is not implemented yet".to_owned())
        } else if active.loading {
            Some("Loading…".to_owned())
        } else if active.last_document_failure.is_some() {
            Some("Page update failed".to_owned())
        } else {
            None
        };
        let status = status_text
            .as_deref()
            .map(|text| bounded_utf8_prefix(text, MAX_STATUS_LABEL_BYTES))
            .map(|text| self.shape(text, STATUS_FONT_SIZE_PX))
            .transpose()?;
        let focus = if active.address_focused {
            BrowserChromeFocus::AddressBar
        } else {
            BrowserChromeFocus::Page
        };
        let state =
            BrowserChromeState::new(tabs.into_boxed_slice(), Some(active_identity), address)
                .with_address_selection(BrowserAddressSelection::new(
                    active.address_selection.anchor().min(address_text.len()),
                    active.address_selection.focus().min(address_text.len()),
                ))
                .with_status(status)
                .with_focus(focus);
        BrowserChromeScene::new(revision, surface, state).map_err(BrowserShellError::new)
    }

    fn request_rerender_if_possible(&mut self, tab: BrowserTabId) -> Result<(), BrowserShellError> {
        if !self.surface.viewport_compatible() {
            return Ok(());
        }
        let snapshot = self
            .session
            .as_ref()
            .ok_or_else(|| BrowserShellError::new("browser session is not initialized"))?
            .tab_snapshot(tab)
            .map_err(BrowserShellError::new)?;
        let (Some(navigation), Some(document)) =
            (snapshot.live_navigation, snapshot.engine_live_version)
        else {
            return Ok(());
        };
        if self
            .rerender_suppressed
            .get(&tab)
            .is_some_and(|suppressed| {
                suppressed.navigation == navigation && suppressed.document == document
            })
        {
            return Ok(());
        }
        self.rerender_suppressed.remove(&tab);
        if self.rerender_pending.get(&tab).is_some_and(|pending| {
            pending.tab == tab && pending.navigation == navigation && pending.document == document
        }) {
            return Ok(());
        }
        let operation = self
            .session_mut()?
            .request_presentation_rerender(tab)
            .map_err(BrowserShellError::new)?;
        self.rerender_pending.insert(
            tab,
            RerenderPending {
                tab,
                navigation,
                document,
                operation,
            },
        );
        self.polling.store(true, Ordering::Release);
        Ok(())
    }

    fn reconcile_rerender_pending(&mut self) -> Result<(), BrowserShellError> {
        let mut pending_entries = Vec::new();
        pending_entries
            .try_reserve_exact(self.rerender_pending.len())
            .map_err(BrowserShellError::new)?;
        pending_entries.extend(self.rerender_pending.values().copied());
        for pending in pending_entries {
            if !self.current_window_contains_tab(pending.tab)? {
                self.rerender_pending.remove(&pending.tab);
                self.rerender_suppressed.remove(&pending.tab);
                continue;
            }
            let snapshot = self
                .session
                .as_ref()
                .ok_or_else(|| BrowserShellError::new("browser session is not initialized"))?
                .tab_snapshot(pending.tab)
                .map_err(BrowserShellError::new)?;
            let labels_obsolete = snapshot.live_navigation != Some(pending.navigation)
                || snapshot.engine_live_version != Some(pending.document);
            let terminal = snapshot
                .last_presentation_rerender
                .filter(|terminal| terminal.operation() == pending.operation);
            if labels_obsolete {
                self.rerender_pending.remove(&pending.tab);
                self.rerender_suppressed.remove(&pending.tab);
            } else if let Some(terminal) = terminal {
                self.rerender_pending.remove(&pending.tab);
                if rerender_terminal_requires_suppression(
                    terminal.failure(),
                    snapshot.frame.is_some(),
                ) {
                    self.rerender_suppressed.insert(
                        pending.tab,
                        RerenderSuppression {
                            navigation: pending.navigation,
                            document: pending.document,
                            failure: terminal.failure(),
                        },
                    );
                } else {
                    self.rerender_suppressed.remove(&pending.tab);
                }
            }
        }
        Ok(())
    }

    fn retire_rerender_authority_after_command(&mut self, outcome: BrowserCommandOutcome) {
        match outcome {
            BrowserCommandOutcome::TabClosed { tab, .. } => {
                self.rerender_pending.remove(&tab);
                self.rerender_suppressed.remove(&tab);
            }
            BrowserCommandOutcome::WindowClosed { .. }
            | BrowserCommandOutcome::SessionClosed { .. } => {
                self.rerender_pending.clear();
                self.rerender_suppressed.clear();
            }
            _ => {}
        }
    }

    fn retire_rerender_authority_after_routed(&mut self, outcome: &LinuxEventOutcome) {
        if let LinuxEventOutcome::Command(command) = outcome {
            self.retire_rerender_authority_after_command(*command);
        }
    }

    fn prepare_page_update(
        &mut self,
        active: BrowserTabId,
    ) -> Result<(BrowserPageUpdate, BrowserPageSnapshot, bool, bool), BrowserShellError> {
        let snapshot = self
            .session
            .as_ref()
            .ok_or_else(|| BrowserShellError::new("browser session is not initialized"))?
            .tab_snapshot(active)
            .map_err(BrowserShellError::new)?;
        let candidate = self
            .session
            .as_ref()
            .expect("session checked above")
            .frame(active)
            .map_err(BrowserShellError::new)?
            .and_then(|frame| frame.descriptor().presentation_scene());
        if let Some(descriptor) = candidate {
            let stale_revision = self
                .presented
                .last_page_revision
                .is_some_and(|last| descriptor.scene_revision() <= last);
            if stale_revision || !self.surface.viewport_compatible() {
                drop(
                    self.session_mut()?
                        .take_frame(active)
                        .map_err(BrowserShellError::new)?,
                );
                if stale_revision {
                    self.request_rerender_if_possible(active)?;
                }
            } else {
                let navigation = snapshot.live_navigation.ok_or_else(|| {
                    BrowserShellError::new("presentation candidate has no exact live navigation")
                })?;
                let document = snapshot.engine_frame_version.ok_or_else(|| {
                    BrowserShellError::new("presentation candidate has no exact frame document")
                })?;
                let browser_navigation = self.allocate_graphics_navigation(navigation)?;
                let scene = self
                    .session_mut()?
                    .take_presentation_scene(
                        active,
                        navigation,
                        document,
                        descriptor.scene_revision(),
                        browser_navigation,
                    )
                    .map_err(BrowserShellError::new)?
                    .ok_or_else(|| {
                        BrowserShellError::new(
                            "validated presentation candidate disappeared before consumption",
                        )
                    })?;
                let page = BrowserPageSnapshot::Scene(scene.identity());
                return Ok((BrowserPageUpdate::Install(scene), page, true, false));
            }
        }

        let scene_invalidated = self.presented.active_tab != Some(active)
            || self.surface.stale
            || !self.surface.viewport_compatible();
        let awaiting_live_frame = snapshot.live_navigation.is_some()
            && snapshot.engine_live_version.is_some()
            && snapshot.frame.is_none();
        let fallback = select_page_fallback(
            matches!(self.presented.page, BrowserPageSnapshot::Scene(_)),
            scene_invalidated,
            awaiting_live_frame,
        );
        let (update, page, need_rerender) =
            materialize_page_fallback(fallback, self.presented.page);
        Ok((update, page, false, need_rerender))
    }

    fn draw(
        &mut self,
        control: &mut LinuxWindowControl<'_>,
    ) -> Result<DrawOutcome, BrowserShellError> {
        // Suspension can leave a nonzero descriptor cached by the native
        // shell. Do not even query/submit through the compositor until the
        // explicit resume transition reopens surface admission.
        if !self.surface.is_presentable() {
            return Ok(DrawOutcome::Deferred);
        }
        let surface = control
            .browser_surface_snapshot()
            .map_err(BrowserShellError::new)?;
        if !self.can_draw_surface(surface.size()) {
            self.surface.record_transition(false, surface.size());
            return Ok(DrawOutcome::Deferred);
        }
        let active = self.active_tab()?;
        let (chrome_revision, epoch, sequence) = self.reserve_frame_labels()?;
        let chrome = self.build_chrome(surface, chrome_revision)?;
        let (page_update, page, installing, need_rerender) = self.prepare_page_update(active)?;
        let request = BrowserFrameRequest::new(surface, page, chrome_revision, epoch, sequence);
        match control.submit_browser_frame(page_update, Some(chrome), request) {
            Ok(receipt) => {
                if receipt.request() != request
                    || !receipt.renderer_frame_submitted()
                    || !receipt.egl_swap_submitted()
                    || receipt.desktop_compositor_acknowledged()
                {
                    return Err(BrowserShellError::new(
                        "browser compositor returned a forged or overclaimed receipt",
                    ));
                }
                self.successful_compositions = self
                    .successful_compositions
                    .checked_add(1)
                    .ok_or_else(|| BrowserShellError::new("composition count exhausted"))?;
                self.record_successful_composition();
                self.presented.active_tab = Some(active);
                self.presented.page = page;
                self.presented.receipt = Some(receipt);
                if let BrowserPageSnapshot::Scene(identity) = page {
                    self.presented.last_page_revision = Some(identity.revision().get());
                    self.rerender_pending.remove(&active);
                    self.rerender_suppressed.remove(&active);
                }
                self.surface.mark_presented();
                if need_rerender {
                    self.request_rerender_if_possible(active)?;
                }
                self.advance_smoke_after_submission(active, installing, receipt, control)?;
                Ok(DrawOutcome::Submitted)
            }
            Err(error) if retry_browser_frame_after(error) => {
                self.record_preaccept_rejection()?;
                if installing {
                    self.rerender_pending.remove(&active);
                    self.request_rerender_if_possible(active)?;
                } else {
                    control.request_redraw().map_err(BrowserShellError::new)?;
                }
                eprintln!("retry-safe preaccept browser composition rejection: {error}");
                Ok(DrawOutcome::PreacceptRejected)
            }
            Err(error) => Err(BrowserShellError::new(format_args!(
                "terminal or unclassified browser composition failure: {error}"
            ))),
        }
    }

    fn pump_engine(&mut self, control: &LinuxWindowControl<'_>) -> Result<(), BrowserShellError> {
        let maximum = self
            .session
            .as_ref()
            .ok_or_else(|| BrowserShellError::new("browser session is not initialized"))?
            .limits()
            .max_engine_events_per_pump();
        let outcome = self
            .session_mut()?
            .pump_engine(maximum)
            .map_err(BrowserShellError::new)?;
        let (processed, more) = match outcome {
            EnginePumpOutcome::Batch {
                processed,
                more_may_remain,
            } => (processed, more_may_remain),
            EnginePumpOutcome::Empty => (0, false),
            _ => {
                return Err(BrowserShellError::new(
                    "bounded engine pump returned a non-batch outcome",
                ));
            }
        };
        self.reconcile_rerender_pending()?;
        let any_loading = self
            .session
            .as_ref()
            .expect("session checked above")
            .window_snapshot(self.browser_window)
            .map_err(BrowserShellError::new)?
            .tabs
            .iter()
            .try_fold(false, |loading, tab| {
                self.session
                    .as_ref()
                    .expect("session checked above")
                    .tab_snapshot(*tab)
                    .map(|snapshot| loading || snapshot.loading)
            })
            .map_err(BrowserShellError::new)?;
        let closing_contexts = self
            .session
            .as_ref()
            .expect("session checked above")
            .closing_context_count();
        self.polling.store(
            more || any_loading
                || closing_contexts != 0
                || !self.rerender_pending.is_empty()
                || self.unfinished_smoke_requires_polling(),
            Ordering::Release,
        );
        if processed > 0 {
            self.invalidate_hit_authority();
            control.request_redraw().map_err(BrowserShellError::new)?;
        }
        Ok(())
    }

    fn route_event(
        &mut self,
        event: LinuxWindowEvent,
    ) -> Result<LinuxEventOutcome, BrowserShellError> {
        let window = self.browser_window;
        self.session_mut()?
            .handle_linux_event(window, event)
            .map_err(BrowserShellError::new)
    }

    fn account_input_once(
        &mut self,
        event: LinuxWindowEvent,
    ) -> Result<LinuxEventOutcome, BrowserShellError> {
        if !matches!(event, LinuxWindowEvent::Input { .. }) {
            return Err(BrowserShellError::new(
                "input accounting received a non-input Linux event",
            ));
        }
        self.route_event(event)
    }

    #[allow(clippy::too_many_lines)]
    fn handle_input(
        &mut self,
        event: LinuxWindowEvent,
        input: &InputEvent,
        control: &mut LinuxWindowControl<'_>,
    ) -> Result<(), BrowserShellError> {
        // Account every native input sequence exactly once before an optional
        // chrome hit action. Early tab/close/address returns therefore cannot
        // bypass ordering, and the fallthrough path must not route it again.
        let outcome = self.account_input_once(event)?;
        self.retire_rerender_authority_after_routed(&outcome);
        if routed_outcome_mutates_chrome(&outcome) {
            self.invalidate_hit_authority();
        }
        if routed_outcome_requires_engine_poll(&outcome) {
            self.polling.store(true, Ordering::Release);
        }
        if routed_outcome_requests_native_exit(&outcome) {
            control.request_exit();
            return Ok(());
        }
        // Keyboard shortcuts can change address/content focus or the active
        // tab before pointer hit handling. Always derive native IME admission
        // from the resulting canonical active-tab snapshot.
        self.sync_native_ime(control)?;
        let mut page_hit_applied = false;
        if let InputEvent::Pointer(pointer) = input
            && pointer_has_chrome_action_authority(pointer)
        {
            let Some(authoritative_receipt) = self.presented.receipt else {
                control.request_redraw().map_err(BrowserShellError::new)?;
                return Ok(());
            };
            let surface = control
                .browser_surface_snapshot()
                .map_err(BrowserShellError::new)?;
            let scale = surface.descriptor().scale.get();
            let x = (pointer.position.x * scale).round();
            let y = (pointer.position.y * scale).round();
            if x >= f64::from(i32::MIN)
                && x <= f64::from(i32::MAX)
                && y >= f64::from(i32::MIN)
                && y <= f64::from(i32::MAX)
            {
                #[allow(clippy::cast_possible_truncation)]
                let point = PhysicalPoint {
                    x: x as i32,
                    y: y as i32,
                };
                if let Some(hit) = control
                    .hit_test_browser(point, surface)
                    .map_err(BrowserShellError::new)?
                {
                    if authoritative_receipt != hit.receipt() {
                        self.invalidate_hit_authority();
                        control.request_redraw().map_err(BrowserShellError::new)?;
                        return Ok(());
                    }
                    match hit.target() {
                        BrowserHitTarget::Tab(identity) => {
                            let tab = BrowserTabId::new(identity.get()).ok_or_else(|| {
                                BrowserShellError::new("hit test returned a zero tab")
                            })?;
                            if !self.current_window_contains_tab(tab)? {
                                self.invalidate_hit_authority();
                                control.request_redraw().map_err(BrowserShellError::new)?;
                                return Ok(());
                            }
                            self.session_mut()?
                                .activate_tab(tab)
                                .map_err(BrowserShellError::new)?;
                            self.invalidate_hit_authority();
                            self.sync_native_ime(control)?;
                            control.request_redraw().map_err(BrowserShellError::new)?;
                            return Ok(());
                        }
                        BrowserHitTarget::TabClose(identity) => {
                            let tab = BrowserTabId::new(identity.get()).ok_or_else(|| {
                                BrowserShellError::new("hit test returned a zero tab")
                            })?;
                            if !self.current_window_contains_tab(tab)? {
                                self.invalidate_hit_authority();
                                control.request_redraw().map_err(BrowserShellError::new)?;
                                return Ok(());
                            }
                            let outcome = self
                                .session_mut()?
                                .close_tab(tab)
                                .map_err(BrowserShellError::new)?;
                            self.retire_rerender_authority_after_command(outcome);
                            self.invalidate_hit_authority();
                            if command_requires_engine_poll(outcome) {
                                self.polling.store(true, Ordering::Release);
                            }
                            if command_requests_native_exit(outcome) {
                                control.request_exit();
                            } else {
                                self.sync_native_ime(control)?;
                                control.request_redraw().map_err(BrowserShellError::new)?;
                            }
                            return Ok(());
                        }
                        BrowserHitTarget::AddressBar => {
                            let window = self.browser_window;
                            self.session_mut()?
                                .focus_address(window)
                                .map_err(BrowserShellError::new)?;
                            self.invalidate_hit_authority();
                            self.sync_native_ime(control)?;
                            control.request_redraw().map_err(BrowserShellError::new)?;
                            return Ok(());
                        }
                        BrowserHitTarget::Page { page, .. } => {
                            if self.presented.page != BrowserPageSnapshot::Scene(page) {
                                self.invalidate_hit_authority();
                                control.request_redraw().map_err(BrowserShellError::new)?;
                                return Ok(());
                            }
                            let tab = self.active_tab()?;
                            self.session_mut()?
                                .focus_content(tab)
                                .map_err(BrowserShellError::new)?;
                            self.invalidate_hit_authority();
                            self.sync_native_ime(control)?;
                            page_hit_applied = true;
                        }
                        BrowserHitTarget::Status => return Ok(()),
                    }
                }
            }
        }
        if input_requires_redraw(&outcome, page_hit_applied) {
            control.request_redraw().map_err(BrowserShellError::new)?;
        }
        Ok(())
    }

    fn handle_surface_transition(
        &mut self,
        event: LinuxWindowEvent,
        control: &mut LinuxWindowControl<'_>,
    ) -> Result<(), BrowserShellError> {
        let resumed = matches!(event, LinuxWindowEvent::Resumed);
        self.route_event(event)?;
        self.rerender_suppressed.clear();
        let surface = control
            .browser_surface_snapshot()
            .map_err(BrowserShellError::new)?;
        self.invalidate_hit_authority();
        if !self.record_surface_transition(resumed, surface.size()) {
            return Ok(());
        }
        let content = BrowserChromeGeometry::for_surface(surface)
            .map_err(BrowserShellError::new)?
            .content()
            .size();
        // Tiny but nonzero native windows can be all chrome. That is a valid
        // blank-page state, not a terminal resize failure.
        self.surface
            .set_viewport_compatible(viewport_matches_engine(self.engine_viewport, content));
        self.draw(control)?;
        Ok(())
    }

    fn handle_explicit_suspend(
        &mut self,
        event: LinuxWindowEvent,
    ) -> Result<(), BrowserShellError> {
        self.route_event(event)?;
        self.invalidate_hit_authority();
        self.mark_explicitly_suspended();
        Ok(())
    }

    fn handle_chrome_state_event(
        &mut self,
        event: LinuxWindowEvent,
        control: &mut LinuxWindowControl<'_>,
    ) -> Result<(), BrowserShellError> {
        let outcome = self.route_event(event)?;
        self.retire_rerender_authority_after_routed(&outcome);
        if routed_outcome_mutates_chrome(&outcome) {
            self.invalidate_hit_authority();
        }
        if routed_outcome_requests_native_exit(&outcome) {
            control.request_exit();
            return Ok(());
        }
        self.sync_native_ime(control)?;
        control.request_redraw().map_err(BrowserShellError::new)
    }

    #[allow(clippy::too_many_lines)]
    fn advance_smoke_after_composition(
        &mut self,
        active: BrowserTabId,
        installing: bool,
        control: &LinuxWindowControl<'_>,
    ) -> Result<(), BrowserShellError> {
        if matches!(self.smoke_stage, SmokeStage::AwaitFinalChromeAfterClose) {
            let window = self
                .session
                .as_ref()
                .ok_or_else(|| BrowserShellError::new("browser session is not initialized"))?
                .window_snapshot(self.browser_window)
                .map_err(BrowserShellError::new)?;
            let first = BrowserTabId::new(1).expect("first smoke tab is nonzero");
            if window.tabs.as_ref() != [first] || active != first {
                return Err(BrowserShellError::new(
                    "final smoke composition did not contain exact one-tab chrome",
                ));
            }
            if self
                .session
                .as_ref()
                .expect("session checked above")
                .closing_context_count()
                != 0
            {
                return Ok(());
            }
            eprintln!(
                "WILDBUZZARD_SMOKE_SUCCESS compositions={}",
                self.successful_compositions
            );
            self.polling.store(true, Ordering::Release);
            self.smoke_stage = SmokeStage::Holding {
                until: Instant::now() + SMOKE_HOLD,
            };
            return Ok(());
        }
        if !installing {
            return Ok(());
        }
        let stage = std::mem::replace(&mut self.smoke_stage, SmokeStage::Disabled);
        self.smoke_stage = match stage {
            SmokeStage::AwaitFirstPage { second_url } => {
                self.invalidate_hit_authority();
                let window = self.browser_window;
                let second = match self
                    .session_mut()?
                    .open_tab(window)
                    .map_err(BrowserShellError::new)?
                {
                    BrowserCommandOutcome::TabOpened { tab, .. } => tab,
                    other => {
                        return Err(BrowserShellError::new(format_args!(
                            "smoke did not open its second tab: {other:?}"
                        )));
                    }
                };
                self.session_mut()?
                    .activate_tab(second)
                    .map_err(BrowserShellError::new)?;
                self.session_mut()?
                    .navigate_new(second, &second_url)
                    .map_err(BrowserShellError::new)?;
                self.sync_native_ime(control)?;
                self.polling.store(true, Ordering::Release);
                control.request_redraw().map_err(BrowserShellError::new)?;
                SmokeStage::AwaitSecondPage
            }
            SmokeStage::AwaitSecondPage => {
                self.invalidate_hit_authority();
                let first = BrowserTabId::new(1).expect("first smoke tab is nonzero");
                self.session_mut()?
                    .activate_tab(first)
                    .map_err(BrowserShellError::new)?;
                self.sync_native_ime(control)?;
                control.request_redraw().map_err(BrowserShellError::new)?;
                SmokeStage::AwaitFirstPageAgain
            }
            SmokeStage::AwaitFirstPageAgain => {
                if active != BrowserTabId::new(1).expect("first smoke tab is nonzero") {
                    return Err(BrowserShellError::new(
                        "smoke reinstalled a page for the wrong active tab",
                    ));
                }
                let initial = self
                    .initial_surface
                    .ok_or_else(|| BrowserShellError::new("smoke lost initial surface"))?
                    .size();
                let smaller = PhysicalSize::new(
                    initial.width.saturating_sub(160).max(640),
                    initial.height.saturating_sub(120).max(480),
                )
                .map_err(BrowserShellError::new)?;
                control
                    .request_inner_size(smaller)
                    .map_err(BrowserShellError::new)?;
                SmokeStage::AwaitResizeAway { initial }
            }
            SmokeStage::AwaitFinalPage => {
                self.invalidate_hit_authority();
                let second = BrowserTabId::new(2).expect("second smoke tab is nonzero");
                let outcome = self
                    .session_mut()?
                    .close_tab(second)
                    .map_err(BrowserShellError::new)?;
                self.retire_rerender_authority_after_command(outcome);
                if command_requests_native_exit(outcome) {
                    return Err(BrowserShellError::new(
                        "smoke closing its second tab unexpectedly closed the session",
                    ));
                }
                if command_requires_engine_poll(outcome) {
                    self.polling.store(true, Ordering::Release);
                }
                self.sync_native_ime(control)?;
                control.request_redraw().map_err(BrowserShellError::new)?;
                self.polling.store(true, Ordering::Release);
                SmokeStage::AwaitFinalChromeAfterClose
            }
            other => other,
        };
        Ok(())
    }

    fn advance_smoke_after_submission(
        &mut self,
        active: BrowserTabId,
        installing: bool,
        receipt: BrowserFrameReceipt,
        control: &LinuxWindowControl<'_>,
    ) -> Result<(), BrowserShellError> {
        self.advance_smoke_after_composition(active, installing, control)?;
        self.advance_smoke_after_resize(receipt.request().surface().size(), control)
    }

    fn advance_smoke_after_resize(
        &mut self,
        size: PhysicalSize,
        control: &LinuxWindowControl<'_>,
    ) -> Result<(), BrowserShellError> {
        match self.commit_smoke_resize_submission(size) {
            SmokeResizeAction::None => {}
            SmokeResizeAction::RequestInnerSize(initial) => {
                control
                    .request_inner_size(initial)
                    .map_err(BrowserShellError::new)?;
            }
            SmokeResizeAction::RequestRerender => {
                let tab = self.active_tab()?;
                self.request_rerender_if_possible(tab)?;
            }
        }
        Ok(())
    }

    fn commit_smoke_resize_submission(&mut self, size: PhysicalSize) -> SmokeResizeAction {
        let stage = std::mem::replace(&mut self.smoke_stage, SmokeStage::Disabled);
        let (stage, action) = match stage {
            SmokeStage::AwaitResizeAway { initial } if size != initial => (
                SmokeStage::AwaitResizeBack { initial },
                SmokeResizeAction::RequestInnerSize(initial),
            ),
            SmokeStage::AwaitResizeBack { initial } if size == initial => (
                SmokeStage::AwaitFinalPage,
                SmokeResizeAction::RequestRerender,
            ),
            other => (other, SmokeResizeAction::None),
        };
        self.smoke_stage = stage;
        action
    }

    fn check_smoke_deadline(&mut self, control: &mut LinuxWindowControl<'_>) {
        if self
            .smoke_deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.fail(
                "same-binary smoke exceeded its internal hard deadline",
                control,
            );
            return;
        }
        if let SmokeStage::Holding { until } = self.smoke_stage
            && Instant::now() >= until
        {
            self.smoke_completed = true;
            self.polling.store(false, Ordering::Release);
            control.request_exit();
        }
    }

    fn finish(&mut self) {
        self.polling.store(false, Ordering::Release);
        if self.engine_shutdown.is_none()
            && let Some(session) = self.session.as_mut()
        {
            self.engine_shutdown = Some(session.shutdown());
        }
        if self.text_shutdown.is_none()
            && let Some(text) = self.text.take()
        {
            self.text_shutdown = Some(text.shutdown());
        }
    }
}

impl LinuxWindowHandler for BrowserHandler {
    fn handle_event(&mut self, event: LinuxWindowEvent, control: &mut LinuxWindowControl<'_>) {
        let result = match &event {
            LinuxWindowEvent::Ready { .. } => self.start_session(event.clone(), control),
            LinuxWindowEvent::Resumed
            | LinuxWindowEvent::Resized { .. }
            | LinuxWindowEvent::ScaleFactorChanged { .. }
                if self.session.is_some() =>
            {
                self.handle_surface_transition(event.clone(), control)
            }
            LinuxWindowEvent::Suspended if self.session.is_some() => {
                self.handle_explicit_suspend(event.clone())
            }
            LinuxWindowEvent::RedrawRequested { .. } if self.session.is_some() => self
                .route_event(event.clone())
                .and_then(|_| self.draw(control).map(|_| ())),
            LinuxWindowEvent::WakeRequested if self.session.is_some() => {
                let result = self.pump_engine(control);
                self.check_smoke_deadline(control);
                result
            }
            LinuxWindowEvent::Input { event: input, .. } if self.session.is_some() => {
                self.handle_input(event.clone(), input, control)
            }
            LinuxWindowEvent::Destroyed { .. } if self.session.is_some() => {
                let result = if matches!(
                    self.session.as_ref().map(BrowserSession::lifecycle),
                    Some(SessionLifecycle::Running)
                ) {
                    self.route_event(event.clone()).map(|_| ())
                } else {
                    Ok(())
                };
                self.finish();
                result
            }
            LinuxWindowEvent::Stopped(_) => {
                self.finish();
                Ok(())
            }
            LinuxWindowEvent::CloseRequested { .. }
            | LinuxWindowEvent::FocusChanged { .. }
            | LinuxWindowEvent::ImeEnabled { .. }
            | LinuxWindowEvent::ImePreedit { .. }
            | LinuxWindowEvent::ImeDisabled { .. }
                if self.session.is_some() =>
            {
                self.handle_chrome_state_event(event.clone(), control)
            }
            _ => Ok(()),
        };
        if let Err(error) = result {
            self.fail(error, control);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    use wild_buzzard_engine::{
        CancellationToken, DocumentLoadProof, DocumentOperationFailure, EngineFrame,
        ExecutionFailure, ExecutorDocumentRerender, ExecutorOutput, NavigationExecutor,
        NavigationRequest, PixelSize,
    };
    use wild_buzzard_linux::{
        InputOrigin, PresentationFailureStage, WebRenderWindowErrorKind,
        WebRenderWindowFailureStage,
    };
    use wild_buzzard_platform::{
        EventSequence, EventTimestampMicros, InputDeviceId, InputMetadata, KeyEvent, KeyLocation,
        KeyState, LogicalPoint, Modifiers, PhysicalKeyCode, PixelFormat, PointerEvent, PointerId,
        PointerKind, ScaleFactor, SeatId, SurfaceDescriptor, SurfaceIdAllocator, SurfaceRole,
        TextInputEvent,
    };

    #[derive(Clone, Copy)]
    enum RerenderBehavior {
        Rendered,
        Rejected(DocumentOperationFailure),
    }

    struct OnePixelExecutor {
        document: Option<wild_buzzard_dom::Document>,
        rerender: RerenderBehavior,
    }

    impl NavigationExecutor for OnePixelExecutor {
        fn execute(
            &mut self,
            navigation: NavigationId,
            _request: &NavigationRequest,
            _cancellation: &CancellationToken,
        ) -> Result<ExecutorOutput, ExecutionFailure> {
            let marker = u8::try_from(navigation.generation().get()).unwrap_or(u8::MAX);
            let document = wild_buzzard_dom::Document::new();
            let proof = DocumentLoadProof::from_snapshot(
                &document
                    .snapshot()
                    .expect("empty deterministic document snapshots"),
            );
            let frame = EngineFrame::from_rgba8_for_document(
                PixelSize::new(1, 1).expect("one-pixel extent is valid"),
                vec![marker, 17, 34, 255],
                document.version(),
            )
            .expect("one-pixel frame is valid");
            self.document = Some(document);
            ExecutorOutput::new_document(200, frame, proof)
        }

        fn rerender_document(
            &mut self,
            _navigation: NavigationId,
            expected_live_version: wild_buzzard_dom::DocumentVersion,
            _cancellation: &CancellationToken,
        ) -> ExecutorDocumentRerender {
            let Some(document) = self.document.as_ref() else {
                return ExecutorDocumentRerender::Rejected {
                    live_version: None,
                    frame_version: None,
                    failure: DocumentOperationFailure::NoLiveDocument,
                };
            };
            let version = document.version();
            if version != expected_live_version {
                return ExecutorDocumentRerender::Rejected {
                    live_version: Some(version),
                    frame_version: Some(version),
                    failure: DocumentOperationFailure::VersionMismatch,
                };
            }
            match self.rerender {
                RerenderBehavior::Rendered => ExecutorDocumentRerender::Rendered {
                    live_version: version,
                    previous_frame_version: version,
                    frame: EngineFrame::from_rgba8_for_document(
                        PixelSize::new(1, 1).unwrap(),
                        vec![51, 68, 85, 255],
                        version,
                    )
                    .unwrap(),
                },
                RerenderBehavior::Rejected(failure) => ExecutorDocumentRerender::Rejected {
                    live_version: Some(version),
                    frame_version: Some(version),
                    failure,
                },
            }
        }

        fn shutdown(&mut self) -> Result<(), ExecutionFailure> {
            Ok(())
        }
    }

    fn test_session() -> BrowserSession<NavigationEnginePort> {
        test_session_with_rerender(RerenderBehavior::Rendered)
    }

    fn test_session_with_rerender(
        rerender: RerenderBehavior,
    ) -> BrowserSession<NavigationEnginePort> {
        let engine_limits = EngineLimits::new(64, 128, MAX_BROWSER_CHROME_TABS, 4, 256)
            .expect("test engine limits are valid");
        let port = NavigationEnginePort::spawn_with_executor(engine_limits, move || {
            Ok(OnePixelExecutor {
                document: None,
                rerender,
            })
        })
        .expect("test navigation worker starts");
        BrowserSession::new(port, shell_session_limits()).expect("test session starts")
    }

    fn wait_for_navigation(
        session: &mut BrowserSession<NavigationEnginePort>,
        tab: BrowserTabId,
        navigation: NavigationId,
    ) {
        for _ in 0..100_000 {
            if session
                .tab_snapshot(tab)
                .expect("test tab remains live")
                .live_navigation
                == Some(navigation)
            {
                return;
            }
            match session.poll_engine_once() {
                Ok(EnginePumpOutcome::Empty) => thread::yield_now(),
                Ok(_) => {}
                Err(error) => panic!("test navigation failed: {error}"),
            }
        }
        panic!("test navigation did not publish within its bounded poll budget");
    }

    fn load_and_consume_frame(handler: &mut BrowserHandler, tab: BrowserTabId) -> NavigationId {
        let outcome = handler
            .session_mut()
            .unwrap()
            .navigate_new(tab, "https://rerender.invalid/")
            .unwrap();
        let BrowserCommandOutcome::NavigationQueued { navigation, .. } = outcome else {
            panic!("test navigation was not queued");
        };
        wait_for_navigation(handler.session_mut().unwrap(), tab, navigation);
        drop(handler.session_mut().unwrap().take_frame(tab).unwrap());
        navigation
    }

    fn drain_exact_rerender(handler: &mut BrowserHandler) {
        for _ in 0..100_000 {
            match handler.session_mut().unwrap().poll_engine_once() {
                Ok(EnginePumpOutcome::Empty) => thread::yield_now(),
                Ok(_) => {}
                Err(error) => panic!("rerender pump failed: {error}"),
            }
            handler.reconcile_rerender_pending().unwrap();
            if handler.rerender_pending.is_empty() {
                return;
            }
        }
        panic!("rerender did not become terminal within its bounded poll budget");
    }

    fn ready_event() -> (wild_buzzard_platform::SurfaceId, LinuxWindowEvent) {
        let mut allocator = SurfaceIdAllocator::new(
            SurfaceNamespace::new(0x5742_0060).expect("test namespace is nonzero"),
        );
        let surface = allocator.allocate().expect("test surface allocates");
        let descriptor = SurfaceDescriptor {
            id: surface,
            size: PhysicalSize::new(1_024, 768).expect("test surface extent is valid"),
            scale: ScaleFactor::new(1.0).expect("unit test scale is valid"),
            format: PixelFormat::Rgba8Srgb,
            role: SurfaceRole::Window,
        };
        (
            surface,
            LinuxWindowEvent::Ready {
                backend: LinuxBackend::Wayland,
                desired_surface: descriptor,
            },
        )
    }

    fn test_metadata(
        surface: wild_buzzard_platform::SurfaceId,
        sequence: u64,
        modifiers: Modifiers,
    ) -> InputMetadata {
        InputMetadata {
            sequence: EventSequence::new(sequence).unwrap(),
            timestamp: EventTimestampMicros(sequence),
            seat: SeatId::new(1).unwrap(),
            device: InputDeviceId::new(1).unwrap(),
            surface,
            modifiers,
        }
    }

    fn key_linux_event(
        surface: wild_buzzard_platform::SurfaceId,
        sequence: u64,
        physical_key: u32,
        modifiers: Modifiers,
    ) -> LinuxWindowEvent {
        LinuxWindowEvent::Input {
            event: InputEvent::Key(KeyEvent {
                metadata: test_metadata(surface, sequence, modifiers),
                physical_key: PhysicalKeyCode(physical_key),
                state: KeyState::Down,
                location: KeyLocation::Standard,
                repeat: false,
            }),
            origin: InputOrigin::Synthetic,
        }
    }

    fn pointer_event(
        surface: wild_buzzard_platform::SurfaceId,
        sequence: u64,
        buttons: u16,
    ) -> PointerEvent {
        PointerEvent {
            metadata: test_metadata(surface, sequence, Modifiers::default()),
            pointer: PointerId::new(1).unwrap(),
            kind: PointerKind::Mouse,
            phase: PointerPhase::Down,
            position: LogicalPoint::new(10.0, 10.0).unwrap(),
            buttons,
            pressure: None,
        }
    }

    #[test]
    fn shell_limits_match_the_browser_chrome_contract() {
        let limits = shell_session_limits();
        assert_eq!(limits.max_windows(), 1);
        assert_eq!(limits.max_tabs_per_window(), MAX_BROWSER_CHROME_TABS);
        assert_eq!(limits.max_total_tabs(), MAX_BROWSER_CHROME_TABS);
        let mut session = test_session();
        for _ in 1..MAX_BROWSER_CHROME_TABS {
            assert!(matches!(
                session.open_tab(BrowserWindowId::new(1).unwrap()),
                Ok(BrowserCommandOutcome::TabOpened { .. })
            ));
        }
        assert!(session.open_tab(BrowserWindowId::new(1).unwrap()).is_err());
        let _ = session.shutdown();
    }

    #[test]
    fn visual_prefixes_are_bounded_without_splitting_utf8() {
        let maximum_address = "a".repeat(MAX_NAVIGATION_URL_BYTES);
        assert_eq!(
            bounded_utf8_prefix(&maximum_address, MAX_ADDRESS_LABEL_BYTES).len(),
            MAX_ADDRESS_LABEL_BYTES
        );
        let unicode = format!("{}€", "a".repeat(MAX_TAB_LABEL_BYTES - 1));
        let prefix = bounded_utf8_prefix(&unicode, MAX_TAB_LABEL_BYTES);
        assert_eq!(prefix, "a".repeat(MAX_TAB_LABEL_BYTES - 1));
        assert!(unicode.is_char_boundary(prefix.len()));
    }

    #[test]
    fn replacement_navigations_do_not_accumulate_graphics_mappings() {
        let polling = Arc::new(AtomicBool::new(false));
        let mut handler = BrowserHandler::new(None, None, polling);
        handler.session = Some(test_session());
        let tab = BrowserTabId::new(1).expect("initial tab is nonzero");

        for index in 0..256 {
            let outcome = if index == 0 {
                handler
                    .session_mut()
                    .unwrap()
                    .navigate_new(tab, "https://replacement.invalid/")
            } else {
                handler.session_mut().unwrap().reload(tab)
            }
            .expect("replacement navigation is admitted");
            let BrowserCommandOutcome::NavigationQueued { navigation, .. } = outcome else {
                panic!("test expected a queued replacement navigation");
            };
            wait_for_navigation(handler.session_mut().unwrap(), tab, navigation);
            let identity = handler
                .allocate_graphics_navigation(navigation)
                .expect("live navigation receives a graphics identity");
            assert_eq!(
                handler.graphics_navigations.len(),
                handler.live_navigations().unwrap().len()
            );
            assert_eq!(handler.graphics_navigations.len(), 1);
            assert_eq!(handler.graphics_navigations[&navigation], identity);
        }
        handler.finish();
    }

    #[test]
    fn loaded_idle_tab_closes_drain_exact_context_ack_without_capacity_accumulation() {
        let polling = Arc::new(AtomicBool::new(false));
        let mut handler = BrowserHandler::new(None, None, polling);
        handler.session = Some(test_session());
        let window = BrowserWindowId::new(1).unwrap();

        for _ in 0..32 {
            let tab = match handler.session_mut().unwrap().open_tab(window).unwrap() {
                BrowserCommandOutcome::TabOpened { tab, .. } => tab,
                other => panic!("unexpected tab-open outcome: {other:?}"),
            };
            let navigation = load_and_consume_frame(&mut handler, tab);
            let document = handler
                .session
                .as_ref()
                .unwrap()
                .tab_snapshot(tab)
                .unwrap()
                .engine_live_version
                .unwrap();
            handler.rerender_suppressed.insert(
                tab,
                RerenderSuppression {
                    navigation,
                    document,
                    failure: None,
                },
            );
            let outcome = handler.session_mut().unwrap().close_tab(tab).unwrap();
            assert!(matches!(
                outcome,
                BrowserCommandOutcome::TabClosed {
                    tab: closed,
                    window_closed: false,
                } if closed == tab
            ));
            assert!(command_requires_engine_poll(outcome));
            handler.retire_rerender_authority_after_command(outcome);
            assert!(handler.rerender_pending.is_empty());
            assert!(handler.rerender_suppressed.is_empty());
            assert_eq!(handler.session.as_ref().unwrap().closing_context_count(), 1);

            for _ in 0..100_000 {
                if handler.session.as_ref().unwrap().closing_context_count() == 0 {
                    break;
                }
                match handler.session_mut().unwrap().poll_engine_once() {
                    Ok(EnginePumpOutcome::Empty) => thread::yield_now(),
                    Ok(_) => {}
                    Err(error) => panic!("context-close acknowledgement failed: {error}"),
                }
            }
            assert_eq!(
                handler.session.as_ref().unwrap().closing_context_count(),
                0,
                "every loaded-tab close must drain its exact engine acknowledgement",
            );
        }
        handler.finish();
    }

    #[test]
    fn inactive_tab_rerender_completion_clears_its_exact_pending_work() {
        let polling = Arc::new(AtomicBool::new(false));
        let mut handler = BrowserHandler::new(None, None, polling);
        handler.session = Some(test_session_with_rerender(RerenderBehavior::Rendered));
        let first = BrowserTabId::new(1).unwrap();
        let navigation = load_and_consume_frame(&mut handler, first);
        let second = match handler
            .session_mut()
            .unwrap()
            .open_tab(BrowserWindowId::new(1).unwrap())
            .unwrap()
        {
            BrowserCommandOutcome::TabOpened { tab, .. } => tab,
            other => panic!("unexpected tab outcome: {other:?}"),
        };
        assert_eq!(handler.active_tab().unwrap(), second);
        handler.request_rerender_if_possible(first).unwrap();
        assert_eq!(handler.rerender_pending[&first].navigation, navigation);
        drain_exact_rerender(&mut handler);
        assert!(handler.rerender_pending.is_empty());
        assert!(
            handler
                .session
                .as_ref()
                .unwrap()
                .tab_snapshot(first)
                .unwrap()
                .frame
                .is_some()
        );
        handler.finish();
    }

    #[test]
    fn rejected_and_resource_limited_rerenders_are_suppressed_to_quiescence() {
        for failure in [
            DocumentOperationFailure::Rendering,
            DocumentOperationFailure::ResourceLimit,
        ] {
            let polling = Arc::new(AtomicBool::new(false));
            let mut handler = BrowserHandler::new(None, None, polling);
            handler.session = Some(test_session_with_rerender(RerenderBehavior::Rejected(
                failure,
            )));
            let tab = BrowserTabId::new(1).unwrap();
            let navigation = load_and_consume_frame(&mut handler, tab);
            let document = handler
                .session
                .as_ref()
                .unwrap()
                .tab_snapshot(tab)
                .unwrap()
                .engine_live_version
                .unwrap();
            handler.request_rerender_if_possible(tab).unwrap();
            drain_exact_rerender(&mut handler);
            assert!(handler.rerender_pending.is_empty());
            assert_eq!(
                handler.rerender_suppressed[&tab],
                RerenderSuppression {
                    navigation,
                    document,
                    failure: Some(failure),
                }
            );
            handler.request_rerender_if_possible(tab).unwrap();
            assert!(handler.rerender_pending.is_empty());
            assert!(matches!(
                handler.session_mut().unwrap().poll_engine_once(),
                Ok(EnginePumpOutcome::Empty)
            ));
            handler.finish();
        }
    }

    #[test]
    fn terminal_no_frame_rerender_is_suppressed_to_quiescence_without_a_failure() {
        assert!(rerender_terminal_requires_suppression(None, false));
        assert!(rerender_terminal_requires_suppression(
            Some(DocumentOperationFailure::Rendering),
            true,
        ));
        assert!(!rerender_terminal_requires_suppression(None, true));
    }

    #[test]
    fn one_shell_route_inserts_committed_text_once() {
        let polling = Arc::new(AtomicBool::new(false));
        let mut handler = BrowserHandler::new(None, None, polling);
        handler.session = Some(test_session());
        let (surface, ready) = ready_event();
        handler.route_event(ready).expect("surface attaches");
        let metadata = InputMetadata {
            sequence: EventSequence::new(1).expect("test event is nonzero"),
            timestamp: EventTimestampMicros(1),
            seat: SeatId::new(1).expect("test seat is nonzero"),
            device: InputDeviceId::new(1).expect("test device is nonzero"),
            surface,
            modifiers: Modifiers::default(),
        };
        let input = InputEvent::Text(
            TextInputEvent::new(metadata, "x".to_owned()).expect("test text is bounded"),
        );
        handler
            .route_event(LinuxWindowEvent::Input {
                event: input,
                origin: InputOrigin::Synthetic,
            })
            .expect("one input event routes");
        let address = &handler
            .session
            .as_ref()
            .unwrap()
            .tab_snapshot(BrowserTabId::new(1).unwrap())
            .unwrap()
            .address;
        assert_eq!(address.as_ref(), "x");
        handler.finish();
    }

    #[test]
    fn pointer_final_tab_close_outcome_requests_native_exit() {
        let polling = Arc::new(AtomicBool::new(false));
        let mut handler = BrowserHandler::new(None, None, polling);
        handler.session = Some(test_session());
        let tab = BrowserTabId::new(1).unwrap();
        let outcome = handler.session_mut().unwrap().close_tab(tab).unwrap();
        assert!(matches!(
            outcome,
            BrowserCommandOutcome::SessionClosed { .. }
        ));
        assert!(command_requests_native_exit(outcome));
        handler.finish();
    }

    #[test]
    fn ctrl_w_final_tab_outcome_requests_native_exit_after_one_input_accounting() {
        let polling = Arc::new(AtomicBool::new(false));
        let mut handler = BrowserHandler::new(None, None, polling);
        handler.session = Some(test_session());
        let (surface, ready) = ready_event();
        handler.route_event(ready).unwrap();
        let outcome = handler
            .account_input_once(key_linux_event(surface, 1, 17, Modifiers::CONTROL))
            .unwrap();
        assert!(matches!(
            outcome,
            LinuxEventOutcome::Command(BrowserCommandOutcome::SessionClosed { .. })
        ));
        assert!(routed_outcome_requests_native_exit(&outcome));
        handler.finish();
    }

    #[test]
    fn keyboard_focus_and_tab_switch_outcomes_drive_active_tab_ime_policy() {
        let polling = Arc::new(AtomicBool::new(false));
        let mut handler = BrowserHandler::new(None, None, polling);
        handler.session = Some(test_session());
        let (surface, ready) = ready_event();
        handler.route_event(ready).unwrap();
        let first = BrowserTabId::new(1).unwrap();
        handler.session_mut().unwrap().focus_content(first).unwrap();
        assert!(!handler.active_tab_allows_ime().unwrap());

        let focus = handler
            .account_input_once(key_linux_event(surface, 1, 38, Modifiers::CONTROL))
            .unwrap();
        assert!(matches!(
            focus,
            LinuxEventOutcome::Command(BrowserCommandOutcome::AddressFocused { tab, .. })
                if tab == first
        ));
        assert!(handler.active_tab_allows_ime().unwrap());

        let second = match handler
            .session_mut()
            .unwrap()
            .open_tab(BrowserWindowId::new(1).unwrap())
            .unwrap()
        {
            BrowserCommandOutcome::TabOpened { tab, .. } => tab,
            other => panic!("unexpected tab outcome: {other:?}"),
        };
        handler
            .session_mut()
            .unwrap()
            .focus_content(second)
            .unwrap();
        handler.session_mut().unwrap().activate_tab(first).unwrap();
        assert!(handler.active_tab_allows_ime().unwrap());
        let switched = handler
            .account_input_once(key_linux_event(surface, 2, 15, Modifiers::CONTROL))
            .unwrap();
        assert!(matches!(
            switched,
            LinuxEventOutcome::Command(BrowserCommandOutcome::TabActivated { tab, .. })
                if tab == second
        ));
        assert!(!handler.active_tab_allows_ime().unwrap());
        handler.finish();
    }

    #[test]
    fn content_focused_page_hit_still_requests_authority_recovery_redraw() {
        let polling = Arc::new(AtomicBool::new(false));
        let mut handler = BrowserHandler::new(None, None, polling);
        handler.session = Some(test_session());
        let (surface, ready) = ready_event();
        handler.route_event(ready).unwrap();
        let tab = BrowserTabId::new(1).unwrap();
        handler.session_mut().unwrap().focus_content(tab).unwrap();
        let pointer = pointer_event(surface, 1, PRIMARY_POINTER_BUTTON);
        let outcome = handler
            .account_input_once(LinuxWindowEvent::Input {
                event: InputEvent::Pointer(pointer),
                origin: InputOrigin::Synthetic,
            })
            .unwrap();
        assert!(matches!(
            outcome,
            LinuxEventOutcome::ContentInputUnrouted { .. }
        ));
        assert!(!input_requires_redraw(&outcome, false));
        assert!(
            input_requires_redraw(&outcome, true),
            "an authoritative page hit must replace invalidated receipt authority",
        );
        handler.finish();
    }

    #[test]
    fn stale_double_close_is_accounted_then_ignored_and_nonprimary_chords_never_close() {
        let polling = Arc::new(AtomicBool::new(false));
        let mut handler = BrowserHandler::new(None, None, polling);
        handler.session = Some(test_session());
        let (surface, ready) = ready_event();
        handler.route_event(ready).unwrap();
        let first = BrowserTabId::new(1).unwrap();
        let second = match handler
            .session_mut()
            .unwrap()
            .open_tab(BrowserWindowId::new(1).unwrap())
            .unwrap()
        {
            BrowserCommandOutcome::TabOpened { tab, .. } => tab,
            other => panic!("unexpected tab outcome: {other:?}"),
        };

        let first_click = pointer_event(surface, 1, PRIMARY_POINTER_BUTTON);
        assert!(pointer_has_chrome_action_authority(&first_click));
        handler
            .account_input_once(LinuxWindowEvent::Input {
                event: InputEvent::Pointer(first_click),
                origin: InputOrigin::Synthetic,
            })
            .unwrap();
        assert!(handler.current_window_contains_tab(second).unwrap());
        handler.session_mut().unwrap().close_tab(second).unwrap();

        let stale_click = pointer_event(surface, 2, PRIMARY_POINTER_BUTTON);
        handler
            .account_input_once(LinuxWindowEvent::Input {
                event: InputEvent::Pointer(stale_click),
                origin: InputOrigin::Synthetic,
            })
            .unwrap();
        assert!(!handler.current_window_contains_tab(second).unwrap());

        for (sequence, buttons) in [(3, 2), (4, 4), (5, 3)] {
            let pointer = pointer_event(surface, sequence, buttons);
            assert!(!pointer_has_chrome_action_authority(&pointer));
            handler
                .account_input_once(LinuxWindowEvent::Input {
                    event: InputEvent::Pointer(pointer),
                    origin: InputOrigin::Synthetic,
                })
                .unwrap();
            assert!(handler.current_window_contains_tab(first).unwrap());
        }
        handler.finish();
    }

    #[test]
    fn terminal_browser_frame_failures_are_never_retryable() {
        let preaccept = ControlError::BrowserPresentationFailed {
            stage: WebRenderWindowFailureStage::ValidateRequest,
            kind: WebRenderWindowErrorKind::ResourceLimit,
            terminal: false,
        };
        let terminal = ControlError::BrowserPresentationFailed {
            stage: WebRenderWindowFailureStage::RenderFrame,
            kind: WebRenderWindowErrorKind::Renderer,
            terminal: true,
        };
        assert!(retry_browser_frame_after(preaccept));
        assert!(!retry_browser_frame_after(terminal));
        assert!(!retry_browser_frame_after(ControlError::NoLiveWindow));
    }

    #[test]
    fn persistent_preaccept_rejections_are_bounded_and_only_success_resets_the_budget() {
        let polling = Arc::new(AtomicBool::new(false));
        let mut handler = BrowserHandler::new(None, None, polling);
        for _ in 0..MAX_CONSECUTIVE_PREACCEPT_REJECTIONS {
            handler.record_preaccept_rejection().unwrap();
        }
        assert!(handler.record_preaccept_rejection().is_err());
        handler.record_successful_composition();
        assert_eq!(handler.consecutive_preaccept_rejections, 0);
        handler.record_preaccept_rejection().unwrap();
    }

    #[test]
    fn ordinary_runs_admit_only_local_requested_or_close_requested_stop() {
        assert!(native_stop_is_admitted(
            LinuxStopReason::Requested,
            false,
            false
        ));
        assert!(native_stop_is_admitted(
            LinuxStopReason::CloseRequested,
            false,
            false
        ));
        let fatal = [
            LinuxStopReason::EventQueueSaturated { capacity: 1 },
            LinuxStopReason::EventSequenceExhausted,
            LinuxStopReason::EventTimestampExhausted,
            LinuxStopReason::DeviceCapacityExhausted { capacity: 1 },
            LinuxStopReason::DeviceIdentityExhausted,
            LinuxStopReason::TouchCapacityExhausted { capacity: 1 },
            LinuxStopReason::PointerIdentityExhausted,
            LinuxStopReason::SurfaceIdentityExhausted,
            LinuxStopReason::SurfaceIdentityViolation,
            LinuxStopReason::WindowCreationFailed,
            LinuxStopReason::PresentationFailed(PresentationFailureStage::SwapBuffers),
            LinuxStopReason::BrowserPresentationFailed(WebRenderWindowFailureStage::RenderFrame),
            LinuxStopReason::WindowDestroyed,
            LinuxStopReason::InvalidPlatformGeometry,
            LinuxStopReason::InvalidTouchPressure,
            LinuxStopReason::InvalidImeText,
            LinuxStopReason::BackendExited,
        ];
        for reason in fatal {
            assert!(!native_stop_is_admitted(reason, false, false));
        }
        assert!(native_stop_is_admitted(
            LinuxStopReason::Requested,
            true,
            true
        ));
        assert!(!native_stop_is_admitted(
            LinuxStopReason::CloseRequested,
            true,
            true
        ));
    }

    #[test]
    fn product_run_requires_requested_clean_engine_shutdown() {
        let clean = EnginePortShutdownStatus::new(
            EnginePortStopReason::Requested,
            EnginePortExecutorShutdown::Clean,
        );
        assert!(engine_shutdown_is_admitted(clean));
        assert!(!engine_shutdown_is_admitted(EnginePortShutdownStatus::new(
            EnginePortStopReason::Requested,
            EnginePortExecutorShutdown::NotStarted,
        )));
        assert!(!engine_shutdown_is_admitted(EnginePortShutdownStatus::new(
            EnginePortStopReason::PortPanicked,
            EnginePortExecutorShutdown::Panicked,
        )));
    }

    #[test]
    fn live_scene_surface_or_tab_change_clears_and_requests_rerender() {
        for (active_changed, surface_stale) in [(true, false), (false, true)] {
            let fallback = select_page_fallback(true, active_changed || surface_stale, true);
            let (update, page, need_rerender) =
                materialize_page_fallback(fallback, BrowserPageSnapshot::Blank);
            assert!(matches!(update, BrowserPageUpdate::ClearToBlank));
            assert!(matches!(page, BrowserPageSnapshot::Blank));
            assert!(need_rerender);
        }
    }

    #[test]
    fn queued_redraw_is_suppressed_through_zero_and_explicit_suspend_until_recovery() {
        let polling = Arc::new(AtomicBool::new(false));
        let mut handler = BrowserHandler::new(None, None, polling);
        let nonzero = PhysicalSize::new(1_024, 768).unwrap();
        let zero = PhysicalSize::new(0, 768).unwrap();
        handler.surface.mark_ready();

        assert!(handler.can_draw_surface(nonzero));
        handler.mark_explicitly_suspended();
        assert!(!handler.can_draw_surface(nonzero));
        assert!(handler.surface.stale);
        assert_eq!(
            handler.surface.admission,
            SurfaceAdmission::ExplicitlySuspended
        );
        assert!(!handler.record_surface_transition(false, nonzero));
        assert!(!handler.can_draw_surface(nonzero));

        assert!(handler.record_surface_transition(true, nonzero));
        assert!(handler.can_draw_surface(nonzero));
        assert!(handler.surface.stale);
        assert_eq!(handler.surface.admission, SurfaceAdmission::Presentable);

        assert!(!handler.record_surface_transition(false, zero));
        assert!(!handler.can_draw_surface(zero));
        assert_eq!(handler.surface.admission, SurfaceAdmission::ZeroSized);
        assert!(handler.record_surface_transition(false, nonzero));
        assert!(handler.can_draw_surface(nonzero));
        assert!(handler.surface.stale);
        assert_eq!(handler.surface.admission, SurfaceAdmission::Presentable);
    }

    #[test]
    fn tiny_nonzero_surface_is_presentable_as_incompatible_blank_chrome_and_recovers() {
        let mut surface = BrowserSurfaceState::default();
        surface.mark_ready();
        let engine = PhysicalSize::new(1_024, 688).unwrap();
        let tiny = PhysicalSize::new(1_024, 1).unwrap();
        assert!(surface.record_transition(false, tiny));
        assert!(!viewport_matches_engine(Some(engine), None));
        surface.set_viewport_compatible(false);
        assert!(surface.is_presentable());
        assert!(surface.stale);
        assert!(!surface.viewport_compatible());

        assert!(surface.record_transition(false, PhysicalSize::new(1_024, 768).unwrap()));
        assert!(viewport_matches_engine(Some(engine), Some(engine)));
        surface.set_viewport_compatible(true);
        assert!(surface.viewport_compatible());
    }

    #[test]
    fn teardown_failure_and_unconfirmed_release_are_rejected_but_confirmed_release_is_admitted() {
        assert!(!browser_teardown_is_admitted(
            BrowserTeardownClass::BrowserTeardownFailed
        ));
        for (backend_acknowledged, renderer_deinitialized) in
            [(false, false), (true, false), (false, true)]
        {
            assert!(!browser_teardown_is_admitted(
                BrowserTeardownClass::BrowserWrappersReleased {
                    backend_acknowledged,
                    renderer_deinitialized,
                }
            ));
        }
        assert!(browser_teardown_is_admitted(
            BrowserTeardownClass::BrowserWrappersReleased {
                backend_acknowledged: true,
                renderer_deinitialized: true,
            }
        ));
    }

    #[test]
    fn smoke_completion_requires_completed_state_and_exact_requested_stop() {
        assert!(is_completed_smoke_exit(LinuxStopReason::Requested, true));
        assert!(!is_completed_smoke_exit(LinuxStopReason::Requested, false));
        assert!(!is_completed_smoke_exit(
            LinuxStopReason::BackendExited,
            true
        ));
    }

    #[test]
    fn pre_holding_smoke_quiescence_keeps_watchdog_wake_admission() {
        let polling = Arc::new(AtomicBool::new(false));
        let smoke = BrowserSmokeConfig {
            second_url: "http://127.0.0.1:1/second".into(),
            hard_deadline: Duration::from_secs(20),
        };
        let mut handler = BrowserHandler::new(None, Some(smoke), polling);
        handler.smoke_stage = SmokeStage::AwaitResizeAway {
            initial: PhysicalSize::new(1_024, 768).unwrap(),
        };
        assert!(
            handler.unfinished_smoke_requires_polling(),
            "no engine work must not disable the only internal-deadline wake path",
        );
        handler.smoke_completed = true;
        assert!(!handler.unfinished_smoke_requires_polling());

        let ordinary = BrowserHandler::new(None, None, Arc::new(AtomicBool::new(false)));
        assert!(!ordinary.unfinished_smoke_requires_polling());
    }

    #[test]
    fn preaccept_rejection_then_later_submitted_surface_advances_resize_smoke() {
        let polling = Arc::new(AtomicBool::new(false));
        let mut handler = BrowserHandler::new(None, None, polling);
        let initial = PhysicalSize::new(1_024, 768).unwrap();
        let smaller = PhysicalSize::new(864, 648).unwrap();
        handler.smoke_stage = SmokeStage::AwaitResizeAway { initial };

        assert!(!smoke_transition_has_receipt(
            DrawOutcome::PreacceptRejected
        ));
        assert!(!smoke_transition_has_receipt(DrawOutcome::Deferred));
        assert!(smoke_transition_has_receipt(DrawOutcome::Submitted));
        assert!(matches!(
            handler.smoke_stage,
            SmokeStage::AwaitResizeAway { initial: actual } if actual == initial
        ));

        // `draw` invokes this exact commit hook for every later submitted
        // receipt, including a RedrawRequested recovery after preaccept.
        assert_eq!(
            handler.commit_smoke_resize_submission(smaller),
            SmokeResizeAction::RequestInnerSize(initial)
        );
        assert!(matches!(
            handler.smoke_stage,
            SmokeStage::AwaitResizeBack { initial: actual } if actual == initial
        ));
        assert_eq!(
            handler.commit_smoke_resize_submission(initial),
            SmokeResizeAction::RequestRerender
        );
        assert!(matches!(handler.smoke_stage, SmokeStage::AwaitFinalPage));
    }

    #[test]
    fn chrome_pointer_sequence_is_accounted_before_an_early_action() {
        let polling = Arc::new(AtomicBool::new(false));
        let mut handler = BrowserHandler::new(None, None, polling);
        handler.session = Some(test_session());
        let (surface, ready) = ready_event();
        handler.route_event(ready).expect("surface attaches");
        let second = match handler
            .session_mut()
            .unwrap()
            .open_tab(BrowserWindowId::new(1).unwrap())
            .unwrap()
        {
            BrowserCommandOutcome::TabOpened { tab, .. } => tab,
            other => panic!("unexpected tab outcome: {other:?}"),
        };
        let metadata = InputMetadata {
            sequence: EventSequence::new(1).expect("test event is nonzero"),
            timestamp: EventTimestampMicros(1),
            seat: SeatId::new(1).expect("test seat is nonzero"),
            device: InputDeviceId::new(1).expect("test device is nonzero"),
            surface,
            modifiers: Modifiers::default(),
        };
        let event = LinuxWindowEvent::Input {
            event: InputEvent::Pointer(PointerEvent {
                metadata,
                pointer: PointerId::new(1).expect("test pointer is nonzero"),
                kind: PointerKind::Mouse,
                phase: PointerPhase::Down,
                position: LogicalPoint::new(10.0, 10.0).unwrap(),
                buttons: 1,
                pressure: None,
            }),
            origin: InputOrigin::Synthetic,
        };
        handler
            .account_input_once(event.clone())
            .expect("pointer sequence is accounted before the chrome action");
        handler
            .session_mut()
            .unwrap()
            .activate_tab(second)
            .expect("the early tab action remains independently typed");
        assert_eq!(handler.active_tab().unwrap(), second);
        assert!(
            handler.account_input_once(event).is_err(),
            "replaying the already-accounted pointer sequence must fail closed"
        );
        handler.finish();
    }
}
