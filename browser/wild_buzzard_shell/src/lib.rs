//! Same-process Rust browser-product integration for one Linux top-level window.

#![forbid(unsafe_code)]

#[cfg(feature = "webdriver")]
mod automation;

#[cfg(feature = "webdriver")]
pub use automation::BrowserWebDriverConfig;

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use wild_buzzard_engine::{
    DocumentOperationFailure, DocumentOperationId, EngineLimits, GeneralWebConfig,
    MAX_NAVIGATION_URL_BYTES, NavigationId, StaticPageConfig, TrustStore,
};
use wild_buzzard_linux::{
    BrowserAddressSelection, BrowserChromeDirection, BrowserChromeElementIdentity,
    BrowserChromeFocus, BrowserChromeGeometry, BrowserChromeRevision, BrowserChromeScene,
    BrowserChromeState, BrowserChromeTab, BrowserElementAvailability, BrowserElementExpansion,
    BrowserElementInteraction, BrowserElementSelection, BrowserFrameReceipt, BrowserFrameRequest,
    BrowserHitTarget, BrowserNavigationIdentity, BrowserPageSnapshot, BrowserPageUpdate,
    BrowserPrimaryActionKind, BrowserPrimaryChromeState, BrowserPrimaryControl,
    BrowserPrimaryControlKind, BrowserPrimaryControlPlacement, BrowserPrimaryLayoutPreview,
    BrowserPrimaryPopup, BrowserPrimaryPopupKind, BrowserPrimaryPopupRow,
    BrowserPrimaryPopupRowKind, BrowserReloadStopMode, BrowserSiteIdentityKind, BrowserTabIdentity,
    ControlError, LinuxBackend, LinuxPresentationMode, LinuxPresentationShutdown, LinuxShellConfig,
    LinuxShutdownReport, LinuxStopReason, LinuxWakeHandle, LinuxWakeStatus, LinuxWindowControl,
    LinuxWindowEvent, LinuxWindowHandler, LinuxWindowShell, MAX_BROWSER_CHROME_GLYPHS,
    MAX_BROWSER_CHROME_RUNS, MAX_BROWSER_CHROME_TABS, MAX_BROWSER_CHROME_TEXT_BYTES,
    MAX_BROWSER_CHROME_TEXTS, MAX_BROWSER_PRIMARY_CONTROLS, MAX_BROWSER_PRIMARY_POPUP_ROWS,
    PhysicalPoint, PhysicalSize, SurfaceNamespace, WebRenderSurfaceSnapshot,
};
use wild_buzzard_platform::{
    InputEvent, PointerEvent, PointerPhase, ScrollDelta, ScrollEvent, ScrollPhase,
};
use wild_buzzard_text::{TextLimits, TextRequest, TextShutdownReport, TextSystem};
use wild_buzzard_ui::{
    BrowserCommandOutcome, BrowserNavigationMode, BrowserSession, BrowserTabId, BrowserWindowId,
    EngineDocumentVersion, EnginePortExecutorShutdown, EnginePortFrameLeaseId,
    EnginePortShutdownStatus, EnginePortStopReason, EnginePumpOutcome, LinuxEventOutcome,
    MAX_PRIMARY_UI_LABEL_BYTES, MAX_PRIMARY_UI_SCROLL_ROWS, NavigationEnginePort,
    PrimaryReloadStopMode, PrimarySiteIdentityKind, PrimaryUiActionBinding, PrimaryUiActionOutcome,
    PrimaryUiAvailability, PrimaryUiControl, PrimaryUiControlSet, PrimaryUiDirection,
    PrimaryUiElementId, PrimaryUiFocus, PrimaryUiLayout, PrimaryUiMoveDirection, PrimaryUiPanel,
    PrimaryUiPanelItemAction, PrimaryUiPanelItemId, PrimaryUiSnapshot, SessionLifecycle,
    SessionLimits,
};

const POLL_INTERVAL: Duration = Duration::from_millis(10);
const SMOKE_HOLD: Duration = Duration::from_secs(3);
const TAB_FONT_SIZE_PX: f32 = 14.0;
const ADDRESS_FONT_SIZE_PX: f32 = 16.0;
const STATUS_FONT_SIZE_PX: f32 = 13.0;
const PRIMARY_CONTROL_FONT_SIZE_PX: f32 = 13.0;
const PRIMARY_POPUP_FONT_SIZE_PX: f32 = 14.0;
const PRIMARY_POINTER_BUTTON: u16 = 1;
const MAX_CONSECUTIVE_PREACCEPT_REJECTIONS: u8 = 8;
// Chrome is a projection of canonical session state. These visual limits make
// every session state admitted by this shell representable by the compositor.
const MAX_TAB_LABEL_BYTES: usize = 32;
const MAX_ADDRESS_LABEL_BYTES: usize = 1_792;
const MAX_STATUS_LABEL_BYTES: usize = 256;
const MAX_PRIMARY_CONTROL_LABEL_BYTES: usize = 64;
const MAX_PRIMARY_ACTION_LABEL_BYTES: usize = 128;
const MAX_PRIMARY_POPUP_LABEL_BYTES: usize = MAX_BROWSER_CHROME_TABS * MAX_TAB_LABEL_BYTES;
const MAX_CHROME_LABEL_BYTES: usize = MAX_BROWSER_CHROME_TABS * MAX_TAB_LABEL_BYTES
    + MAX_ADDRESS_LABEL_BYTES
    + MAX_STATUS_LABEL_BYTES
    + MAX_BROWSER_PRIMARY_CONTROLS * MAX_PRIMARY_CONTROL_LABEL_BYTES
    + MAX_PRIMARY_POPUP_LABEL_BYTES;
// Navigation identities are never reused. The lookup retains only exact live
// engine navigations; old page scenes and receipts copy the value they need.
const MAX_GRAPHICS_NAVIGATIONS: usize = 4_096;
const MAX_GRAPHICS_UI_ELEMENTS: usize = MAX_BROWSER_CHROME_TABS * 3 + 32;

const _: () = assert!(MAX_CHROME_LABEL_BYTES <= MAX_BROWSER_CHROME_TEXT_BYTES);
const _: () = assert!(MAX_CHROME_LABEL_BYTES <= MAX_BROWSER_CHROME_RUNS);
const _: () = assert!(MAX_CHROME_LABEL_BYTES <= MAX_BROWSER_CHROME_GLYPHS);
const _: () = assert!(MAX_PRIMARY_CONTROL_LABEL_BYTES <= MAX_PRIMARY_UI_LABEL_BYTES);
const _: () = assert!(MAX_PRIMARY_ACTION_LABEL_BYTES <= MAX_PRIMARY_UI_LABEL_BYTES);
const _: () = assert!(
    MAX_BROWSER_CHROME_TABS + 2 + MAX_BROWSER_PRIMARY_CONTROLS + MAX_BROWSER_PRIMARY_POPUP_ROWS
        <= MAX_BROWSER_CHROME_TEXTS
);

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

const fn graphics_direction(direction: PrimaryUiDirection) -> BrowserChromeDirection {
    match direction {
        PrimaryUiDirection::LeftToRight => BrowserChromeDirection::LeftToRight,
        PrimaryUiDirection::RightToLeft => BrowserChromeDirection::RightToLeft,
    }
}

const fn graphics_control_kind(control: PrimaryUiControl) -> BrowserPrimaryControlKind {
    match control {
        PrimaryUiControl::Back => BrowserPrimaryControlKind::Back,
        PrimaryUiControl::Forward => BrowserPrimaryControlKind::Forward,
        PrimaryUiControl::ReloadStop => BrowserPrimaryControlKind::ReloadStop,
        PrimaryUiControl::SiteIdentity => BrowserPrimaryControlKind::SiteIdentity,
        PrimaryUiControl::AddressBar => BrowserPrimaryControlKind::UrlBar,
        PrimaryUiControl::NewTab => BrowserPrimaryControlKind::NewTab,
        PrimaryUiControl::AllTabs => BrowserPrimaryControlKind::AllTabs,
        PrimaryUiControl::ApplicationMenu => BrowserPrimaryControlKind::ApplicationMenu,
        PrimaryUiControl::Overflow => BrowserPrimaryControlKind::Overflow,
    }
}

const fn primary_control_kind(control: BrowserPrimaryControlKind) -> PrimaryUiControl {
    match control {
        BrowserPrimaryControlKind::Back => PrimaryUiControl::Back,
        BrowserPrimaryControlKind::Forward => PrimaryUiControl::Forward,
        BrowserPrimaryControlKind::ReloadStop => PrimaryUiControl::ReloadStop,
        BrowserPrimaryControlKind::SiteIdentity => PrimaryUiControl::SiteIdentity,
        BrowserPrimaryControlKind::UrlBar => PrimaryUiControl::AddressBar,
        BrowserPrimaryControlKind::NewTab => PrimaryUiControl::NewTab,
        BrowserPrimaryControlKind::AllTabs => PrimaryUiControl::AllTabs,
        BrowserPrimaryControlKind::ApplicationMenu => PrimaryUiControl::ApplicationMenu,
        BrowserPrimaryControlKind::Overflow => PrimaryUiControl::Overflow,
    }
}

const fn graphics_availability(availability: PrimaryUiAvailability) -> BrowserElementAvailability {
    match availability {
        PrimaryUiAvailability::Disabled => BrowserElementAvailability::Disabled,
        PrimaryUiAvailability::Enabled => BrowserElementAvailability::Enabled,
    }
}

const fn graphics_reload_stop(mode: PrimaryReloadStopMode) -> BrowserReloadStopMode {
    match mode {
        PrimaryReloadStopMode::Reload => BrowserReloadStopMode::Reload,
        PrimaryReloadStopMode::Stop => BrowserReloadStopMode::Stop,
    }
}

const fn graphics_site_identity(identity: PrimarySiteIdentityKind) -> BrowserSiteIdentityKind {
    match identity {
        PrimarySiteIdentityKind::NoPage => BrowserSiteIdentityKind::Empty,
        PrimarySiteIdentityKind::LoopbackHttp => BrowserSiteIdentityKind::LoopbackHttp,
        PrimarySiteIdentityKind::InsecureHttp | PrimarySiteIdentityKind::Unverified => {
            BrowserSiteIdentityKind::Insecure
        }
    }
}

const fn graphics_popup_kind(panel: PrimaryUiPanel) -> BrowserPrimaryPopupKind {
    match panel {
        PrimaryUiPanel::SiteIdentity => BrowserPrimaryPopupKind::SiteIdentity,
        PrimaryUiPanel::AllTabs => BrowserPrimaryPopupKind::AllTabs,
        PrimaryUiPanel::ApplicationMenu => BrowserPrimaryPopupKind::ApplicationMenu,
        PrimaryUiPanel::Overflow => BrowserPrimaryPopupKind::Overflow,
    }
}

fn primary_layout_from_preview(
    preview: &BrowserPrimaryLayoutPreview,
) -> Result<PrimaryUiLayout, BrowserShellError> {
    let mut visible = PrimaryUiControlSet::empty();
    let mut overflowed = PrimaryUiControlSet::empty();
    for control in preview.controls() {
        let primary = primary_control_kind(control.kind());
        match control.placement() {
            BrowserPrimaryControlPlacement::Toolbar
            | BrowserPrimaryControlPlacement::AddressField => {
                visible = visible.with(primary);
            }
            BrowserPrimaryControlPlacement::OverflowPanel => {
                overflowed = overflowed.with(primary);
            }
            BrowserPrimaryControlPlacement::Hidden if primary == PrimaryUiControl::Overflow => {}
            BrowserPrimaryControlPlacement::Hidden => {
                return Err(BrowserShellError::new(
                    "pure primary layout hid a required functional control",
                ));
            }
        }
    }
    PrimaryUiLayout::new(visible, overflowed, preview.popup_row_capacity())
        .map_err(BrowserShellError::new)
}

fn primary_ui_element_is_live(snapshot: &PrimaryUiSnapshot, element: PrimaryUiElementId) -> bool {
    match element {
        PrimaryUiElementId::Page
        | PrimaryUiElementId::Control(_)
        | PrimaryUiElementId::PanelItem(
            PrimaryUiPanelItemId::IdentitySummary
            | PrimaryUiPanelItemId::ApplicationNewTab
            | PrimaryUiPanelItemId::ApplicationCloseTab
            | PrimaryUiPanelItemId::ApplicationBack
            | PrimaryUiPanelItemId::ApplicationForward
            | PrimaryUiPanelItemId::ApplicationReloadStop,
        ) => true,
        PrimaryUiElementId::Tab(tab) | PrimaryUiElementId::TabClose(tab) => {
            snapshot.tabs.iter().any(|candidate| candidate.tab == tab)
        }
        PrimaryUiElementId::PanelItem(PrimaryUiPanelItemId::AllTabsTab(tab)) => {
            snapshot.tabs.iter().any(|candidate| candidate.tab == tab)
        }
        PrimaryUiElementId::PanelItem(PrimaryUiPanelItemId::OverflowControl(_)) => false,
    }
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
    ) || matches!(
        outcome,
        LinuxEventOutcome::PrimaryUi(PrimaryUiActionOutcome::Command(command))
            if command_requests_native_exit(*command)
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
    ) || matches!(
        outcome,
        LinuxEventOutcome::PrimaryUi(PrimaryUiActionOutcome::Command(command))
            if command_requires_engine_poll(*command)
    )
}

const fn routed_outcome_mutates_chrome(outcome: &LinuxEventOutcome) -> bool {
    matches!(outcome, LinuxEventOutcome::AddressEdited { .. })
        || matches!(
            outcome,
            LinuxEventOutcome::Command(command)
                if !matches!(command, BrowserCommandOutcome::NoChange)
        )
        || matches!(
            outcome,
            LinuxEventOutcome::PrimaryUi(
                PrimaryUiActionOutcome::Command(_)
                    | PrimaryUiActionOutcome::FocusChanged { .. }
                    | PrimaryUiActionOutcome::PanelChanged(_)
                    | PrimaryUiActionOutcome::PanelScrolled { .. }
            )
        )
}

const fn input_requires_redraw(outcome: &LinuxEventOutcome, page_hit_applied: bool) -> bool {
    page_hit_applied || !matches!(outcome, LinuxEventOutcome::ContentInputUnrouted { .. })
}

const PRIMARY_POPUP_SCROLL_ROW_PIXELS: f64 = 40.0;

fn bounded_popup_scroll_rows(
    vertical: f64,
    row_units: f64,
) -> Option<(PrimaryUiMoveDirection, u8)> {
    if vertical == 0.0 {
        return None;
    }
    let bounded = row_units
        .ceil()
        .clamp(1.0, f64::from(MAX_PRIMARY_UI_SCROLL_ROWS));
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let rows = bounded as u8;
    Some((
        if vertical < 0.0 {
            PrimaryUiMoveDirection::Forward
        } else {
            PrimaryUiMoveDirection::Backward
        },
        rows,
    ))
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

#[cfg_attr(not(feature = "webdriver"), derive(Clone, Copy))]
enum AutomationStartup {
    Disabled,
    #[cfg(feature = "webdriver")]
    Enabled(BrowserWebDriverConfig),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BrowserScriptExecution {
    Static,
    #[cfg(feature = "contained_inline_classic")]
    ContainedInlineClassic,
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
    run_browser_configured(
        backend,
        initial_url,
        smoke,
        AutomationStartup::Disabled,
        BrowserScriptExecution::Static,
    )
}

/// Runs the visible browser with the default-off numeric-loopback Rust
/// parser-blocking JavaScript integration gate.
///
/// This entry point exists for live integration testing. It does not enable
/// general-web product script admission.
///
/// # Errors
///
/// Returns the same failures as [`run_browser`].
#[cfg(feature = "contained_inline_classic")]
pub fn run_browser_contained_inline_classic(
    backend: Option<LinuxBackend>,
    initial_url: Option<Box<str>>,
    smoke: Option<BrowserSmokeConfig>,
) -> Result<BrowserRunReport, BrowserShellError> {
    run_browser_configured(
        backend,
        initial_url,
        smoke,
        AutomationStartup::Disabled,
        BrowserScriptExecution::ContainedInlineClassic,
    )
}

/// Runs the browser with one explicitly configured authenticated `WebDriver`
/// Classic endpoint. This API exists only when the default-off `webdriver`
/// feature is selected.
///
/// # Errors
///
/// Returns [`BrowserShellError`] for automation startup/shutdown failures in
/// addition to the ordinary browser failures documented by [`run_browser`].
#[cfg(feature = "webdriver")]
pub fn run_browser_with_webdriver(
    backend: Option<LinuxBackend>,
    initial_url: Option<Box<str>>,
    smoke: Option<BrowserSmokeConfig>,
    webdriver: BrowserWebDriverConfig,
) -> Result<BrowserRunReport, BrowserShellError> {
    run_browser_configured(
        backend,
        initial_url,
        smoke,
        AutomationStartup::Enabled(webdriver),
        BrowserScriptExecution::Static,
    )
}

/// Runs the contained Rust JavaScript presentation gate with one authenticated
/// WebDriver Classic endpoint.
///
/// # Errors
///
/// Returns the combined browser and automation failures documented by
/// [`run_browser_with_webdriver`].
#[cfg(all(feature = "webdriver", feature = "contained_inline_classic"))]
pub fn run_browser_contained_inline_classic_with_webdriver(
    backend: Option<LinuxBackend>,
    initial_url: Option<Box<str>>,
    smoke: Option<BrowserSmokeConfig>,
    webdriver: BrowserWebDriverConfig,
) -> Result<BrowserRunReport, BrowserShellError> {
    run_browser_configured(
        backend,
        initial_url,
        smoke,
        AutomationStartup::Enabled(webdriver),
        BrowserScriptExecution::ContainedInlineClassic,
    )
}

#[allow(clippy::too_many_lines)]
fn run_browser_configured(
    backend: Option<LinuxBackend>,
    initial_url: Option<Box<str>>,
    smoke: Option<BrowserSmokeConfig>,
    automation: AutomationStartup,
    script_execution: BrowserScriptExecution,
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
    #[cfg(feature = "webdriver")]
    let (mut automation_listener, automation_owner) = match automation {
        AutomationStartup::Disabled => (None, None),
        AutomationStartup::Enabled(config) => {
            let (listener, owner) =
                automation::start(config, wake.clone()).map_err(BrowserShellError::new)?;
            (Some(listener), Some(owner))
        }
    };
    #[cfg(not(feature = "webdriver"))]
    let _ = automation;
    let running = Arc::new(AtomicBool::new(true));
    let polling = Arc::new(AtomicBool::new(smoke.is_some()));
    let poll_thread = spawn_poll_thread(wake, Arc::clone(&running), Arc::clone(&polling))?;
    let mut handler = BrowserHandler::new(initial_url, smoke, polling);
    handler.configure_script_execution(script_execution);
    #[cfg(feature = "webdriver")]
    {
        handler.automation = automation_owner;
    }
    let native = shell.run(&mut handler).map_err(BrowserShellError::new);
    running.store(false, Ordering::Release);
    poll_thread
        .join()
        .map_err(|_| BrowserShellError::new("payload-free engine wake thread panicked"))?;
    #[cfg(feature = "webdriver")]
    if let Some(listener) = automation_listener.as_mut() {
        listener.shutdown().map_err(BrowserShellError::new)?;
    }
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
struct PresentationCommitIdentity {
    tab: BrowserTabId,
    navigation: NavigationId,
    document: EngineDocumentVersion,
    lease: EnginePortFrameLeaseId,
    scene_revision: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NativePresentationCommitOutcome {
    Pending,
    SubmissionInProgress,
    NativeCommitted,
    #[cfg(feature = "webdriver")]
    NotCommitted,
    #[cfg(feature = "webdriver")]
    Cancelled,
}

struct NativePresentationCommitMarker<'a> {
    outcome: &'a mut NativePresentationCommitOutcome,
}

impl NativePresentationCommitMarker<'_> {
    fn begin_submission(&mut self) -> bool {
        if *self.outcome != NativePresentationCommitOutcome::Pending {
            return false;
        }
        *self.outcome = NativePresentationCommitOutcome::SubmissionInProgress;
        true
    }

    fn mark_native_committed(&mut self) -> bool {
        if *self.outcome != NativePresentationCommitOutcome::SubmissionInProgress {
            return false;
        }
        *self.outcome = NativePresentationCommitOutcome::NativeCommitted;
        true
    }

    fn commit_shell_state<R>(&mut self, operation: impl FnOnce() -> R) -> Option<R> {
        (*self.outcome == NativePresentationCommitOutcome::NativeCommitted).then(operation)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PresentedUiHit {
    Page,
    Tab(BrowserTabIdentity),
    TabClose(BrowserTabIdentity),
    AddressBar,
    PrimaryControl {
        element: BrowserChromeElementIdentity,
        kind: BrowserPrimaryControlKind,
    },
    PopupRow {
        element: BrowserChromeElementIdentity,
        kind: BrowserPrimaryPopupRowKind,
    },
    PopupDismiss {
        kind: BrowserPrimaryPopupKind,
        anchor: BrowserChromeElementIdentity,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PresentedUiDisposition {
    Action(PrimaryUiActionBinding),
    ConsumeDisabled,
}

struct PresentedPopupAuthority {
    kind: BrowserPrimaryPopupKind,
    anchor: BrowserChromeElementIdentity,
}

#[derive(Default)]
struct PresentedUiAuthority {
    entries: Vec<(PresentedUiHit, PresentedUiDisposition)>,
    popup: Option<PresentedPopupAuthority>,
}

impl PresentedUiAuthority {
    fn disposition(&self, hit: PresentedUiHit) -> Option<PresentedUiDisposition> {
        self.entries
            .iter()
            .find_map(|(candidate, disposition)| (*candidate == hit).then_some(*disposition))
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.popup = None;
    }

    fn push(
        &mut self,
        hit: PresentedUiHit,
        disposition: PresentedUiDisposition,
    ) -> Result<(), BrowserShellError> {
        if self.entries.iter().any(|(candidate, _)| *candidate == hit) {
            return Err(BrowserShellError::new(
                "primary hit authority contains a duplicate exact target",
            ));
        }
        self.entries
            .try_reserve(1)
            .map_err(BrowserShellError::new)?;
        self.entries.push((hit, disposition));
        Ok(())
    }

    fn push_action(
        &mut self,
        hit: PresentedUiHit,
        binding: PrimaryUiActionBinding,
    ) -> Result<(), BrowserShellError> {
        self.push(hit, PresentedUiDisposition::Action(binding))
    }

    fn push_disabled(&mut self, hit: PresentedUiHit) -> Result<(), BrowserShellError> {
        self.push(hit, PresentedUiDisposition::ConsumeDisabled)
    }

    fn install_popup(
        &mut self,
        kind: BrowserPrimaryPopupKind,
        anchor: BrowserChromeElementIdentity,
    ) -> Result<(), BrowserShellError> {
        if self.popup.is_some() {
            return Err(BrowserShellError::new(
                "primary hit authority contains more than one popup",
            ));
        }
        self.popup = Some(PresentedPopupAuthority { kind, anchor });
        Ok(())
    }

    fn popup_matches(
        &self,
        kind: BrowserPrimaryPopupKind,
        anchor: BrowserChromeElementIdentity,
    ) -> bool {
        self.popup
            .as_ref()
            .is_some_and(|popup| popup.kind == kind && popup.anchor == anchor)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PresentedPointerRegion {
    Target {
        hit: PresentedUiHit,
        disposition: PresentedUiDisposition,
    },
    PopupSurface {
        kind: BrowserPrimaryPopupKind,
        anchor: BrowserChromeElementIdentity,
    },
}

enum PresentedPointerLookup {
    Region(PresentedPointerRegion),
    Other,
    InvalidAuthority,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PresentedReceiptIdentity {
    surface: wild_buzzard_platform::SurfaceId,
    surface_revision: u64,
    chrome_revision: u64,
    root_epoch: u32,
    sequence: u64,
    backend_publish_id: u64,
}

impl PresentedReceiptIdentity {
    fn from_receipt(receipt: BrowserFrameReceipt) -> Self {
        let request = receipt.request();
        let surface = request.surface();
        Self {
            surface: surface.surface(),
            surface_revision: surface.revision().get(),
            chrome_revision: request.chrome_revision().get(),
            root_epoch: request.epoch(),
            sequence: request.sequence(),
            backend_publish_id: receipt.backend_publish_id(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PresentedPointerContact {
    receipt: PresentedReceiptIdentity,
    pointer: wild_buzzard_platform::PointerId,
    seat: wild_buzzard_platform::SeatId,
    device: wild_buzzard_platform::InputDeviceId,
    kind: wild_buzzard_platform::PointerKind,
    surface: wild_buzzard_platform::SurfaceId,
    region: PresentedPointerRegion,
}

impl PresentedPointerContact {
    const fn with_receipt_identity(mut self, receipt: PresentedReceiptIdentity) -> Self {
        self.receipt = receipt;
        self
    }

    fn exact_action(self) -> Option<PrimaryUiActionBinding> {
        match self.region {
            PresentedPointerRegion::Target {
                disposition: PresentedUiDisposition::Action(binding),
                ..
            } => Some(binding),
            PresentedPointerRegion::Target {
                disposition: PresentedUiDisposition::ConsumeDisabled,
                ..
            }
            | PresentedPointerRegion::PopupSurface { .. } => None,
        }
    }

    fn visual_hit(self) -> Option<PresentedUiHit> {
        match self.region {
            PresentedPointerRegion::Target {
                hit,
                disposition: PresentedUiDisposition::Action(_),
            } if hit.has_pointer_visual() => Some(hit),
            PresentedPointerRegion::Target { .. } | PresentedPointerRegion::PopupSurface { .. } => {
                None
            }
        }
    }

    fn is_current(self, authority: &PresentedUiAuthority) -> bool {
        match self.region {
            PresentedPointerRegion::Target { hit, disposition } => {
                authority.disposition(hit) == Some(disposition)
            }
            PresentedPointerRegion::PopupSurface { kind, anchor } => {
                authority.popup_matches(kind, anchor)
            }
        }
    }
}

impl PresentedUiHit {
    const fn has_pointer_visual(self) -> bool {
        matches!(
            self,
            Self::Tab(_)
                | Self::TabClose(_)
                | Self::AddressBar
                | Self::PrimaryControl { .. }
                | Self::PopupRow { .. }
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PointerVisualRedrawToken {
    generation: u64,
    source_receipt: PresentedReceiptIdentity,
    signature: Option<(PresentedUiHit, BrowserElementInteraction)>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct PointerReceiptHandoff {
    generation: Option<u64>,
    hover: Option<PresentedPointerContact>,
    capture: Option<PresentedPointerContact>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PresentedPointerTransition {
    activation: Option<PrimaryUiActionBinding>,
    visual_changed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct PopupPixelScrollAccumulator {
    receipt: Option<PresentedReceiptIdentity>,
    surface: wild_buzzard_platform::SurfaceId,
    seat: wild_buzzard_platform::SeatId,
    device: wild_buzzard_platform::InputDeviceId,
    kind: BrowserPrimaryPopupKind,
    pixels: f64,
}

#[derive(Default)]
struct PresentedPointerState {
    hover: Option<PresentedPointerContact>,
    capture: Option<PresentedPointerContact>,
    pending_visual_redraw: Option<PointerVisualRedrawToken>,
    popup_pixel_scroll: Option<PopupPixelScrollAccumulator>,
    visual_generation: u64,
}

impl PresentedPointerState {
    fn clear_pointer_contacts(&mut self) {
        self.hover = None;
        self.capture = None;
        self.pending_visual_redraw = None;
    }

    fn clear(&mut self) {
        self.clear_pointer_contacts();
        self.popup_pixel_scroll = None;
    }

    fn visual_interaction(
        &self,
        hit: PresentedUiHit,
        receipt: Option<BrowserFrameReceipt>,
    ) -> BrowserElementInteraction {
        let Some(receipt) = receipt else {
            return BrowserElementInteraction::Idle;
        };
        self.visual_interaction_for(hit, PresentedReceiptIdentity::from_receipt(receipt))
    }

    fn visual_interaction_for(
        &self,
        hit: PresentedUiHit,
        receipt: PresentedReceiptIdentity,
    ) -> BrowserElementInteraction {
        if self
            .capture
            .is_some_and(|contact| contact.receipt == receipt && contact.visual_hit() == Some(hit))
        {
            BrowserElementInteraction::Pressed
        } else if self
            .hover
            .is_some_and(|contact| contact.receipt == receipt && contact.visual_hit() == Some(hit))
        {
            BrowserElementInteraction::Hovered
        } else {
            BrowserElementInteraction::Idle
        }
    }

    fn visual_signature_for(
        &self,
        receipt: PresentedReceiptIdentity,
    ) -> Option<(PresentedUiHit, BrowserElementInteraction)> {
        self.capture
            .filter(|contact| contact.receipt == receipt)
            .and_then(PresentedPointerContact::visual_hit)
            .map(|hit| (hit, BrowserElementInteraction::Pressed))
            .or_else(|| {
                self.hover
                    .filter(|contact| contact.receipt == receipt)
                    .and_then(PresentedPointerContact::visual_hit)
                    .map(|hit| (hit, BrowserElementInteraction::Hovered))
            })
    }

    fn apply_pointer(
        &mut self,
        pointer: &PointerEvent,
        receipt: BrowserFrameReceipt,
        region: Option<PresentedPointerRegion>,
    ) -> PresentedPointerTransition {
        self.apply_pointer_for(
            pointer,
            PresentedReceiptIdentity::from_receipt(receipt),
            region,
        )
    }

    fn apply_pointer_for(
        &mut self,
        pointer: &PointerEvent,
        receipt: PresentedReceiptIdentity,
        region: Option<PresentedPointerRegion>,
    ) -> PresentedPointerTransition {
        self.popup_pixel_scroll = None;
        let before = self.visual_signature_for(receipt);
        let contact = region.map(|region| PresentedPointerContact {
            receipt,
            pointer: pointer.pointer,
            seat: pointer.metadata.seat,
            device: pointer.metadata.device,
            kind: pointer.kind,
            surface: pointer.metadata.surface,
            region,
        });
        let mut activation = None;
        match pointer.phase {
            PointerPhase::Enter | PointerPhase::Move => {
                self.hover = contact;
                if self.capture.is_some_and(|capture| Some(capture) != contact) {
                    self.capture = None;
                }
            }
            PointerPhase::Down => {
                self.hover = contact;
                self.capture = if pointer.buttons == PRIMARY_POINTER_BUTTON
                    && contact
                        .and_then(PresentedPointerContact::exact_action)
                        .is_some()
                {
                    contact
                } else {
                    None
                };
            }
            PointerPhase::Up => {
                if pointer.buttons == 0
                    && let (Some(capture), Some(release)) = (self.capture, contact)
                    && capture == release
                {
                    activation = capture.exact_action();
                }
                self.capture = None;
                self.hover = contact;
            }
            PointerPhase::Cancel | PointerPhase::Leave => {
                self.clear_pointer_contacts();
            }
        }
        PresentedPointerTransition {
            activation,
            visual_changed: before != self.visual_signature_for(receipt),
        }
    }

    fn prepare_handoff(
        &self,
        receipt: BrowserFrameReceipt,
        authority: &PresentedUiAuthority,
        surface: WebRenderSurfaceSnapshot,
        page_unchanged: bool,
    ) -> PointerReceiptHandoff {
        self.prepare_handoff_for(
            PresentedReceiptIdentity::from_receipt(receipt),
            authority,
            surface.surface(),
            receipt.request().surface() == surface,
            page_unchanged,
        )
    }

    fn can_attempt_handoff(
        &self,
        receipt: BrowserFrameReceipt,
        surface: WebRenderSurfaceSnapshot,
        page_unchanged: bool,
    ) -> bool {
        let receipt_identity = PresentedReceiptIdentity::from_receipt(receipt);
        self.pending_visual_redraw.is_some_and(|token| {
            token.source_receipt == receipt_identity
                && token.signature == self.visual_signature_for(receipt_identity)
                && receipt.request().surface() == surface
                && page_unchanged
        })
    }

    fn prepare_handoff_for(
        &self,
        receipt: PresentedReceiptIdentity,
        authority: &PresentedUiAuthority,
        surface: wild_buzzard_platform::SurfaceId,
        exact_surface: bool,
        page_unchanged: bool,
    ) -> PointerReceiptHandoff {
        let Some(token) = self.pending_visual_redraw else {
            return PointerReceiptHandoff::default();
        };
        if token.source_receipt != receipt
            || token.signature != self.visual_signature_for(receipt)
            || !page_unchanged
            || !exact_surface
        {
            return PointerReceiptHandoff::default();
        }
        let retain = |contact: PresentedPointerContact| {
            (contact.receipt == receipt
                && contact.surface == surface
                && contact.is_current(authority))
            .then_some(contact)
        };
        let handoff = PointerReceiptHandoff {
            generation: Some(token.generation),
            hover: self.hover.and_then(retain),
            capture: self.capture.and_then(retain),
        };
        let retained_signature = handoff
            .capture
            .and_then(PresentedPointerContact::visual_hit)
            .map(|hit| (hit, BrowserElementInteraction::Pressed))
            .or_else(|| {
                handoff
                    .hover
                    .and_then(PresentedPointerContact::visual_hit)
                    .map(|hit| (hit, BrowserElementInteraction::Hovered))
            });
        if retained_signature == token.signature {
            handoff
        } else {
            PointerReceiptHandoff::default()
        }
    }

    fn commit_handoff(&mut self, receipt: BrowserFrameReceipt, handoff: &PointerReceiptHandoff) {
        self.commit_handoff_for(PresentedReceiptIdentity::from_receipt(receipt), handoff);
    }

    fn commit_handoff_for(
        &mut self,
        receipt: PresentedReceiptIdentity,
        handoff: &PointerReceiptHandoff,
    ) {
        if handoff.generation != self.pending_visual_redraw.map(|pending| pending.generation) {
            self.clear_pointer_contacts();
            return;
        }
        self.pending_visual_redraw = None;
        self.hover = handoff
            .hover
            .map(|contact| contact.with_receipt_identity(receipt));
        self.capture = handoff
            .capture
            .map(|contact| contact.with_receipt_identity(receipt));
    }

    fn mark_visual_redraw_pending(
        &mut self,
        receipt: BrowserFrameReceipt,
    ) -> Result<(), BrowserShellError> {
        self.mark_visual_redraw_pending_for(PresentedReceiptIdentity::from_receipt(receipt))
    }

    fn mark_visual_redraw_pending_for(
        &mut self,
        receipt: PresentedReceiptIdentity,
    ) -> Result<(), BrowserShellError> {
        let generation = self
            .visual_generation
            .checked_add(1)
            .ok_or_else(|| BrowserShellError::new("pointer visual generation exhausted"))?;
        self.visual_generation = generation;
        self.pending_visual_redraw = Some(PointerVisualRedrawToken {
            generation,
            source_receipt: receipt,
            signature: self.visual_signature_for(receipt),
        });
        Ok(())
    }

    fn hover_allows_popup_scroll(
        &self,
        receipt: BrowserFrameReceipt,
        surface: wild_buzzard_platform::SurfaceId,
        popup: &PresentedPopupAuthority,
    ) -> bool {
        let receipt = PresentedReceiptIdentity::from_receipt(receipt);
        self.hover
            .filter(|contact| contact.receipt == receipt && contact.surface == surface)
            .is_some_and(|contact| match contact.region {
                PresentedPointerRegion::Target {
                    hit: PresentedUiHit::PopupRow { .. },
                    ..
                } => true,
                PresentedPointerRegion::PopupSurface { kind, anchor } => {
                    kind == popup.kind && anchor == popup.anchor
                }
                PresentedPointerRegion::Target { .. } => false,
            })
    }

    fn normalized_popup_scroll(
        &mut self,
        scroll: &ScrollEvent,
        visible_capacity: usize,
        receipt: Option<PresentedReceiptIdentity>,
        kind: BrowserPrimaryPopupKind,
    ) -> Option<(PrimaryUiMoveDirection, u8)> {
        if scroll.phase == ScrollPhase::Cancel {
            self.popup_pixel_scroll = None;
            return None;
        }
        if matches!(scroll.phase, ScrollPhase::Begin | ScrollPhase::End)
            && !matches!(scroll.delta, ScrollDelta::Pixels(_))
        {
            self.popup_pixel_scroll = None;
            return None;
        }
        if scroll.phase == ScrollPhase::Begin {
            self.popup_pixel_scroll = None;
        }
        let outcome = match scroll.delta {
            ScrollDelta::Lines(vector) => {
                self.popup_pixel_scroll = None;
                bounded_popup_scroll_rows(vector.y, vector.y.abs())
            }
            ScrollDelta::Pages(vector) => {
                self.popup_pixel_scroll = None;
                let capacity = u32::try_from(visible_capacity.max(1)).ok()?;
                bounded_popup_scroll_rows(vector.y, vector.y.abs() * f64::from(capacity))
            }
            ScrollDelta::Pixels(vector) => {
                let exact_context = |candidate: &PopupPixelScrollAccumulator| {
                    candidate.receipt == receipt
                        && candidate.surface == scroll.metadata.surface
                        && candidate.seat == scroll.metadata.seat
                        && candidate.device == scroll.metadata.device
                        && candidate.kind == kind
                };
                if !self.popup_pixel_scroll.as_ref().is_some_and(exact_context) {
                    self.popup_pixel_scroll = Some(PopupPixelScrollAccumulator {
                        receipt,
                        surface: scroll.metadata.surface,
                        seat: scroll.metadata.seat,
                        device: scroll.metadata.device,
                        kind,
                        pixels: 0.0,
                    });
                }
                let accumulator = self
                    .popup_pixel_scroll
                    .as_mut()
                    .expect("pixel scroll context was installed above");
                accumulator.pixels += vector.y;
                let row_units = accumulator.pixels.abs() / PRIMARY_POPUP_SCROLL_ROW_PIXELS;
                if row_units < 1.0 {
                    None
                } else {
                    let direction = if accumulator.pixels < 0.0 {
                        PrimaryUiMoveDirection::Forward
                    } else {
                        PrimaryUiMoveDirection::Backward
                    };
                    let bounded = row_units.floor().min(f64::from(MAX_PRIMARY_UI_SCROLL_ROWS));
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    let rows = bounded as u8;
                    if row_units > f64::from(MAX_PRIMARY_UI_SCROLL_ROWS) {
                        accumulator.pixels = 0.0;
                    } else {
                        accumulator.pixels -= accumulator.pixels.signum()
                            * f64::from(rows)
                            * PRIMARY_POPUP_SCROLL_ROW_PIXELS;
                    }
                    Some((direction, rows))
                }
            }
        };
        if scroll.phase == ScrollPhase::End {
            self.popup_pixel_scroll = None;
        }
        outcome
    }

    fn promote_popup_scroll_to_canonical(
        &mut self,
        surface: wild_buzzard_platform::SurfaceId,
        kind: BrowserPrimaryPopupKind,
    ) {
        if let Some(accumulator) = self.popup_pixel_scroll.as_mut()
            && accumulator.surface == surface
            && accumulator.kind == kind
        {
            accumulator.receipt = None;
        }
    }

    fn input_device_removed(&mut self, device: wild_buzzard_platform::InputDeviceId) -> bool {
        let affected = self.hover.is_some_and(|contact| contact.device == device)
            || self.capture.is_some_and(|contact| contact.device == device)
            || self
                .popup_pixel_scroll
                .is_some_and(|scroll| scroll.device == device);
        if affected {
            self.clear();
        }
        affected
    }

    fn retain_canonical_popup_scroll(
        &mut self,
        surface: wild_buzzard_platform::SurfaceId,
        all_tabs_focused: bool,
    ) {
        if !self.popup_pixel_scroll.as_ref().is_some_and(|accumulator| {
            accumulator.receipt.is_none()
                && accumulator.surface == surface
                && accumulator.kind == BrowserPrimaryPopupKind::AllTabs
                && all_tabs_focused
        }) {
            self.popup_pixel_scroll = None;
        }
    }
}

struct ChromeCandidate {
    scene: BrowserChromeScene,
    authority: PresentedUiAuthority,
}

struct BrowserFrameCommitCandidate {
    active: BrowserTabId,
    page: BrowserPageSnapshot,
    authority: PresentedUiAuthority,
    pointer_handoff: PointerReceiptHandoff,
    request: BrowserFrameRequest,
    installing: bool,
    need_rerender: bool,
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
    AwaitApplicationPopup { initial: PhysicalSize },
    AwaitPopupDismissed { initial: PhysicalSize },
    AwaitResizeAway { initial: PhysicalSize },
    AwaitResizeBack { initial: PhysicalSize },
    AwaitFinalPage,
    AwaitFinalChromeAfterClose,
    Holding { until: Instant },
}

const fn smoke_composition_may_advance(stage: &SmokeStage, installing_page: bool) -> bool {
    installing_page
        || matches!(
            stage,
            SmokeStage::AwaitApplicationPopup { .. } | SmokeStage::AwaitPopupDismissed { .. }
        )
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
    #[cfg(feature = "webdriver")]
    automation: Option<automation::AutomationOwner>,
    text: Option<TextSystem>,
    browser_window: BrowserWindowId,
    navigation_mode: BrowserNavigationMode,
    script_execution: BrowserScriptExecution,
    initial_url: Option<Box<str>>,
    initial_surface: Option<WebRenderSurfaceSnapshot>,
    engine_viewport: Option<PhysicalSize>,
    surface: BrowserSurfaceState,
    presented: PresentedState,
    graphics_navigations: BTreeMap<NavigationId, BrowserNavigationIdentity>,
    next_graphics_navigation: Option<u64>,
    graphics_ui_elements: BTreeMap<PrimaryUiElementId, BrowserChromeElementIdentity>,
    next_graphics_ui_element: Option<u64>,
    presented_ui: PresentedUiAuthority,
    presented_pointer: PresentedPointerState,
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
        Self::new_with_script_execution(initial_url, smoke, polling, BrowserScriptExecution::Static)
    }

    fn new_with_script_execution(
        initial_url: Option<Box<str>>,
        smoke: Option<BrowserSmokeConfig>,
        polling: Arc<AtomicBool>,
        script_execution: BrowserScriptExecution,
    ) -> Self {
        let navigation_mode =
            if smoke.is_some() || !matches!(script_execution, BrowserScriptExecution::Static) {
                BrowserNavigationMode::NumericLoopback
            } else {
                BrowserNavigationMode::GeneralWeb
            };
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
            #[cfg(feature = "webdriver")]
            automation: None,
            text: None,
            browser_window: BrowserWindowId::new(1).expect("initial browser window is nonzero"),
            navigation_mode,
            script_execution,
            initial_url,
            initial_surface: None,
            engine_viewport: None,
            surface: BrowserSurfaceState::default(),
            presented: PresentedState::default(),
            graphics_navigations: BTreeMap::new(),
            next_graphics_navigation: Some(1),
            graphics_ui_elements: BTreeMap::new(),
            next_graphics_ui_element: Some(1),
            presented_ui: PresentedUiAuthority::default(),
            presented_pointer: PresentedPointerState::default(),
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

    fn configure_script_execution(&mut self, script_execution: BrowserScriptExecution) {
        self.script_execution = script_execution;
        if !matches!(script_execution, BrowserScriptExecution::Static) {
            self.navigation_mode = BrowserNavigationMode::NumericLoopback;
        }
    }

    fn fail(&mut self, detail: impl fmt::Display, control: &mut LinuxWindowControl<'_>) {
        if self.failure.is_none() {
            self.failure = Some(detail.to_string());
        }
        self.invalidate_hit_authority();
        self.polling.store(false, Ordering::Release);
        control.request_exit();
    }

    fn invalidate_hit_authority(&mut self) {
        self.presented.receipt = None;
        self.presented_ui.clear();
        self.presented_pointer.clear();
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
        let port = match (self.navigation_mode, self.script_execution) {
            (BrowserNavigationMode::NumericLoopback, BrowserScriptExecution::Static) => {
                NavigationEnginePort::spawn_for_presentation(page_config, EngineLimits::default())
            }
            #[cfg(feature = "contained_inline_classic")]
            (
                BrowserNavigationMode::NumericLoopback,
                BrowserScriptExecution::ContainedInlineClassic,
            ) => NavigationEnginePort::spawn_contained_inline_classic_for_presentation(
                page_config,
                EngineLimits::default(),
            ),
            (BrowserNavigationMode::GeneralWeb, BrowserScriptExecution::Static) => {
                NavigationEnginePort::spawn_general_web_for_presentation(
                    page_config,
                    GeneralWebConfig::default(),
                    TrustStore::default(),
                    EngineLimits::default(),
                )
            }
            #[cfg(feature = "contained_inline_classic")]
            (BrowserNavigationMode::GeneralWeb, BrowserScriptExecution::ContainedInlineClassic) => {
                return Err(BrowserShellError::new(
                    "contained script execution cannot acquire general-web authority",
                ));
            }
        }
        .map_err(BrowserShellError::new)?;
        let mut session = BrowserSession::new_with_navigation_mode(
            port,
            shell_session_limits(),
            self.navigation_mode,
        )
        .map_err(BrowserShellError::new)?;
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
        #[cfg(feature = "webdriver")]
        self.drain_automation(control)?;
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

    #[cfg(feature = "webdriver")]
    fn drain_automation(
        &mut self,
        control: &LinuxWindowControl<'_>,
    ) -> Result<(), BrowserShellError> {
        let browser_window = self.browser_window;
        let outcome = {
            let Some(automation) = self.automation.as_mut() else {
                return Ok(());
            };
            let session = self
                .session
                .as_mut()
                .ok_or_else(|| BrowserShellError::new("browser session is not initialized"))?;
            automation.drain(session, browser_window)
        };
        if outcome.navigation_started || outcome.screenshot_requested || outcome.more_may_remain {
            self.polling.store(true, Ordering::Release);
        }
        if outcome.navigation_started {
            self.invalidate_hit_authority();
        }
        if outcome.navigation_started || outcome.screenshot_requested {
            control.request_redraw().map_err(BrowserShellError::new)?;
        }
        Ok(())
    }

    #[cfg(feature = "webdriver")]
    fn observe_automation_session(&mut self) -> Result<(), BrowserShellError> {
        let Some(automation) = self.automation.as_mut() else {
            return Ok(());
        };
        let session = self
            .session
            .as_mut()
            .ok_or_else(|| BrowserShellError::new("browser session is not initialized"))?;
        automation.observe_session(session);
        Ok(())
    }

    #[cfg(feature = "webdriver")]
    fn automation_has_pending_work(&self) -> bool {
        self.automation.as_ref().is_some_and(|automation| {
            automation.has_pending_navigation() || automation.has_pending_screenshot()
        })
    }

    #[cfg(feature = "webdriver")]
    fn automation_presentation_identity(
        &self,
        active: BrowserTabId,
    ) -> Result<Option<PresentationCommitIdentity>, BrowserShellError> {
        let session = self
            .session
            .as_ref()
            .ok_or_else(|| BrowserShellError::new("browser session is not initialized"))?;
        session
            .presentation_candidate_labels(active)
            .map(|candidate| {
                candidate.map(|(navigation, document, lease, scene_revision)| {
                    PresentationCommitIdentity {
                        tab: active,
                        navigation,
                        document,
                        lease,
                        scene_revision,
                    }
                })
            })
            .map_err(BrowserShellError::new)
    }

    #[cfg(feature = "webdriver")]
    fn receipt_navigation(
        &self,
        receipt: BrowserFrameReceipt,
    ) -> Result<Option<NavigationId>, BrowserShellError> {
        let BrowserPageSnapshot::Scene(page) = receipt.request().page() else {
            return Ok(None);
        };
        let mut matches = self
            .graphics_navigations
            .iter()
            .filter(|(_, identity)| **identity == page.navigation())
            .map(|(navigation, _)| *navigation);
        let navigation = matches.next().ok_or_else(|| {
            BrowserShellError::new(
                "compositor receipt names an unregistered browser navigation identity",
            )
        })?;
        if matches.next().is_some() {
            return Err(BrowserShellError::new(
                "compositor receipt navigation identity is not unique",
            ));
        }
        Ok(Some(navigation))
    }

    #[cfg(feature = "webdriver")]
    fn observe_automation_composition(
        &mut self,
        identity: PresentationCommitIdentity,
        receipt: BrowserFrameReceipt,
    ) -> Result<(), BrowserShellError> {
        let navigation = self.receipt_navigation(receipt)?;
        if navigation != Some(identity.navigation) {
            return Err(BrowserShellError::new(
                "automation commit receipt names a foreign navigation",
            ));
        }
        let BrowserPageSnapshot::Scene(page) = receipt.request().page() else {
            return Err(BrowserShellError::new(
                "automation commit receipt contains no exact page scene",
            ));
        };
        let document = page.document_version();
        if page.revision().get() != identity.scene_revision
            || document.document_id().get() != identity.document.document()
            || document.revision() != identity.document.revision()
        {
            return Err(BrowserShellError::new(
                "automation commit receipt names foreign document labels",
            ));
        }
        let Some(automation) = self.automation.as_mut() else {
            return Ok(());
        };
        let session = self
            .session
            .as_ref()
            .ok_or_else(|| BrowserShellError::new("browser session is not initialized"))?;
        automation.observe_composition(session, identity);
        Ok(())
    }

    fn active_tab(&self) -> Result<BrowserTabId, BrowserShellError> {
        self.session
            .as_ref()
            .ok_or_else(|| BrowserShellError::new("browser session is not initialized"))?
            .window_snapshot(self.browser_window)
            .map(|window| window.active)
            .map_err(BrowserShellError::new)
    }

    fn canonical_all_tabs_scroll_focused(&self) -> Result<bool, BrowserShellError> {
        let snapshot = self
            .session
            .as_ref()
            .ok_or_else(|| BrowserShellError::new("browser session is not initialized"))?
            .primary_ui_snapshot(self.browser_window)
            .map_err(BrowserShellError::new)?;
        Ok(snapshot
            .panel
            .as_ref()
            .is_some_and(|panel| panel.panel == PrimaryUiPanel::AllTabs)
            && matches!(
                snapshot.focus,
                PrimaryUiFocus::PanelItem(_) | PrimaryUiFocus::Control(PrimaryUiControl::AllTabs)
            ))
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

    fn reconcile_graphics_ui_elements(&mut self, snapshot: &PrimaryUiSnapshot) {
        self.graphics_ui_elements
            .retain(|element, _| primary_ui_element_is_live(snapshot, *element));
    }

    fn graphics_ui_element(
        &mut self,
        element: PrimaryUiElementId,
    ) -> Result<BrowserChromeElementIdentity, BrowserShellError> {
        if let Some(identity) = self.graphics_ui_elements.get(&element).copied() {
            return Ok(identity);
        }
        if self.graphics_ui_elements.len() >= MAX_GRAPHICS_UI_ELEMENTS {
            return Err(BrowserShellError::new(
                "graphics primary-element registry reached its hard live limit",
            ));
        }
        let raw = self
            .next_graphics_ui_element
            .ok_or_else(|| BrowserShellError::new("graphics primary-element identity exhausted"))?;
        let identity = BrowserChromeElementIdentity::new(raw)
            .ok_or_else(|| BrowserShellError::new("graphics primary-element identity was zero"))?;
        self.next_graphics_ui_element = raw.checked_add(1);
        self.graphics_ui_elements.insert(element, identity);
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

    #[allow(clippy::too_many_lines)]
    fn build_primary_projection(
        &mut self,
        snapshot: &PrimaryUiSnapshot,
        tab_identities: &BTreeMap<BrowserTabId, BrowserTabIdentity>,
    ) -> Result<
        (
            BrowserPrimaryChromeState,
            BrowserChromeFocus,
            PresentedUiAuthority,
        ),
        BrowserShellError,
    > {
        self.reconcile_graphics_ui_elements(snapshot);
        let mut authority = PresentedUiAuthority::default();
        let page_binding = snapshot
            .bind_action(PrimaryUiElementId::Page)
            .ok_or_else(|| BrowserShellError::new("primary page lacks exact focus authority"))?;
        authority.push_action(PresentedUiHit::Page, page_binding)?;
        for tab in &snapshot.tabs {
            let identity = *tab_identities
                .get(&tab.tab)
                .ok_or_else(|| BrowserShellError::new("primary tab lacks graphics identity"))?;
            let tab_binding = snapshot
                .bind_action(PrimaryUiElementId::Tab(tab.tab))
                .ok_or_else(|| BrowserShellError::new("presented tab lacks exact action"))?;
            authority.push_action(PresentedUiHit::Tab(identity), tab_binding)?;
            let close_hit = PresentedUiHit::TabClose(identity);
            if let Some(binding) = snapshot.bind_action(PrimaryUiElementId::TabClose(tab.tab)) {
                authority.push_action(close_hit, binding)?;
            } else if tab.close_availability.is_enabled() {
                return Err(BrowserShellError::new(
                    "enabled presented tab close lacks exact action",
                ));
            } else {
                authority.push_disabled(close_hit)?;
            }
        }

        if snapshot.controls.len() != PrimaryUiControl::ALL.len() {
            return Err(BrowserShellError::new(
                "primary snapshot omitted a fixed control",
            ));
        }
        let mut controls = Vec::new();
        controls
            .try_reserve_exact(PrimaryUiControl::ALL.len())
            .map_err(BrowserShellError::new)?;
        let mut control_elements = BTreeMap::new();
        for (expected, control) in PrimaryUiControl::ALL.iter().zip(snapshot.controls.iter()) {
            if control.control != *expected {
                return Err(BrowserShellError::new(
                    "primary snapshot fixed-control order drifted",
                ));
            }
            let semantic = PrimaryUiElementId::Control(control.control);
            let element = self.graphics_ui_element(semantic)?;
            control_elements.insert(control.control, element);
            let kind = graphics_control_kind(control.control);
            let label = self.shape(
                bounded_utf8_prefix(&control.name, MAX_PRIMARY_CONTROL_LABEL_BYTES),
                PRIMARY_CONTROL_FONT_SIZE_PX,
            )?;
            let hit = if control.control == PrimaryUiControl::AddressBar {
                PresentedUiHit::AddressBar
            } else {
                PresentedUiHit::PrimaryControl { element, kind }
            };
            let interaction = if control.visible {
                self.presented_pointer
                    .visual_interaction(hit, self.presented.receipt)
            } else {
                BrowserElementInteraction::Idle
            };
            controls.push(
                BrowserPrimaryControl::new(
                    element,
                    kind,
                    label,
                    graphics_availability(control.availability),
                )
                .with_interaction(interaction),
            );
            if control.visible {
                if let Some(binding) = snapshot.bind_action(semantic) {
                    authority.push_action(hit, binding)?;
                } else if control.availability.is_enabled() {
                    return Err(BrowserShellError::new(
                        "enabled presented primary control lacks exact action",
                    ));
                } else {
                    authority.push_disabled(hit)?;
                }
            }
        }

        let reload_stop = snapshot
            .controls
            .iter()
            .find(|control| control.control == PrimaryUiControl::ReloadStop)
            .and_then(|control| control.reload_stop_mode)
            .ok_or_else(|| BrowserShellError::new("primary ReloadStop has no sole mode"))?;
        let site_identity = snapshot
            .controls
            .iter()
            .find(|control| control.control == PrimaryUiControl::SiteIdentity)
            .and_then(|control| control.site_identity)
            .ok_or_else(|| BrowserShellError::new("primary site identity has no classification"))?;

        let mut focused_popup_element = None;
        let popup = snapshot
            .panel
            .as_ref()
            .map(|panel| {
                let kind = graphics_popup_kind(panel.panel);
                let anchor = *control_elements
                    .get(&panel.anchor)
                    .ok_or_else(|| BrowserShellError::new("primary popup lost its anchor"))?;
                let mut rows = Vec::new();
                rows.try_reserve_exact(panel.items.len())
                    .map_err(BrowserShellError::new)?;
                let visible_end = panel
                    .scroll_offset
                    .saturating_add(panel.visible_capacity)
                    .min(panel.items.len());
                for (item_index, item) in panel.items.iter().enumerate() {
                    if item.expanded {
                        return Err(BrowserShellError::new(
                            "primary popup row claimed an unimplemented child view",
                        ));
                    }
                    let (element, row_kind, row) = match (panel.panel, item.id, item.action) {
                        (
                            PrimaryUiPanel::SiteIdentity,
                            PrimaryUiPanelItemId::IdentitySummary,
                            PrimaryUiPanelItemAction::None,
                        ) => {
                            let element =
                                self.graphics_ui_element(PrimaryUiElementId::PanelItem(item.id))?;
                            let action = BrowserPrimaryActionKind::SiteInformation;
                            (
                                element,
                                BrowserPrimaryPopupRowKind::Action(action),
                                BrowserPrimaryPopupRow::action(
                                    element,
                                    action,
                                    self.shape(
                                        bounded_utf8_prefix(
                                            &item.name,
                                            MAX_PRIMARY_ACTION_LABEL_BYTES,
                                        ),
                                        PRIMARY_POPUP_FONT_SIZE_PX,
                                    )?,
                                    graphics_availability(item.availability),
                                ),
                            )
                        }
                        (
                            PrimaryUiPanel::AllTabs,
                            PrimaryUiPanelItemId::AllTabsTab(tab),
                            PrimaryUiPanelItemAction::ActivateTab(action_tab),
                        ) if tab == action_tab => {
                            let element =
                                self.graphics_ui_element(PrimaryUiElementId::PanelItem(item.id))?;
                            let tab_identity = *tab_identities.get(&tab).ok_or_else(|| {
                                BrowserShellError::new("all-tabs row names a foreign tab")
                            })?;
                            (
                                element,
                                BrowserPrimaryPopupRowKind::Tab(tab_identity),
                                BrowserPrimaryPopupRow::tab(
                                    element,
                                    tab_identity,
                                    graphics_availability(item.availability),
                                ),
                            )
                        }
                        (
                            PrimaryUiPanel::Overflow,
                            PrimaryUiPanelItemId::OverflowControl(control),
                            PrimaryUiPanelItemAction::InvokeControl(action_control),
                        ) if control == action_control => {
                            let element = *control_elements.get(&control).ok_or_else(|| {
                                BrowserShellError::new("overflow row lost control identity")
                            })?;
                            let control = graphics_control_kind(control);
                            (
                                element,
                                BrowserPrimaryPopupRowKind::Control(control),
                                BrowserPrimaryPopupRow::relocated_control(
                                    element,
                                    control,
                                    graphics_availability(item.availability),
                                ),
                            )
                        }
                        (PrimaryUiPanel::ApplicationMenu, id, action) => {
                            let (expected_action, expected_control) = match id {
                                PrimaryUiPanelItemId::ApplicationNewTab => (
                                    BrowserPrimaryActionKind::NewTab,
                                    PrimaryUiPanelItemAction::InvokeControl(
                                        PrimaryUiControl::NewTab,
                                    ),
                                ),
                                PrimaryUiPanelItemId::ApplicationCloseTab => (
                                    BrowserPrimaryActionKind::CloseTab,
                                    PrimaryUiPanelItemAction::CloseActiveTab,
                                ),
                                PrimaryUiPanelItemId::ApplicationBack => (
                                    BrowserPrimaryActionKind::Back,
                                    PrimaryUiPanelItemAction::InvokeControl(PrimaryUiControl::Back),
                                ),
                                PrimaryUiPanelItemId::ApplicationForward => (
                                    BrowserPrimaryActionKind::Forward,
                                    PrimaryUiPanelItemAction::InvokeControl(
                                        PrimaryUiControl::Forward,
                                    ),
                                ),
                                PrimaryUiPanelItemId::ApplicationReloadStop => (
                                    BrowserPrimaryActionKind::ReloadStop,
                                    PrimaryUiPanelItemAction::InvokeControl(
                                        PrimaryUiControl::ReloadStop,
                                    ),
                                ),
                                _ => {
                                    return Err(BrowserShellError::new(
                                        "application popup contains a foreign row",
                                    ));
                                }
                            };
                            if action != expected_control {
                                return Err(BrowserShellError::new(
                                    "application popup row/action mapping drifted",
                                ));
                            }
                            let element =
                                self.graphics_ui_element(PrimaryUiElementId::PanelItem(item.id))?;
                            (
                                element,
                                BrowserPrimaryPopupRowKind::Action(expected_action),
                                BrowserPrimaryPopupRow::action(
                                    element,
                                    expected_action,
                                    self.shape(
                                        bounded_utf8_prefix(
                                            &item.name,
                                            MAX_PRIMARY_ACTION_LABEL_BYTES,
                                        ),
                                        PRIMARY_POPUP_FONT_SIZE_PX,
                                    )?,
                                    graphics_availability(item.availability),
                                ),
                            )
                        }
                        _ => {
                            return Err(BrowserShellError::new(
                                "primary popup kind, identity, and action disagree",
                            ));
                        }
                    };
                    let row_hit = PresentedUiHit::PopupRow {
                        element,
                        kind: row_kind,
                    };
                    let row = row
                        .with_interaction(
                            self.presented_pointer
                                .visual_interaction(row_hit, self.presented.receipt),
                        )
                        .with_selection(if item.selected {
                            BrowserElementSelection::Selected
                        } else {
                            BrowserElementSelection::NotSelected
                        })
                        .with_expansion(BrowserElementExpansion::Leaf);
                    if snapshot.focus == PrimaryUiFocus::PanelItem(item.id) {
                        focused_popup_element = Some(element);
                    }
                    if item_index >= panel.scroll_offset && item_index < visible_end {
                        if let Some(binding) =
                            snapshot.bind_action(PrimaryUiElementId::PanelItem(item.id))
                        {
                            authority.push_action(row_hit, binding)?;
                        } else if item.availability.is_enabled() {
                            return Err(BrowserShellError::new(
                                "enabled visible primary popup row lacks exact action",
                            ));
                        } else {
                            authority.push_disabled(row_hit)?;
                        }
                    }
                    rows.push(row);
                }
                let dismissal = snapshot.bind_panel_dismissal().ok_or_else(|| {
                    BrowserShellError::new("open primary popup lacks exact dismissal authority")
                })?;
                authority.push_action(PresentedUiHit::PopupDismiss { kind, anchor }, dismissal)?;
                authority.install_popup(kind, anchor)?;
                Ok(
                    BrowserPrimaryPopup::new(kind, anchor, rows.into_boxed_slice())
                        .with_first_visible_row(panel.scroll_offset),
                )
            })
            .transpose()?;

        let focus = match snapshot.focus {
            PrimaryUiFocus::Page => BrowserChromeFocus::Page,
            PrimaryUiFocus::Tab(tab) => BrowserChromeFocus::Tab(
                *tab_identities
                    .get(&tab)
                    .ok_or_else(|| BrowserShellError::new("focused primary tab is foreign"))?,
            ),
            PrimaryUiFocus::Control(PrimaryUiControl::AddressBar) => {
                return Err(BrowserShellError::new(
                    "primary focus used ambiguous address-as-control identity",
                ));
            }
            PrimaryUiFocus::Control(control) => BrowserChromeFocus::PrimaryControl(
                *control_elements
                    .get(&control)
                    .ok_or_else(|| BrowserShellError::new("focused primary control is foreign"))?,
            ),
            PrimaryUiFocus::AddressBar => BrowserChromeFocus::AddressBar,
            PrimaryUiFocus::PanelItem(_) => {
                BrowserChromeFocus::PopupRow(focused_popup_element.ok_or_else(|| {
                    BrowserShellError::new("focused popup row is absent from its exact inventory")
                })?)
            }
        };
        let primary = BrowserPrimaryChromeState::new(
            graphics_direction(snapshot.direction),
            controls.into_boxed_slice(),
            graphics_reload_stop(reload_stop),
            graphics_site_identity(site_identity),
        )
        .with_popup(popup);
        Ok((primary, focus, authority))
    }

    #[allow(clippy::too_many_lines)]
    fn build_chrome(
        &mut self,
        surface: WebRenderSurfaceSnapshot,
        revision: BrowserChromeRevision,
    ) -> Result<ChromeCandidate, BrowserShellError> {
        let window = self
            .session
            .as_ref()
            .ok_or_else(|| BrowserShellError::new("browser session is not initialized"))?
            .window_snapshot(self.browser_window)
            .map_err(BrowserShellError::new)?;
        let direction = self
            .session
            .as_ref()
            .expect("session checked above")
            .primary_ui_direction(self.browser_window)
            .map_err(BrowserShellError::new)?;
        let preview = BrowserPrimaryLayoutPreview::for_surface(
            surface,
            graphics_direction(direction),
            window.tabs.len(),
        )
        .map_err(BrowserShellError::new)?;
        let layout = primary_layout_from_preview(&preview)?;
        let browser_window = self.browser_window;
        self.session_mut()?
            .set_primary_ui_layout(browser_window, layout)
            .map_err(BrowserShellError::new)?;
        let primary_snapshot = self
            .session
            .as_ref()
            .expect("session checked above")
            .primary_ui_snapshot(browser_window)
            .map_err(BrowserShellError::new)?;
        if primary_snapshot.tabs.len() != window.tabs.len()
            || !primary_snapshot
                .tabs
                .iter()
                .zip(window.tabs.iter())
                .all(|(primary, tab)| primary.tab == *tab)
        {
            return Err(BrowserShellError::new(
                "primary tab inventory drifted during one-pass projection",
            ));
        }
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
        let mut tab_identities = BTreeMap::new();
        for (snapshot, primary_tab) in snapshots.iter().zip(primary_snapshot.tabs.iter()) {
            let title = if snapshot.address.is_empty() {
                "New Tab"
            } else {
                bounded_utf8_prefix(&snapshot.address, MAX_TAB_LABEL_BYTES)
            };
            let shaped = self.shape(title, TAB_FONT_SIZE_PX)?;
            let identity = BrowserTabIdentity::new(snapshot.id.get())
                .ok_or_else(|| BrowserShellError::new("browser tab identity was zero"))?;
            tab_identities.insert(snapshot.id, identity);
            tabs.push(
                BrowserChromeTab::new(identity, shaped)
                    .with_loading(snapshot.loading)
                    .with_interaction(
                        self.presented_pointer.visual_interaction(
                            PresentedUiHit::Tab(identity),
                            self.presented.receipt,
                        ),
                    )
                    .with_close_state(
                        graphics_availability(primary_tab.close_availability),
                        self.presented_pointer.visual_interaction(
                            PresentedUiHit::TabClose(identity),
                            self.presented.receipt,
                        ),
                    ),
            );
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
        let (primary, focus, authority) =
            self.build_primary_projection(&primary_snapshot, &tab_identities)?;
        let state =
            BrowserChromeState::new(tabs.into_boxed_slice(), Some(active_identity), address)
                .with_address_selection(BrowserAddressSelection::new(
                    active.address_selection.anchor().min(address_text.len()),
                    active.address_selection.focus().min(address_text.len()),
                ))
                .with_status(status)
                .with_focus(focus)
                .with_primary_chrome(Some(primary));
        let scene =
            BrowserChromeScene::new(revision, surface, state).map_err(BrowserShellError::new)?;
        let resolved = scene.primary_layout().ok_or_else(|| {
            BrowserShellError::new("primary layout was not frozen into the scene")
        })?;
        if resolved.preview() != &preview {
            return Err(BrowserShellError::new(
                "frozen primary layout differs from the pure pre-scene resolution",
            ));
        }
        Ok(ChromeCandidate { scene, authority })
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
        match outcome {
            LinuxEventOutcome::Command(command)
            | LinuxEventOutcome::PrimaryUi(PrimaryUiActionOutcome::Command(command)) => {
                self.retire_rerender_authority_after_command(*command);
            }
            _ => {}
        }
    }

    fn prepare_page_update(
        &mut self,
        active: BrowserTabId,
        expected: Option<PresentationCommitIdentity>,
    ) -> Result<(BrowserPageUpdate, BrowserPageSnapshot, bool, bool), BrowserShellError> {
        if expected.is_some_and(|expected| expected.tab != active) {
            return Err(BrowserShellError::new(
                "presentation commit permit names a foreign active tab",
            ));
        }
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
                let scene = match expected {
                    Some(expected) => self.session_mut()?.take_exact_presentation_scene(
                        active,
                        (
                            expected.navigation,
                            expected.document,
                            expected.lease,
                            expected.scene_revision,
                        ),
                        browser_navigation,
                    ),
                    None => self.session_mut()?.take_presentation_scene(
                        active,
                        navigation,
                        document,
                        descriptor.scene_revision(),
                        browser_navigation,
                    ),
                }
                .map_err(BrowserShellError::new)?
                .ok_or_else(|| {
                    BrowserShellError::new(
                        "validated presentation candidate disappeared before consumption",
                    )
                })?;
                if let Some(expected) = expected {
                    let identity = scene.identity();
                    let scene_document = identity.document_version();
                    if self.graphics_navigations.get(&expected.navigation)
                        != Some(&identity.navigation())
                        || identity.revision().get() != expected.scene_revision
                        || scene_document.document_id().get() != expected.document.document()
                        || scene_document.revision() != expected.document.revision()
                    {
                        return Err(BrowserShellError::new(
                            "presentation commit permit disagrees with the consumed page scene",
                        ));
                    }
                }
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
        let active = self.active_tab()?;
        #[cfg(feature = "webdriver")]
        if self.automation.is_some()
            && let Some(identity) = self.automation_presentation_identity(active)?
        {
            let admission = self
                .automation
                .as_ref()
                .expect("automation presence checked above")
                .presentation_admission(
                    self.session
                        .as_ref()
                        .expect("active tab proved an initialized session"),
                    identity,
                );
            match admission {
                automation::AutomationPresentationAdmission::Authorized(permit) => {
                    let Some(transaction) = permit.commit(identity, |marker| {
                        self.draw_transaction(
                            control,
                            surface,
                            active,
                            Some(identity),
                            Some(marker),
                        )
                    }) else {
                        self.observe_automation_session()?;
                        return Ok(DrawOutcome::Deferred);
                    };
                    let (outcome, receipt) = transaction?;
                    if let Some(receipt) = receipt {
                        self.observe_automation_composition(identity, receipt)?;
                    }
                    return Ok(outcome);
                }
                automation::AutomationPresentationAdmission::Rejected => {
                    self.observe_automation_session()?;
                    return Ok(DrawOutcome::Deferred);
                }
                automation::AutomationPresentationAdmission::Unrestricted => {}
            }
        }
        let (outcome, _) = self.draw_transaction(control, surface, active, None, None)?;
        Ok(outcome)
    }

    fn draw_transaction(
        &mut self,
        control: &mut LinuxWindowControl<'_>,
        surface: WebRenderSurfaceSnapshot,
        active: BrowserTabId,
        expected: Option<PresentationCommitIdentity>,
        mut commit_marker: Option<&mut NativePresentationCommitMarker<'_>>,
    ) -> Result<(DrawOutcome, Option<BrowserFrameReceipt>), BrowserShellError> {
        if commit_marker
            .as_deref_mut()
            .is_some_and(|marker| !marker.begin_submission())
        {
            return Ok((DrawOutcome::Deferred, None));
        }
        if !self.can_draw_surface(surface.size()) {
            self.surface.record_transition(false, surface.size());
            return Ok((DrawOutcome::Deferred, None));
        }
        if expected.is_some() && !self.surface.viewport_compatible() {
            return Ok((DrawOutcome::Deferred, None));
        }
        if expected.is_some_and(|identity| {
            self.presented
                .last_page_revision
                .is_some_and(|revision| revision >= identity.scene_revision)
        }) {
            self.request_rerender_if_possible(active)?;
            return Ok((DrawOutcome::Deferred, None));
        }
        let (chrome_revision, epoch, sequence) = self.reserve_frame_labels()?;
        let (page_update, page, installing, need_rerender) =
            self.prepare_page_update(active, expected)?;
        if expected.is_some() && !installing {
            return Err(BrowserShellError::new(
                "presentation commit permit did not consume its exact page scene",
            ));
        }
        let presented_receipt = self.presented.receipt;
        let attempted_pointer_handoff = presented_receipt.is_some_and(|receipt| {
            self.presented_pointer.can_attempt_handoff(
                receipt,
                surface,
                page == self.presented.page,
            )
        });
        if !attempted_pointer_handoff {
            self.presented_pointer.clear_pointer_contacts();
        }
        let mut candidate = self.build_chrome(surface, chrome_revision)?;
        let canonical_all_tabs_scroll_focused = self.canonical_all_tabs_scroll_focused()?;
        self.presented_pointer
            .retain_canonical_popup_scroll(surface.surface(), canonical_all_tabs_scroll_focused);
        let mut pointer_handoff =
            presented_receipt.map_or_else(PointerReceiptHandoff::default, |presented_receipt| {
                self.presented_pointer.prepare_handoff(
                    presented_receipt,
                    &candidate.authority,
                    surface,
                    page == self.presented.page,
                )
            });
        if attempted_pointer_handoff && pointer_handoff.generation.is_none() {
            self.presented_pointer.clear_pointer_contacts();
            candidate = self.build_chrome(surface, chrome_revision)?;
            pointer_handoff = PointerReceiptHandoff::default();
        }
        let ChromeCandidate {
            scene: chrome,
            authority,
        } = candidate;
        let request = BrowserFrameRequest::new(surface, page, chrome_revision, epoch, sequence);
        let commit = BrowserFrameCommitCandidate {
            active,
            page,
            authority,
            pointer_handoff,
            request,
            installing,
            need_rerender,
        };
        #[cfg(feature = "webdriver")]
        let screenshot = self
            .automation
            .as_mut()
            .and_then(|automation| automation.claim_screenshot_capture(request));
        #[cfg(feature = "webdriver")]
        if let Some(screenshot) = screenshot {
            return match control.submit_browser_frame_with_capture(
                page_update,
                Some(chrome),
                request,
            ) {
                Ok(capture) => {
                    let receipt = capture.receipt();
                    match self.accept_submitted_frame(
                        commit,
                        receipt,
                        control,
                        commit_marker,
                    ) {
                        Ok(outcome) => {
                            let automation = self.automation.as_mut().ok_or_else(|| {
                                BrowserShellError::new(
                                    "screenshot owner disappeared after exact frame capture",
                                )
                            })?;
                            let _ = automation.complete_screenshot_capture(screenshot, capture);
                            Ok(outcome)
                        }
                        Err(error) => {
                            if let Some(automation) = self.automation.as_mut() {
                                let _ = automation.fail_screenshot_capture(screenshot);
                            }
                            Err(error)
                        }
                    }
                }
                Err(error) => {
                    if let Some(automation) = self.automation.as_mut() {
                        let _ = automation.fail_screenshot_capture(screenshot);
                    }
                    self.handle_browser_submission_error(installing, error, control)
                }
            };
        }
        match control.submit_browser_frame(page_update, Some(chrome), request) {
            Ok(receipt) => self.accept_submitted_frame(commit, receipt, control, commit_marker),
            Err(error) => self.handle_browser_submission_error(installing, error, control),
        }
    }

    fn handle_browser_submission_error(
        &mut self,
        installing: bool,
        error: ControlError,
        control: &LinuxWindowControl<'_>,
    ) -> Result<(DrawOutcome, Option<BrowserFrameReceipt>), BrowserShellError> {
        if !retry_browser_frame_after(error) {
            return Err(BrowserShellError::new(format_args!(
                "terminal or unclassified browser composition failure: {error}"
            )));
        }
        self.record_preaccept_rejection()?;
        if installing {
            let active = self.active_tab()?;
            self.rerender_pending.remove(&active);
            self.request_rerender_if_possible(active)?;
        } else {
            control.request_redraw().map_err(BrowserShellError::new)?;
        }
        eprintln!("retry-safe preaccept browser composition rejection: {error}");
        Ok((DrawOutcome::PreacceptRejected, None))
    }

    fn accept_submitted_frame(
        &mut self,
        commit: BrowserFrameCommitCandidate,
        receipt: BrowserFrameReceipt,
        control: &mut LinuxWindowControl<'_>,
        commit_marker: Option<&mut NativePresentationCommitMarker<'_>>,
    ) -> Result<(DrawOutcome, Option<BrowserFrameReceipt>), BrowserShellError> {
        let Some(marker) = commit_marker else {
            return self.commit_submitted_frame(commit, receipt, control);
        };
        if !marker.mark_native_committed() {
            return Err(BrowserShellError::new(
                "automation native commit marker rejected a successful submission",
            ));
        }
        marker
            .commit_shell_state(|| self.commit_submitted_frame(commit, receipt, control))
            .ok_or_else(|| {
                BrowserShellError::new(
                    "automation native commit did not authorize shell state acceptance",
                )
            })?
    }

    fn commit_submitted_frame(
        &mut self,
        commit: BrowserFrameCommitCandidate,
        receipt: BrowserFrameReceipt,
        control: &mut LinuxWindowControl<'_>,
    ) -> Result<(DrawOutcome, Option<BrowserFrameReceipt>), BrowserShellError> {
        if receipt.request() != commit.request
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
        self.presented.active_tab = Some(commit.active);
        self.presented.page = commit.page;
        self.presented.receipt = Some(receipt);
        self.presented_ui = commit.authority;
        self.presented_pointer
            .commit_handoff(receipt, &commit.pointer_handoff);
        if let BrowserPageSnapshot::Scene(identity) = commit.page {
            self.presented.last_page_revision = Some(identity.revision().get());
            self.rerender_pending.remove(&commit.active);
            self.rerender_suppressed.remove(&commit.active);
        }
        self.surface.mark_presented();
        if commit.need_rerender {
            self.request_rerender_if_possible(commit.active)?;
        }
        self.advance_smoke_after_submission(commit.active, commit.installing, receipt, control)?;
        Ok((DrawOutcome::Submitted, Some(receipt)))
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
        #[cfg(feature = "webdriver")]
        self.observe_automation_session()?;
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
        #[cfg(feature = "webdriver")]
        let automation_pending = self.automation_has_pending_work();
        #[cfg(not(feature = "webdriver"))]
        let automation_pending = false;
        self.polling.store(
            more || any_loading
                || closing_contexts != 0
                || !self.rerender_pending.is_empty()
                || automation_pending
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

    fn presented_pointer_lookup(
        &self,
        pointer: &PointerEvent,
        control: &mut LinuxWindowControl<'_>,
    ) -> Result<PresentedPointerLookup, BrowserShellError> {
        let Some(authoritative_receipt) = self.presented.receipt else {
            return Ok(PresentedPointerLookup::InvalidAuthority);
        };
        let surface = control
            .browser_surface_snapshot()
            .map_err(BrowserShellError::new)?;
        let scale = surface.descriptor().scale.get();
        let x = (pointer.position.x * scale).round();
        let y = (pointer.position.y * scale).round();
        if x < f64::from(i32::MIN)
            || x > f64::from(i32::MAX)
            || y < f64::from(i32::MIN)
            || y > f64::from(i32::MAX)
        {
            return Ok(PresentedPointerLookup::Other);
        }
        #[allow(clippy::cast_possible_truncation)]
        let point = PhysicalPoint {
            x: x as i32,
            y: y as i32,
        };
        let Some(hit) = control
            .hit_test_browser(point, surface)
            .map_err(BrowserShellError::new)?
        else {
            return Ok(PresentedPointerLookup::Other);
        };
        if authoritative_receipt != hit.receipt() {
            return Ok(PresentedPointerLookup::InvalidAuthority);
        }
        let presented_hit = match hit.target() {
            BrowserHitTarget::Tab(identity) => Some(PresentedUiHit::Tab(identity)),
            BrowserHitTarget::TabClose(identity) => Some(PresentedUiHit::TabClose(identity)),
            BrowserHitTarget::AddressBar => Some(PresentedUiHit::AddressBar),
            BrowserHitTarget::Page { page, .. } => {
                if self.presented.page != BrowserPageSnapshot::Scene(page) {
                    return Ok(PresentedPointerLookup::InvalidAuthority);
                }
                Some(PresentedUiHit::Page)
            }
            BrowserHitTarget::PrimaryControl { element, kind } => {
                Some(PresentedUiHit::PrimaryControl { element, kind })
            }
            BrowserHitTarget::PrimaryPopupRow { element, kind } => {
                Some(PresentedUiHit::PopupRow { element, kind })
            }
            BrowserHitTarget::PrimaryPopupDismiss { kind, anchor } => {
                Some(PresentedUiHit::PopupDismiss { kind, anchor })
            }
            BrowserHitTarget::PrimaryPopupSurface { kind, anchor } => {
                return Ok(if self.presented_ui.popup_matches(kind, anchor) {
                    PresentedPointerLookup::Region(PresentedPointerRegion::PopupSurface {
                        kind,
                        anchor,
                    })
                } else {
                    PresentedPointerLookup::InvalidAuthority
                });
            }
            BrowserHitTarget::Status => return Ok(PresentedPointerLookup::Other),
        };
        let hit = presented_hit.expect("all non-surface hit targets were mapped");
        Ok(match self.presented_ui.disposition(hit) {
            Some(disposition) => {
                PresentedPointerLookup::Region(PresentedPointerRegion::Target { hit, disposition })
            }
            None => PresentedPointerLookup::InvalidAuthority,
        })
    }

    fn handle_presented_pointer(
        &mut self,
        pointer: &PointerEvent,
        control: &mut LinuxWindowControl<'_>,
    ) -> Result<(), BrowserShellError> {
        let Some(receipt) = self.presented.receipt else {
            self.presented_pointer.clear();
            control.request_redraw().map_err(BrowserShellError::new)?;
            return Ok(());
        };
        let region = match self.presented_pointer_lookup(pointer, control)? {
            PresentedPointerLookup::Region(region) => Some(region),
            PresentedPointerLookup::Other => None,
            PresentedPointerLookup::InvalidAuthority => {
                self.invalidate_hit_authority();
                control.request_redraw().map_err(BrowserShellError::new)?;
                return Ok(());
            }
        };
        let transition = self
            .presented_pointer
            .apply_pointer(pointer, receipt, region);
        if let Some(binding) = transition.activation {
            self.dispatch_presented_ui_binding(binding, control)?;
        } else if transition.visual_changed {
            control.request_redraw().map_err(BrowserShellError::new)?;
            self.presented_pointer.mark_visual_redraw_pending(receipt)?;
        }
        Ok(())
    }

    fn handle_presented_scroll(
        &mut self,
        scroll: &ScrollEvent,
        control: &mut LinuxWindowControl<'_>,
    ) -> Result<bool, BrowserShellError> {
        let snapshot = self
            .session
            .as_ref()
            .ok_or_else(|| BrowserShellError::new("browser session is not initialized"))?
            .primary_ui_snapshot(self.browser_window)
            .map_err(BrowserShellError::new)?;
        let Some(panel) = snapshot
            .panel
            .as_ref()
            .filter(|panel| panel.panel == PrimaryUiPanel::AllTabs)
        else {
            self.presented_pointer.popup_pixel_scroll = None;
            return Ok(false);
        };
        let native_surface = control
            .browser_surface_snapshot()
            .map_err(BrowserShellError::new)?
            .surface();
        if scroll.metadata.surface != native_surface {
            self.presented_pointer.popup_pixel_scroll = None;
            return Ok(false);
        }
        let canonical_focus = matches!(
            snapshot.focus,
            PrimaryUiFocus::PanelItem(_) | PrimaryUiFocus::Control(PrimaryUiControl::AllTabs)
        );
        let hover_receipt = self.presented.receipt.filter(|receipt| {
            self.presented_ui.popup.as_ref().is_some_and(|popup| {
                popup.kind == BrowserPrimaryPopupKind::AllTabs
                    && self.presented_pointer.hover_allows_popup_scroll(
                        *receipt,
                        scroll.metadata.surface,
                        popup,
                    )
            })
        });
        if !canonical_focus && hover_receipt.is_none() {
            self.presented_pointer.popup_pixel_scroll = None;
            return Ok(false);
        }
        let receipt_identity = if canonical_focus {
            None
        } else {
            hover_receipt.map(PresentedReceiptIdentity::from_receipt)
        };
        let Some((direction, rows)) = self.presented_pointer.normalized_popup_scroll(
            scroll,
            panel.visible_capacity,
            receipt_identity,
            BrowserPrimaryPopupKind::AllTabs,
        ) else {
            return Ok(true);
        };
        let Some(binding) = snapshot.bind_panel_scroll(direction, rows) else {
            return Ok(true);
        };
        let outcome = self
            .session_mut()?
            .dispatch_primary_ui_binding(binding)
            .map_err(BrowserShellError::new)?;
        match outcome {
            PrimaryUiActionOutcome::PanelScrolled { .. } => {
                self.presented_pointer.promote_popup_scroll_to_canonical(
                    scroll.metadata.surface,
                    BrowserPrimaryPopupKind::AllTabs,
                );
                let popup_pixel_scroll = self.presented_pointer.popup_pixel_scroll;
                self.invalidate_hit_authority();
                self.presented_pointer.popup_pixel_scroll = popup_pixel_scroll;
                self.sync_native_ime(control)?;
                control.request_redraw().map_err(BrowserShellError::new)?;
            }
            PrimaryUiActionOutcome::NoChange => {}
            PrimaryUiActionOutcome::Stale { .. } | PrimaryUiActionOutcome::Disabled(_) => {
                self.invalidate_hit_authority();
                control.request_redraw().map_err(BrowserShellError::new)?;
            }
            other => {
                return Err(BrowserShellError::new(format_args!(
                    "primary popup scroll produced an invalid outcome: {other:?}"
                )));
            }
        }
        Ok(true)
    }

    fn dispatch_presented_ui_binding(
        &mut self,
        binding: PrimaryUiActionBinding,
        control: &mut LinuxWindowControl<'_>,
    ) -> Result<(), BrowserShellError> {
        let outcome = self
            .session_mut()?
            .dispatch_primary_ui_binding(binding)
            .map_err(BrowserShellError::new)?;
        if let PrimaryUiActionOutcome::Command(command) = outcome {
            self.retire_rerender_authority_after_command(command);
            if command_requires_engine_poll(command) {
                self.polling.store(true, Ordering::Release);
            }
            if command_requests_native_exit(command) {
                self.invalidate_hit_authority();
                control.request_exit();
                return Ok(());
            }
        }
        self.invalidate_hit_authority();
        self.sync_native_ime(control)?;
        control.request_redraw().map_err(BrowserShellError::new)
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
        match input {
            InputEvent::Pointer(pointer) => {
                self.handle_presented_pointer(pointer, control)?;
                return Ok(());
            }
            InputEvent::Scroll(scroll) if self.handle_presented_scroll(scroll, control)? => {
                return Ok(());
            }
            InputEvent::Scroll(_) | InputEvent::Key(_) | InputEvent::Text(_) => {}
        }
        if input_requires_redraw(&outcome, false) {
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
        let invalidates_pointer_authority = matches!(
            event,
            LinuxWindowEvent::FocusChanged { .. } | LinuxWindowEvent::ImeDisabled { .. }
        );
        let outcome = self.route_event(event)?;
        self.retire_rerender_authority_after_routed(&outcome);
        if invalidates_pointer_authority || routed_outcome_mutates_chrome(&outcome) {
            self.invalidate_hit_authority();
        }
        if routed_outcome_requests_native_exit(&outcome) {
            control.request_exit();
            return Ok(());
        }
        self.sync_native_ime(control)?;
        control.request_redraw().map_err(BrowserShellError::new)
    }

    fn handle_input_device_removed(
        &mut self,
        event: LinuxWindowEvent,
        device: wild_buzzard_platform::InputDeviceId,
        control: &mut LinuxWindowControl<'_>,
    ) -> Result<(), BrowserShellError> {
        if self.retire_input_device(event, device)? {
            control.request_redraw().map_err(BrowserShellError::new)?;
        }
        Ok(())
    }

    fn retire_input_device(
        &mut self,
        event: LinuxWindowEvent,
        device: wild_buzzard_platform::InputDeviceId,
    ) -> Result<bool, BrowserShellError> {
        let outcome = self.route_event(event)?;
        if !matches!(outcome, LinuxEventOutcome::NativeStateChanged) {
            return Err(BrowserShellError::new(
                "input-device removal produced an invalid session outcome",
            ));
        }
        let affected = self.presented_pointer.input_device_removed(device);
        if affected {
            self.invalidate_hit_authority();
        }
        Ok(affected)
    }

    #[allow(clippy::too_many_lines)]
    fn advance_smoke_after_composition(
        &mut self,
        active: BrowserTabId,
        installing: bool,
        control: &mut LinuxWindowControl<'_>,
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
        if !smoke_composition_may_advance(&self.smoke_stage, installing) {
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
                let popup_binding = self
                    .session
                    .as_ref()
                    .ok_or_else(|| BrowserShellError::new("browser session is not initialized"))?
                    .primary_ui_snapshot(self.browser_window)
                    .map_err(BrowserShellError::new)?
                    .bind_action(PrimaryUiElementId::Control(
                        PrimaryUiControl::ApplicationMenu,
                    ))
                    .ok_or_else(|| {
                        BrowserShellError::new(
                            "live primary-UI smoke application control lacks exact authority",
                        )
                    })?;
                self.invalidate_hit_authority();
                let popup_outcome = self
                    .session_mut()?
                    .dispatch_primary_ui_binding(popup_binding)
                    .map_err(BrowserShellError::new)?;
                if popup_outcome
                    != PrimaryUiActionOutcome::PanelChanged(Some(PrimaryUiPanel::ApplicationMenu))
                {
                    return Err(BrowserShellError::new(format_args!(
                        "live primary-UI smoke did not open its application popup: {popup_outcome:?}"
                    )));
                }
                control.request_redraw().map_err(BrowserShellError::new)?;
                SmokeStage::AwaitApplicationPopup { initial }
            }
            SmokeStage::AwaitApplicationPopup { initial } => {
                let popup_snapshot = self
                    .session
                    .as_ref()
                    .ok_or_else(|| BrowserShellError::new("browser session is not initialized"))?
                    .primary_ui_snapshot(self.browser_window)
                    .map_err(BrowserShellError::new)?;
                let popup = popup_snapshot.panel.as_ref().ok_or_else(|| {
                    BrowserShellError::new("live primary-UI smoke popup was not projected")
                })?;
                if popup.panel != PrimaryUiPanel::ApplicationMenu
                    || popup.items.len() != 5
                    || popup.visible_capacity == 0
                {
                    return Err(BrowserShellError::new(
                        "live primary-UI smoke popup inventory or capacity drifted",
                    ));
                }
                let dismissal = popup_snapshot.bind_panel_dismissal().ok_or_else(|| {
                    BrowserShellError::new(
                        "live primary-UI smoke popup lacks exact dismissal authority",
                    )
                })?;
                self.invalidate_hit_authority();
                let dismissal_outcome = self
                    .session_mut()?
                    .dispatch_primary_ui_binding(dismissal)
                    .map_err(BrowserShellError::new)?;
                if dismissal_outcome != PrimaryUiActionOutcome::PanelChanged(None) {
                    return Err(BrowserShellError::new(format_args!(
                        "live primary-UI smoke did not dismiss its application popup: {dismissal_outcome:?}"
                    )));
                }
                control.request_redraw().map_err(BrowserShellError::new)?;
                SmokeStage::AwaitPopupDismissed { initial }
            }
            SmokeStage::AwaitPopupDismissed { initial } => {
                if self
                    .session
                    .as_ref()
                    .ok_or_else(|| BrowserShellError::new("browser session is not initialized"))?
                    .primary_ui_snapshot(self.browser_window)
                    .map_err(BrowserShellError::new)?
                    .panel
                    .is_some()
                {
                    return Err(BrowserShellError::new(
                        "live primary-UI smoke retained a dismissed popup",
                    ));
                }
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
        control: &mut LinuxWindowControl<'_>,
    ) -> Result<(), BrowserShellError> {
        self.advance_smoke_after_composition(active, installing, control)?;
        self.advance_smoke_after_resize(receipt.request().surface().size(), control)
    }

    fn advance_smoke_after_resize(
        &mut self,
        size: PhysicalSize,
        control: &mut LinuxWindowControl<'_>,
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
        self.invalidate_hit_authority();
        self.polling.store(false, Ordering::Release);
        #[cfg(feature = "webdriver")]
        if let (Some(automation), Some(session)) = (self.automation.as_mut(), self.session.as_mut())
        {
            automation.shutdown(session);
        }
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
                #[cfg(feature = "webdriver")]
                let result = self
                    .drain_automation(control)
                    .and_then(|()| self.pump_engine(control));
                #[cfg(not(feature = "webdriver"))]
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
            LinuxWindowEvent::InputDeviceRemoved { device, .. } if self.session.is_some() => {
                self.handle_input_device_removed(event.clone(), *device, control)
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

    use wild_buzzard_ui::PrimaryUiAction;

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
        PointerKind, ScaleFactor, ScrollVector, SeatId, SurfaceDescriptor, SurfaceIdAllocator,
        SurfaceRole, TextInputEvent,
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

    #[test]
    fn ordinary_product_selects_general_web_while_smoke_retains_loopback() {
        let ordinary = BrowserHandler::new(
            Some("https://example.com/".into()),
            None,
            Arc::new(AtomicBool::new(false)),
        );
        assert_eq!(ordinary.navigation_mode, BrowserNavigationMode::GeneralWeb);

        let smoke = BrowserHandler::new(
            Some("http://127.0.0.1:8000/a".into()),
            Some(BrowserSmokeConfig {
                second_url: "http://127.0.0.1:8000/b".into(),
                hard_deadline: Duration::from_secs(20),
            }),
            Arc::new(AtomicBool::new(true)),
        );
        assert_eq!(
            smoke.navigation_mode,
            BrowserNavigationMode::NumericLoopback
        );
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
        pointer_event_with_phase(surface, sequence, PointerPhase::Down, buttons)
    }

    fn pointer_event_with_phase(
        surface: wild_buzzard_platform::SurfaceId,
        sequence: u64,
        phase: PointerPhase,
        buttons: u16,
    ) -> PointerEvent {
        PointerEvent {
            metadata: test_metadata(surface, sequence, Modifiers::default()),
            pointer: PointerId::new(1).unwrap(),
            kind: PointerKind::Mouse,
            phase,
            position: LogicalPoint::new(10.0, 10.0).unwrap(),
            buttons,
            pressure: None,
        }
    }

    fn scroll_event(
        surface: wild_buzzard_platform::SurfaceId,
        sequence: u64,
        delta: ScrollDelta,
        phase: ScrollPhase,
    ) -> ScrollEvent {
        ScrollEvent {
            metadata: test_metadata(surface, sequence, Modifiers::default()),
            delta,
            phase,
        }
    }

    const fn receipt_identity(
        surface: wild_buzzard_platform::SurfaceId,
        sequence: u64,
    ) -> PresentedReceiptIdentity {
        PresentedReceiptIdentity {
            surface,
            surface_revision: 1,
            chrome_revision: sequence,
            root_epoch: 1,
            sequence,
            backend_publish_id: sequence,
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
    #[allow(clippy::too_many_lines)]
    fn presented_primary_authority_requires_exact_identity_kind_and_live_receipt_epoch() {
        let mut session = test_session();
        let window = BrowserWindowId::new(1).unwrap();
        let tab = BrowserTabId::new(1).unwrap();
        let navigation = match session
            .navigate_new(tab, "http://authority.invalid/")
            .unwrap()
        {
            BrowserCommandOutcome::NavigationQueued { navigation, .. } => navigation,
            other => panic!("unexpected navigation outcome: {other:?}"),
        };
        wait_for_navigation(&mut session, tab, navigation);
        let revision = session.primary_ui_revision(window).unwrap();
        assert_eq!(
            session
                .dispatch_primary_ui_action(
                    window,
                    revision,
                    PrimaryUiAction::InvokeControl(PrimaryUiControl::SiteIdentity),
                )
                .unwrap(),
            PrimaryUiActionOutcome::PanelChanged(Some(PrimaryUiPanel::SiteIdentity)),
        );
        let snapshot = session.primary_ui_snapshot(window).unwrap();
        let page = snapshot.bind_action(PrimaryUiElementId::Page).unwrap();
        let new_tab = snapshot
            .bind_action(PrimaryUiElementId::Control(PrimaryUiControl::NewTab))
            .unwrap();
        assert_eq!(
            snapshot.bind_action(PrimaryUiElementId::Control(PrimaryUiControl::Back)),
            None,
        );
        let identity_row = snapshot.panel.as_ref().unwrap().items[0].id;
        assert_eq!(identity_row, PrimaryUiPanelItemId::IdentitySummary);
        assert_eq!(
            snapshot.bind_action(PrimaryUiElementId::PanelItem(identity_row)),
            None,
        );
        let element = BrowserChromeElementIdentity::new(7).unwrap();
        let disabled_back = PresentedUiHit::PrimaryControl {
            element: BrowserChromeElementIdentity::new(8).unwrap(),
            kind: BrowserPrimaryControlKind::Back,
        };
        let disabled_identity_row = PresentedUiHit::PopupRow {
            element: BrowserChromeElementIdentity::new(9).unwrap(),
            kind: BrowserPrimaryPopupRowKind::Action(BrowserPrimaryActionKind::SiteInformation),
        };
        let mut authority = PresentedUiAuthority::default();
        authority.push_action(PresentedUiHit::Page, page).unwrap();
        authority
            .push_action(
                PresentedUiHit::PrimaryControl {
                    element,
                    kind: BrowserPrimaryControlKind::NewTab,
                },
                new_tab,
            )
            .unwrap();
        authority.push_disabled(disabled_back).unwrap();
        authority.push_disabled(disabled_identity_row).unwrap();

        assert_eq!(
            authority.disposition(PresentedUiHit::Page),
            Some(PresentedUiDisposition::Action(page)),
        );
        assert_eq!(
            authority.disposition(PresentedUiHit::PrimaryControl {
                element,
                kind: BrowserPrimaryControlKind::NewTab,
            }),
            Some(PresentedUiDisposition::Action(new_tab)),
        );
        assert_eq!(
            authority.disposition(PresentedUiHit::PrimaryControl {
                element,
                kind: BrowserPrimaryControlKind::ApplicationMenu,
            }),
            None,
            "a matching element with a forged control kind has no authority",
        );
        let before_disabled_hits = session.primary_ui_snapshot(window).unwrap();
        for _ in 0..2 {
            assert_eq!(
                authority.disposition(disabled_back),
                Some(PresentedUiDisposition::ConsumeDisabled),
            );
            assert_eq!(
                authority.disposition(disabled_identity_row),
                Some(PresentedUiDisposition::ConsumeDisabled),
            );
        }
        let after_disabled_hits = session.primary_ui_snapshot(window).unwrap();
        assert_eq!(after_disabled_hits.revision, before_disabled_hits.revision);
        assert_eq!(after_disabled_hits.focus, before_disabled_hits.focus);
        assert_eq!(session.tab_count(), 1);
        assert_eq!(
            session.tab_snapshot(tab).unwrap().live_navigation,
            Some(navigation),
            "consuming repeated disabled hits cannot produce an engine command",
        );
        assert!(
            authority
                .push(
                    PresentedUiHit::PrimaryControl {
                        element,
                        kind: BrowserPrimaryControlKind::NewTab,
                    },
                    PresentedUiDisposition::Action(new_tab),
                )
                .is_err(),
            "one presented hit target cannot carry two actions",
        );

        authority.clear();
        assert_eq!(authority.disposition(PresentedUiHit::Page), None);
        assert_eq!(
            authority.disposition(PresentedUiHit::PrimaryControl {
                element,
                kind: BrowserPrimaryControlKind::NewTab,
            }),
            None,
            "receipt invalidation clears every exact presented action",
        );
        let _ = session.shutdown();
    }

    #[test]
    fn pointer_activation_requires_exact_down_up_contact_across_visual_receipts() {
        let (surface, _) = ready_event();
        let mut session = test_session();
        let window = BrowserWindowId::new(1).unwrap();
        let binding = session
            .primary_ui_snapshot(window)
            .unwrap()
            .bind_action(PrimaryUiElementId::Control(PrimaryUiControl::NewTab))
            .unwrap();
        let hit = PresentedUiHit::PrimaryControl {
            element: BrowserChromeElementIdentity::new(41).unwrap(),
            kind: BrowserPrimaryControlKind::NewTab,
        };
        let region = PresentedPointerRegion::Target {
            hit,
            disposition: PresentedUiDisposition::Action(binding),
        };
        let mut authority = PresentedUiAuthority::default();
        authority.push_action(hit, binding).unwrap();
        let first = receipt_identity(surface, 1);
        let hovered = receipt_identity(surface, 2);
        let pressed = receipt_identity(surface, 3);
        let mut pointer = PresentedPointerState::default();

        let hover = pointer.apply_pointer_for(
            &pointer_event_with_phase(surface, 1, PointerPhase::Move, 0),
            first,
            Some(region),
        );
        assert_eq!(hover.activation, None);
        assert!(hover.visual_changed);
        assert_eq!(
            pointer.visual_interaction_for(hit, first),
            BrowserElementInteraction::Hovered
        );
        pointer.mark_visual_redraw_pending_for(first).unwrap();
        let hover_handoff = pointer.prepare_handoff_for(first, &authority, surface, true, true);
        assert!(hover_handoff.generation.is_some());
        pointer.commit_handoff_for(hovered, &hover_handoff);
        assert_eq!(
            pointer.visual_interaction_for(hit, hovered),
            BrowserElementInteraction::Hovered
        );

        let down = pointer.apply_pointer_for(
            &pointer_event_with_phase(surface, 2, PointerPhase::Down, PRIMARY_POINTER_BUTTON),
            hovered,
            Some(region),
        );
        assert_eq!(down.activation, None, "pointer Down never invokes chrome");
        assert!(down.visual_changed);
        assert_eq!(
            pointer.visual_interaction_for(hit, hovered),
            BrowserElementInteraction::Pressed
        );
        pointer.mark_visual_redraw_pending_for(hovered).unwrap();
        let press_handoff = pointer.prepare_handoff_for(hovered, &authority, surface, true, true);
        assert!(press_handoff.generation.is_some());
        pointer.commit_handoff_for(pressed, &press_handoff);

        let up = pointer.apply_pointer_for(
            &pointer_event_with_phase(surface, 3, PointerPhase::Up, 0),
            pressed,
            Some(region),
        );
        assert_eq!(up.activation, Some(binding));
        assert!(up.visual_changed);
        assert_eq!(
            pointer.visual_interaction_for(hit, pressed),
            BrowserElementInteraction::Hovered
        );
        assert_eq!(
            pointer
                .apply_pointer_for(
                    &pointer_event_with_phase(surface, 4, PointerPhase::Up, 0),
                    pressed,
                    Some(region),
                )
                .activation,
            None,
            "one exact capture can activate only once",
        );
        let _ = session.shutdown();
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn pointer_capture_cancels_on_drag_stale_receipt_disabled_target_and_authority_change() {
        let (surface, _) = ready_event();
        let mut session = test_session();
        let snapshot = session
            .primary_ui_snapshot(BrowserWindowId::new(1).unwrap())
            .unwrap();
        let action = snapshot
            .bind_action(PrimaryUiElementId::Control(PrimaryUiControl::NewTab))
            .unwrap();
        let different_action = snapshot.bind_action(PrimaryUiElementId::Page).unwrap();
        let hit = PresentedUiHit::PrimaryControl {
            element: BrowserChromeElementIdentity::new(42).unwrap(),
            kind: BrowserPrimaryControlKind::NewTab,
        };
        let region = PresentedPointerRegion::Target {
            hit,
            disposition: PresentedUiDisposition::Action(action),
        };
        let disabled = PresentedPointerRegion::Target {
            hit,
            disposition: PresentedUiDisposition::ConsumeDisabled,
        };
        let first = receipt_identity(surface, 10);
        let second = receipt_identity(surface, 11);

        let mut pointer = PresentedPointerState::default();
        pointer.apply_pointer_for(
            &pointer_event_with_phase(surface, 10, PointerPhase::Down, PRIMARY_POINTER_BUTTON),
            first,
            Some(region),
        );
        pointer.apply_pointer_for(
            &pointer_event_with_phase(surface, 11, PointerPhase::Move, PRIMARY_POINTER_BUTTON),
            first,
            None,
        );
        assert_eq!(
            pointer
                .apply_pointer_for(
                    &pointer_event_with_phase(surface, 12, PointerPhase::Up, 0),
                    first,
                    Some(region),
                )
                .activation,
            None,
            "dragging away cancels capture",
        );

        pointer.apply_pointer_for(
            &pointer_event_with_phase(surface, 13, PointerPhase::Down, PRIMARY_POINTER_BUTTON),
            first,
            Some(region),
        );
        assert_eq!(
            pointer
                .apply_pointer_for(
                    &pointer_event_with_phase(surface, 14, PointerPhase::Up, 0),
                    second,
                    Some(region),
                )
                .activation,
            None,
            "a stale receipt cannot release a current capture",
        );

        pointer.clear();
        pointer.apply_pointer_for(
            &pointer_event_with_phase(surface, 15, PointerPhase::Down, PRIMARY_POINTER_BUTTON),
            second,
            Some(region),
        );
        let mut foreign_device_up = pointer_event_with_phase(surface, 16, PointerPhase::Up, 0);
        foreign_device_up.metadata.device = InputDeviceId::new(2).unwrap();
        assert_eq!(
            pointer
                .apply_pointer_for(&foreign_device_up, second, Some(region))
                .activation,
            None,
            "a different native input device cannot release capture",
        );

        pointer.clear();
        pointer.apply_pointer_for(
            &pointer_event_with_phase(surface, 17, PointerPhase::Down, PRIMARY_POINTER_BUTTON),
            second,
            Some(region),
        );
        let mut foreign_kind_up = pointer_event_with_phase(surface, 18, PointerPhase::Up, 0);
        foreign_kind_up.kind = PointerKind::Touch;
        assert_eq!(
            pointer
                .apply_pointer_for(&foreign_kind_up, second, Some(region))
                .activation,
            None,
            "a forged pointer kind cannot release mouse capture",
        );

        pointer.clear();
        let disabled_down = pointer.apply_pointer_for(
            &pointer_event_with_phase(surface, 19, PointerPhase::Down, PRIMARY_POINTER_BUTTON),
            second,
            Some(disabled),
        );
        assert_eq!(disabled_down.activation, None);
        assert!(!disabled_down.visual_changed);
        assert!(pointer.capture.is_none());
        assert_eq!(
            pointer
                .apply_pointer_for(
                    &pointer_event_with_phase(surface, 16, PointerPhase::Up, 0),
                    second,
                    Some(disabled),
                )
                .activation,
            None,
        );

        pointer.apply_pointer_for(
            &pointer_event_with_phase(surface, 21, PointerPhase::Down, PRIMARY_POINTER_BUTTON),
            second,
            Some(region),
        );
        pointer.mark_visual_redraw_pending_for(second).unwrap();
        let mut changed_authority = PresentedUiAuthority::default();
        changed_authority
            .push_action(hit, different_action)
            .unwrap();
        let rejected = pointer.prepare_handoff_for(second, &changed_authority, surface, true, true);
        assert_eq!(rejected.generation, None);
        pointer.commit_handoff_for(receipt_identity(surface, 12), &rejected);
        assert!(pointer.capture.is_none());
        assert_eq!(
            pointer.visual_interaction_for(hit, receipt_identity(surface, 12)),
            BrowserElementInteraction::Idle,
            "an authority-changing redraw cannot retain pressed pixels",
        );

        pointer.apply_pointer_for(
            &pointer_event_with_phase(surface, 22, PointerPhase::Down, PRIMARY_POINTER_BUTTON),
            second,
            Some(region),
        );
        assert!(pointer.input_device_removed(InputDeviceId::new(1).unwrap()));
        assert!(pointer.capture.is_none());
        assert_eq!(
            pointer.visual_interaction_for(hit, second),
            BrowserElementInteraction::Idle,
            "device removal cancels the device's pressed authority",
        );
        assert_eq!(
            pointer
                .apply_pointer_for(
                    &pointer_event_with_phase(surface, 23, PointerPhase::Up, 0),
                    second,
                    Some(region),
                )
                .activation,
            None,
        );
        let _ = session.shutdown();
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn popup_scroll_normalization_accumulates_pixels_and_resets_exact_context() {
        let (surface, _) = ready_event();
        let first = receipt_identity(surface, 20);
        let second = receipt_identity(surface, 21);
        let mut pointer = PresentedPointerState::default();
        let pixels = |sequence, y, phase| {
            scroll_event(
                surface,
                sequence,
                ScrollDelta::Pixels(ScrollVector::new(0.0, y).unwrap()),
                phase,
            )
        };

        assert_eq!(
            pointer.normalized_popup_scroll(
                &pixels(1, -30.0, ScrollPhase::Begin),
                3,
                Some(first),
                BrowserPrimaryPopupKind::AllTabs,
            ),
            None,
        );
        assert_eq!(
            pointer.normalized_popup_scroll(
                &pixels(2, -10.0, ScrollPhase::Update),
                3,
                Some(first),
                BrowserPrimaryPopupKind::AllTabs,
            ),
            Some((PrimaryUiMoveDirection::Forward, 1)),
        );
        assert_eq!(
            pointer.normalized_popup_scroll(
                &pixels(3, -30.0, ScrollPhase::Begin),
                3,
                Some(first),
                BrowserPrimaryPopupKind::AllTabs,
            ),
            None,
        );
        assert_eq!(
            pointer.normalized_popup_scroll(
                &pixels(4, -10.0, ScrollPhase::Update),
                3,
                Some(second),
                BrowserPrimaryPopupKind::AllTabs,
            ),
            None,
            "a different exact receipt resets the partial gesture",
        );
        assert_eq!(
            pointer.normalized_popup_scroll(
                &pixels(5, -30.0, ScrollPhase::Update),
                3,
                Some(second),
                BrowserPrimaryPopupKind::AllTabs,
            ),
            Some((PrimaryUiMoveDirection::Forward, 1)),
        );
        assert_eq!(
            pointer.normalized_popup_scroll(
                &pixels(6, -30.0, ScrollPhase::Begin),
                3,
                None,
                BrowserPrimaryPopupKind::AllTabs,
            ),
            None,
        );
        let mut foreign_device_update = pixels(7, -10.0, ScrollPhase::Update);
        foreign_device_update.metadata.device = InputDeviceId::new(2).unwrap();
        assert_eq!(
            pointer.normalized_popup_scroll(
                &foreign_device_update,
                3,
                None,
                BrowserPrimaryPopupKind::AllTabs,
            ),
            None,
            "pixel remainders cannot combine across input devices",
        );
        let mut same_foreign_device_update = pixels(8, -30.0, ScrollPhase::Update);
        same_foreign_device_update.metadata.device = InputDeviceId::new(2).unwrap();
        assert_eq!(
            pointer.normalized_popup_scroll(
                &same_foreign_device_update,
                3,
                None,
                BrowserPrimaryPopupKind::AllTabs,
            ),
            Some((PrimaryUiMoveDirection::Forward, 1)),
        );
        assert_eq!(
            pointer.normalized_popup_scroll(
                &pixels(9, -30.0, ScrollPhase::Begin),
                3,
                None,
                BrowserPrimaryPopupKind::AllTabs,
            ),
            None,
        );
        let mut foreign_seat_update = pixels(10, -10.0, ScrollPhase::Update);
        foreign_seat_update.metadata.seat = SeatId::new(2).unwrap();
        assert_eq!(
            pointer.normalized_popup_scroll(
                &foreign_seat_update,
                3,
                None,
                BrowserPrimaryPopupKind::AllTabs,
            ),
            None,
            "pixel remainders cannot combine across seats",
        );
        assert_eq!(
            pointer.normalized_popup_scroll(
                &pixels(11, 20.0, ScrollPhase::Begin),
                3,
                Some(second),
                BrowserPrimaryPopupKind::AllTabs,
            ),
            None,
        );
        assert_eq!(
            pointer.normalized_popup_scroll(
                &pixels(12, 20.0, ScrollPhase::End),
                3,
                Some(second),
                BrowserPrimaryPopupKind::AllTabs,
            ),
            Some((PrimaryUiMoveDirection::Backward, 1)),
            "End applies its final complete row before clearing remainder",
        );
        assert_eq!(
            pointer.normalized_popup_scroll(
                &pixels(13, -39.0, ScrollPhase::Update),
                3,
                Some(second),
                BrowserPrimaryPopupKind::AllTabs,
            ),
            None,
            "the completed gesture left no remainder",
        );
        assert_eq!(
            pointer.normalized_popup_scroll(
                &pixels(14, -1.0, ScrollPhase::Cancel),
                3,
                Some(second),
                BrowserPrimaryPopupKind::AllTabs,
            ),
            None,
        );
        assert_eq!(
            pointer.normalized_popup_scroll(
                &pixels(15, -1.0, ScrollPhase::Update),
                3,
                Some(second),
                BrowserPrimaryPopupKind::AllTabs,
            ),
            None,
            "Cancel discards the prior partial row",
        );

        let lines = scroll_event(
            surface,
            11,
            ScrollDelta::Lines(ScrollVector::new(0.0, -3.0).unwrap()),
            ScrollPhase::Discrete,
        );
        assert_eq!(
            pointer.normalized_popup_scroll(&lines, 3, None, BrowserPrimaryPopupKind::AllTabs,),
            Some((PrimaryUiMoveDirection::Forward, 3)),
        );
        let pages = scroll_event(
            surface,
            12,
            ScrollDelta::Pages(ScrollVector::new(0.0, 2.0).unwrap()),
            ScrollPhase::Discrete,
        );
        assert_eq!(
            pointer.normalized_popup_scroll(&pages, 3, None, BrowserPrimaryPopupKind::AllTabs,),
            Some((PrimaryUiMoveDirection::Backward, 6)),
        );
        let bounded = scroll_event(
            surface,
            13,
            ScrollDelta::Lines(ScrollVector::new(0.0, -10_000.0).unwrap()),
            ScrollPhase::Discrete,
        );
        assert_eq!(
            pointer.normalized_popup_scroll(&bounded, 3, None, BrowserPrimaryPopupKind::AllTabs,),
            Some((PrimaryUiMoveDirection::Forward, MAX_PRIMARY_UI_SCROLL_ROWS,)),
        );
    }

    #[test]
    fn canonical_all_tabs_scroll_survives_bursts_and_its_own_redraw_boundary() {
        let (surface, _) = ready_event();
        let mut session = test_session();
        let window = BrowserWindowId::new(1).unwrap();
        for _ in 1..20 {
            session.open_tab(window).unwrap();
        }
        session
            .set_primary_ui_layout(
                window,
                PrimaryUiLayout::new(
                    PrimaryUiControlSet::wide_defaults(),
                    PrimaryUiControlSet::empty(),
                    3,
                )
                .unwrap(),
            )
            .unwrap();
        let revision = session.primary_ui_revision(window).unwrap();
        assert!(matches!(
            session
                .dispatch_primary_ui_action(
                    window,
                    revision,
                    PrimaryUiAction::InvokeControl(PrimaryUiControl::AllTabs),
                )
                .unwrap(),
            PrimaryUiActionOutcome::PanelChanged(Some(PrimaryUiPanel::AllTabs))
        ));
        let mut pointer = PresentedPointerState::default();
        for sequence in 1..=17 {
            let snapshot = session.primary_ui_snapshot(window).unwrap();
            assert!(matches!(
                snapshot.focus,
                PrimaryUiFocus::PanelItem(_) | PrimaryUiFocus::Control(PrimaryUiControl::AllTabs)
            ));
            let event = scroll_event(
                surface,
                sequence,
                ScrollDelta::Lines(ScrollVector::new(0.0, 1.0).unwrap()),
                ScrollPhase::Discrete,
            );
            let (direction, rows) = pointer
                .normalized_popup_scroll(
                    &event,
                    snapshot.panel.as_ref().unwrap().visible_capacity,
                    None,
                    BrowserPrimaryPopupKind::AllTabs,
                )
                .unwrap();
            let binding = snapshot.bind_panel_scroll(direction, rows).unwrap();
            assert!(matches!(
                session.dispatch_primary_ui_binding(binding).unwrap(),
                PrimaryUiActionOutcome::PanelScrolled { .. }
            ));
        }
        assert_eq!(
            session
                .primary_ui_snapshot(window)
                .unwrap()
                .panel
                .unwrap()
                .scroll_offset,
            0,
            "a pre-redraw burst reaches the first offscreen tab",
        );

        let begin = scroll_event(
            surface,
            18,
            ScrollDelta::Pixels(ScrollVector::new(0.0, -50.0).unwrap()),
            ScrollPhase::Begin,
        );
        assert_eq!(
            pointer.normalized_popup_scroll(&begin, 3, None, BrowserPrimaryPopupKind::AllTabs,),
            Some((PrimaryUiMoveDirection::Forward, 1)),
        );
        pointer.promote_popup_scroll_to_canonical(surface, BrowserPrimaryPopupKind::AllTabs);
        pointer.clear_pointer_contacts();
        pointer.retain_canonical_popup_scroll(surface, true);
        pointer.commit_handoff_for(
            receipt_identity(surface, 30),
            &PointerReceiptHandoff::default(),
        );
        let update = scroll_event(
            surface,
            19,
            ScrollDelta::Pixels(ScrollVector::new(0.0, -30.0).unwrap()),
            ScrollPhase::Update,
        );
        assert_eq!(
            pointer.normalized_popup_scroll(&update, 3, None, BrowserPrimaryPopupKind::AllTabs,),
            Some((PrimaryUiMoveDirection::Forward, 1)),
            "the exact canonical 10px remainder survives its requested redraw",
        );
        let _ = session.shutdown();
    }

    #[test]
    fn terminal_finish_clears_all_presented_interaction_authority() {
        let (surface, _) = ready_event();
        let polling = Arc::new(AtomicBool::new(true));
        let mut handler = BrowserHandler::new(None, None, Arc::clone(&polling));
        handler
            .presented_ui
            .push_disabled(PresentedUiHit::Page)
            .unwrap();
        handler.presented_pointer.hover = Some(PresentedPointerContact {
            receipt: receipt_identity(surface, 40),
            pointer: PointerId::new(1).unwrap(),
            seat: SeatId::new(1).unwrap(),
            device: InputDeviceId::new(1).unwrap(),
            kind: PointerKind::Mouse,
            surface,
            region: PresentedPointerRegion::PopupSurface {
                kind: BrowserPrimaryPopupKind::AllTabs,
                anchor: BrowserChromeElementIdentity::new(43).unwrap(),
            },
        });
        handler.presented_pointer.popup_pixel_scroll = Some(PopupPixelScrollAccumulator {
            receipt: None,
            surface,
            seat: SeatId::new(1).unwrap(),
            device: InputDeviceId::new(1).unwrap(),
            kind: BrowserPrimaryPopupKind::AllTabs,
            pixels: 12.0,
        });

        handler.finish();

        assert!(handler.presented_ui.entries.is_empty());
        assert!(handler.presented_ui.popup.is_none());
        assert!(handler.presented_pointer.hover.is_none());
        assert!(handler.presented_pointer.capture.is_none());
        assert!(handler.presented_pointer.pending_visual_redraw.is_none());
        assert!(handler.presented_pointer.popup_pixel_scroll.is_none());
        assert!(!polling.load(Ordering::Acquire));
    }

    #[test]
    fn normalized_device_removal_retires_affected_shell_capture_and_hit_authority() {
        let polling = Arc::new(AtomicBool::new(false));
        let mut handler = BrowserHandler::new(None, None, polling);
        handler.session = Some(test_session());
        let (surface, ready) = ready_event();
        handler.route_event(ready).unwrap();
        let binding = handler
            .session
            .as_ref()
            .unwrap()
            .primary_ui_snapshot(BrowserWindowId::new(1).unwrap())
            .unwrap()
            .bind_action(PrimaryUiElementId::Control(PrimaryUiControl::NewTab))
            .unwrap();
        let hit = PresentedUiHit::PrimaryControl {
            element: BrowserChromeElementIdentity::new(44).unwrap(),
            kind: BrowserPrimaryControlKind::NewTab,
        };
        handler.presented_pointer.apply_pointer_for(
            &pointer_event_with_phase(surface, 1, PointerPhase::Down, PRIMARY_POINTER_BUTTON),
            receipt_identity(surface, 1),
            Some(PresentedPointerRegion::Target {
                hit,
                disposition: PresentedUiDisposition::Action(binding),
            }),
        );
        handler.presented_ui.push_action(hit, binding).unwrap();

        let device = InputDeviceId::new(1).unwrap();
        assert!(
            handler
                .retire_input_device(
                    LinuxWindowEvent::InputDeviceRemoved { surface, device },
                    device,
                )
                .unwrap()
        );
        assert!(handler.presented_pointer.capture.is_none());
        assert!(handler.presented_ui.entries.is_empty());

        let foreign = InputDeviceId::new(2).unwrap();
        assert!(
            !handler
                .retire_input_device(
                    LinuxWindowEvent::InputDeviceRemoved {
                        surface,
                        device: foreign,
                    },
                    foreign,
                )
                .unwrap()
        );
        handler.finish();
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
    fn popup_smoke_stages_advance_on_retained_page_compositions() {
        let initial = PhysicalSize::new(1_366, 768).unwrap();
        assert!(smoke_composition_may_advance(
            &SmokeStage::AwaitApplicationPopup { initial },
            false,
        ));
        assert!(smoke_composition_may_advance(
            &SmokeStage::AwaitPopupDismissed { initial },
            false,
        ));
        assert!(!smoke_composition_may_advance(
            &SmokeStage::AwaitSecondPage,
            false,
        ));
        assert!(smoke_composition_may_advance(
            &SmokeStage::AwaitSecondPage,
            true,
        ));
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
