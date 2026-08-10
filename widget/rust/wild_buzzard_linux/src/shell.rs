use std::cell::Cell;
use std::error::Error;
use std::fmt;
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::Arc;

use wild_buzzard_linux_presenter::{
    BrowserChromeScene, BrowserFrameReceipt, BrowserFrameRequest, BrowserHitTestResult,
    BrowserPageUpdate, LinuxPresentationBackend, LinuxPresentedWindow, LinuxPresenterCreationError,
    PresentationError, PresentationFailureStage, SolidColorFrame, SwapSubmissionReceipt,
    WebRenderPresentedWindow, WebRenderSurfaceSnapshot, WebRenderWindowError,
    WebRenderWindowFailureStage, WebRenderWindowResizeRequest, WebRenderWindowStartupFailure,
    prepare_and_attach,
};
use wild_buzzard_platform::{
    LogicalPoint, LogicalRect, LogicalSize, PhysicalPoint, PhysicalSize, ScaleFactor,
    SurfaceDescriptor, SurfaceId, SurfaceIdAllocator, SurfaceRole,
};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize as WinitLogicalSize;
use winit::event::{DeviceEvent, DeviceId, Ime, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::platform::wayland::{
    ActiveEventLoopExtWayland, EventLoopBuilderExtWayland, WindowAttributesExtWayland,
};
use winit::platform::x11::{EventLoopBuilderExtX11, WindowAttributesExtX11};
use winit::window::{Window, WindowId};

use crate::config::{ConfigError, LinuxBackendPreference, LinuxPresentationMode, LinuxShellConfig};
use crate::event::{
    ControlError, LinuxBackend, LinuxBrowserShutdownFailure, LinuxPresentationShutdown,
    LinuxShutdownReport, LinuxStopReason, LinuxWindowEvent,
};
use crate::lifecycle::{ShellState, WakeAdmission, WakeGate, WakeOwner};
use crate::normalize::{InputBatch, InputNormalizer, physical_size, scale_factor};
use crate::queue::PushError;

#[derive(Clone, Copy, Debug)]
enum WakeEvent {
    Wake,
}

/// Result of requesting a bounded cross-thread event-loop wake.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LinuxWakeStatus {
    /// One wake was admitted to winit's user-event channel.
    Queued,
    /// A prior wake is still pending, so no second event was sent.
    AlreadyPending,
    /// The event loop has closed and cannot be woken.
    Closed,
}

/// Cloneable, payload-free, coalescing handle for waking the owner thread.
#[derive(Clone)]
pub struct LinuxWakeHandle {
    proxy: EventLoopProxy<WakeEvent>,
    gate: Arc<WakeGate>,
}

enum AttachedWindow {
    Direct(Box<LinuxPresentedWindow>),
    Browser(Box<WebRenderPresentedWindow>),
}

impl AttachedWindow {
    fn request_redraw(&self) {
        match self {
            Self::Direct(window) => window.request_redraw(),
            Self::Browser(window) => window.request_redraw(),
        }
    }

    fn set_ime_allowed(&self, allowed: bool) {
        match self {
            Self::Direct(window) => window.set_ime_allowed(allowed),
            Self::Browser(window) => window.set_ime_allowed(allowed),
        }
    }

    fn set_ime_cursor_area(&self, area: LogicalRect) {
        match self {
            Self::Direct(window) => window.set_ime_cursor_area(
                area.origin.x,
                area.origin.y,
                area.size.width,
                area.size.height,
            ),
            Self::Browser(window) => window.set_ime_cursor_area(area),
        }
    }

    fn matches_window_id(&self, id: WindowId) -> bool {
        match self {
            Self::Direct(window) => window.matches_window_id(id),
            Self::Browser(window) => window.matches_window_id(id),
        }
    }

    fn request_inner_size(&self, size: PhysicalSize) -> Option<PhysicalSize> {
        match self {
            Self::Direct(window) => window.request_inner_size(size),
            Self::Browser(window) => window.request_inner_size(size),
        }
    }

    fn surface_snapshot(&self) -> Option<WebRenderSurfaceSnapshot> {
        match self {
            Self::Direct(_) => None,
            Self::Browser(window) => Some(window.surface_snapshot()),
        }
    }

    fn resize(
        &mut self,
        surface: SurfaceId,
        size: PhysicalSize,
    ) -> Result<(), AttachedWindowError> {
        match self {
            Self::Direct(window) => window
                .resize(surface, size)
                .map_err(AttachedWindowError::Direct),
            Self::Browser(window) => {
                let expected = window.surface_snapshot();
                if expected.descriptor().id != surface {
                    return Err(AttachedWindowError::WrongSurface);
                }
                window
                    .resize(WebRenderWindowResizeRequest::new(expected, size))
                    .map(|_| ())
                    .map_err(AttachedWindowError::Browser)
            }
        }
    }

    fn update_scale(
        &mut self,
        surface: SurfaceId,
        scale: ScaleFactor,
    ) -> Result<(), AttachedWindowError> {
        match self {
            Self::Direct(window) => window
                .update_scale(surface, scale)
                .map_err(AttachedWindowError::Direct),
            Self::Browser(window) => {
                let expected = window.surface_snapshot();
                if expected.descriptor().id != surface {
                    return Err(AttachedWindowError::WrongSurface);
                }
                window
                    .update_scale(expected, scale)
                    .map(|_| ())
                    .map_err(AttachedWindowError::Browser)
            }
        }
    }

    fn suspend(&mut self) -> Result<(), AttachedWindowError> {
        match self {
            Self::Direct(window) => window.suspend().map_err(AttachedWindowError::Direct),
            Self::Browser(window) => window
                .suspend(window.surface_snapshot())
                .map(|_| ())
                .map_err(AttachedWindowError::Browser),
        }
    }

    fn resume(&mut self) -> Result<(), AttachedWindowError> {
        match self {
            Self::Direct(window) => window.resume().map_err(AttachedWindowError::Direct),
            Self::Browser(window) => window
                .resume(window.surface_snapshot())
                .map(|_| ())
                .map_err(AttachedWindowError::Browser),
        }
    }
}

enum AttachedWindowError {
    Direct(PresentationError),
    Browser(WebRenderWindowError),
    WrongSurface,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WinitSurfaceActivity {
    AwaitingFirstResume,
    Active,
    ExplicitlySuspended,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PresenterLifecycleAction {
    Suspend,
    Resume,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WinitSurfaceTransition {
    Suppressed,
    Deliver(Option<PresenterLifecycleAction>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct NativeSurfaceLifecycle {
    activity: WinitSurfaceActivity,
    presenter_suspended: bool,
}

impl Default for NativeSurfaceLifecycle {
    fn default() -> Self {
        Self {
            activity: WinitSurfaceActivity::AwaitingFirstResume,
            presenter_suspended: false,
        }
    }
}

impl NativeSurfaceLifecycle {
    const fn resumed(&mut self, size: Option<PhysicalSize>) -> WinitSurfaceTransition {
        match self.activity {
            WinitSurfaceActivity::Active => WinitSurfaceTransition::Suppressed,
            WinitSurfaceActivity::AwaitingFirstResume => {
                self.activity = WinitSurfaceActivity::Active;
                WinitSurfaceTransition::Deliver(None)
            }
            WinitSurfaceActivity::ExplicitlySuspended => {
                self.activity = WinitSurfaceActivity::Active;
                let drawable = match size {
                    Some(size) => size.width != 0 && size.height != 0,
                    None => false,
                };
                let action = if self.presenter_suspended && drawable {
                    Some(PresenterLifecycleAction::Resume)
                } else {
                    None
                };
                WinitSurfaceTransition::Deliver(action)
            }
        }
    }

    const fn suspended(&mut self) -> WinitSurfaceTransition {
        if matches!(self.activity, WinitSurfaceActivity::ExplicitlySuspended) {
            return WinitSurfaceTransition::Suppressed;
        }
        self.activity = WinitSurfaceActivity::ExplicitlySuspended;
        let action = if self.presenter_suspended {
            None
        } else {
            Some(PresenterLifecycleAction::Suspend)
        };
        WinitSurfaceTransition::Deliver(action)
    }

    const fn resized(&mut self, size: PhysicalSize) -> Option<PresenterLifecycleAction> {
        self.presenter_suspended = size.width == 0 || size.height == 0;
        if matches!(self.activity, WinitSurfaceActivity::ExplicitlySuspended)
            && !self.presenter_suspended
        {
            Some(PresenterLifecycleAction::Suspend)
        } else {
            None
        }
    }

    const fn presenter_action_completed(&mut self, action: PresenterLifecycleAction) {
        self.presenter_suspended = matches!(action, PresenterLifecycleAction::Suspend);
    }

    const fn presenter_created(&mut self, size: PhysicalSize) {
        self.presenter_suspended = size.width == 0 || size.height == 0;
    }
}

impl AttachedWindowError {
    fn stop_reason(&self) -> LinuxStopReason {
        match self {
            Self::Direct(error) => LinuxStopReason::PresentationFailed(error.stage()),
            Self::Browser(error) => LinuxStopReason::BrowserPresentationFailed(error.stage()),
            Self::WrongSurface => LinuxStopReason::SurfaceIdentityViolation,
        }
    }
}

impl LinuxWakeHandle {
    /// Queues at most one outstanding wake and never transports application data.
    #[must_use]
    pub fn wake(&self) -> LinuxWakeStatus {
        match self.gate.admit() {
            WakeAdmission::AlreadyPending => LinuxWakeStatus::AlreadyPending,
            WakeAdmission::Closed => LinuxWakeStatus::Closed,
            WakeAdmission::Admitted => {
                if self.proxy.send_event(WakeEvent::Wake).is_err() {
                    self.gate.close();
                    LinuxWakeStatus::Closed
                } else {
                    LinuxWakeStatus::Queued
                }
            }
        }
    }
}

/// Callback-scoped controls for the live top-level window.
pub struct LinuxWindowControl<'a> {
    window: Option<&'a mut AttachedWindow>,
    close_cancelled: Option<&'a mut bool>,
    requested_stop: &'a mut Option<LinuxStopReason>,
    resize_request: &'a Cell<CallbackResizeRequest>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum CallbackResizeRequest {
    #[default]
    NotRequested,
    AwaitNativeEvent,
    SynchronouslyApplied(PhysicalSize),
}

fn reserve_callback_resize_request(
    resize_request: &Cell<CallbackResizeRequest>,
) -> Result<(), ControlError> {
    if !matches!(resize_request.get(), CallbackResizeRequest::NotRequested) {
        return Err(ControlError::InnerSizeAlreadyRequested);
    }
    resize_request.set(CallbackResizeRequest::AwaitNativeEvent);
    Ok(())
}

fn record_callback_resize_response(
    resize_request: &Cell<CallbackResizeRequest>,
    applied: Option<PhysicalSize>,
) {
    debug_assert!(matches!(
        resize_request.get(),
        CallbackResizeRequest::AwaitNativeEvent
    ));
    if let Some(applied) = applied {
        resize_request.set(CallbackResizeRequest::SynchronouslyApplied(applied));
    }
}

fn latch_browser_presentation_stop(
    requested_stop: &mut Option<LinuxStopReason>,
    stage: WebRenderWindowFailureStage,
    terminal: bool,
) {
    if terminal && requested_stop.is_none() {
        *requested_stop = Some(LinuxStopReason::BrowserPresentationFailed(stage));
    }
}

impl LinuxWindowControl<'_> {
    /// Requests a redraw event without performing rendering.
    pub fn request_redraw(&self) -> Result<(), ControlError> {
        let window = self.window.as_deref().ok_or(ControlError::NoLiveWindow)?;
        window.request_redraw();
        Ok(())
    }

    /// Requests a native client-area size at most once during this callback.
    ///
    /// A synchronous winit result is reconciled through the shell's canonical
    /// resize path after the callback returns. `None` means only a later native
    /// `Resized` event may mutate EGL/WebRender state.
    pub fn request_inner_size(
        &self,
        size: PhysicalSize,
    ) -> Result<Option<PhysicalSize>, ControlError> {
        let window = self.window.as_deref().ok_or(ControlError::NoLiveWindow)?;
        reserve_callback_resize_request(self.resize_request)?;
        let applied = window.request_inner_size(size);
        record_callback_resize_response(self.resize_request, applied);
        Ok(applied)
    }

    /// Enables or disables delivery of native IME events.
    pub fn set_ime_allowed(&self, allowed: bool) -> Result<(), ControlError> {
        let window = self.window.as_deref().ok_or(ControlError::NoLiveWindow)?;
        window.set_ime_allowed(allowed);
        Ok(())
    }

    /// Updates the logical rectangle used to position an IME candidate window.
    pub fn set_ime_cursor_area(&self, area: LogicalRect) -> Result<(), ControlError> {
        let window = self.window.as_deref().ok_or(ControlError::NoLiveWindow)?;
        let origin = LogicalPoint::new(area.origin.x, area.origin.y)
            .map_err(|_| ControlError::InvalidImeCursorArea)?;
        let size = LogicalSize::new(area.size.width, area.size.height)
            .map_err(|_| ControlError::InvalidImeCursorArea)?;
        window.set_ime_cursor_area(LogicalRect { origin, size });
        Ok(())
    }

    /// Draws one bounded Wild Buzzard-owned frame directly into the native
    /// EGL back buffer and submits its swap on the owner thread.
    ///
    /// A native/GL failure seals the shell after this callback. The receipt
    /// proves draw verification and successful swap submission, not desktop
    /// compositor acknowledgement.
    pub fn submit_solid_frame(
        &mut self,
        frame: SolidColorFrame,
    ) -> Result<SwapSubmissionReceipt, ControlError> {
        let window = self
            .window
            .as_deref_mut()
            .ok_or(ControlError::NoLiveWindow)?;
        let AttachedWindow::Direct(window) = window else {
            return Err(ControlError::WrongPresentationMode);
        };
        match window.submit_solid_frame(frame) {
            Ok(receipt) => Ok(receipt),
            Err(error) => {
                if error.is_terminal() && self.requested_stop.is_none() {
                    *self.requested_stop = Some(LinuxStopReason::PresentationFailed(error.stage()));
                }
                Err(ControlError::PresentationFailed {
                    stage: error.stage(),
                    kind: error.kind(),
                })
            }
        }
    }

    /// Exact value-only browser-compositor surface snapshot.
    pub fn browser_surface_snapshot(&self) -> Result<WebRenderSurfaceSnapshot, ControlError> {
        self.window
            .as_deref()
            .ok_or(ControlError::NoLiveWindow)?
            .surface_snapshot()
            .ok_or(ControlError::WrongPresentationMode)
    }

    /// Submits one immutable page/chrome composition to the same native
    /// surface. Inputs are consumed on every result.
    pub fn submit_browser_frame(
        &mut self,
        page: BrowserPageUpdate,
        chrome: Option<BrowserChromeScene>,
        request: BrowserFrameRequest,
    ) -> Result<BrowserFrameReceipt, ControlError> {
        let window = self
            .window
            .as_deref_mut()
            .ok_or(ControlError::NoLiveWindow)?;
        let AttachedWindow::Browser(window) = window else {
            return Err(ControlError::WrongPresentationMode);
        };
        match window.submit_browser_frame(page, chrome, request) {
            Ok(receipt) => Ok(receipt),
            Err(error) => {
                latch_browser_presentation_stop(
                    self.requested_stop,
                    error.stage(),
                    error.is_terminal(),
                );
                Err(ControlError::BrowserPresentationFailed {
                    stage: error.stage(),
                    kind: error.kind(),
                    terminal: error.is_terminal(),
                })
            }
        }
    }

    /// Resolves a physical point only against the last successful exact
    /// browser receipt for `surface`.
    pub fn hit_test_browser(
        &mut self,
        point: PhysicalPoint,
        surface: WebRenderSurfaceSnapshot,
    ) -> Result<Option<BrowserHitTestResult>, ControlError> {
        let window = self.window.as_deref().ok_or(ControlError::NoLiveWindow)?;
        let AttachedWindow::Browser(window) = window else {
            return Err(ControlError::WrongPresentationMode);
        };
        match window.hit_test_browser(point, surface) {
            Ok(target) => Ok(target),
            Err(error) => {
                latch_browser_presentation_stop(
                    self.requested_stop,
                    error.stage(),
                    error.is_terminal(),
                );
                Err(ControlError::BrowserPresentationFailed {
                    stage: error.stage(),
                    kind: error.kind(),
                    terminal: error.is_terminal(),
                })
            }
        }
    }

    /// Cancels only the exact close intent currently being delivered.
    pub fn cancel_close(&mut self) -> Result<(), ControlError> {
        let cancelled = self
            .close_cancelled
            .as_deref_mut()
            .ok_or(ControlError::NotDeliveringCloseIntent)?;
        *cancelled = true;
        Ok(())
    }

    /// Requests orderly event-loop shutdown after the current callback.
    pub fn request_exit(&mut self) {
        if self.requested_stop.is_none() {
            *self.requested_stop = Some(LinuxStopReason::Requested);
        }
    }
}

/// Consumer of ordered, normalized events on the event-loop owner thread.
pub trait LinuxWindowHandler {
    /// Handles one event. The control object is valid only for this callback.
    fn handle_event(&mut self, event: LinuxWindowEvent, control: &mut LinuxWindowControl<'_>);
}

/// Validated owner of one winit event loop and one top-level window.
pub struct LinuxWindowShell {
    config: LinuxShellConfig,
    event_loop: EventLoop<WakeEvent>,
    wake_owner: WakeOwner,
    // Make the main-thread ownership requirement explicit even if winit's
    // internal auto-traits change later.
    not_send_or_sync: PhantomData<Rc<()>>,
}

impl LinuxWindowShell {
    /// Validates configuration and initializes the selected display backend.
    ///
    /// Winit requires this function to run on the process main thread and
    /// permits only one event loop per process.
    pub fn new(config: LinuxShellConfig) -> Result<Self, LinuxShellError> {
        let config = config.validate()?;
        let mut builder = EventLoop::<WakeEvent>::with_user_event();
        match config.backend {
            LinuxBackendPreference::Auto => {}
            LinuxBackendPreference::Wayland => {
                EventLoopBuilderExtWayland::with_wayland(&mut builder);
            }
            LinuxBackendPreference::X11 => {
                EventLoopBuilderExtX11::with_x11(&mut builder);
            }
        }
        let event_loop = builder
            .build()
            .map_err(|error| LinuxShellError::EventLoopCreation(error.to_string()))?;
        Ok(Self {
            config,
            event_loop,
            wake_owner: WakeOwner::new(),
            not_send_or_sync: PhantomData,
        })
    }

    /// Returns a payload-free wake handle suitable for another thread.
    #[must_use]
    pub fn wake_handle(&self) -> LinuxWakeHandle {
        LinuxWakeHandle {
            proxy: self.event_loop.create_proxy(),
            gate: self.wake_owner.gate(),
        }
    }

    /// Runs until an explicit request, uncancelled close, or terminal fault.
    pub fn run<H: LinuxWindowHandler>(
        self,
        handler: &mut H,
    ) -> Result<LinuxShutdownReport, LinuxShellError> {
        let Self {
            config,
            event_loop,
            wake_owner,
            not_send_or_sync: _,
        } = self;
        let mut application = ShellApplication::new(config, handler, wake_owner);
        event_loop
            .run_app(&mut application)
            .map_err(|error| LinuxShellError::EventLoopRun(error.to_string()))?;
        if let Some(error) = application.fatal_error.take() {
            return Err(error);
        }
        application
            .state
            .report()
            .ok_or(LinuxShellError::MissingShutdownReport)
    }
}

/// Failure to configure, initialize, or finish the native event loop.
#[derive(Debug)]
pub enum LinuxShellError {
    /// Caller configuration failed validation.
    Config(ConfigError),
    /// The X11 or Wayland event loop could not be initialized.
    EventLoopCreation(String),
    /// The native top-level window could not be created.
    WindowCreation(String),
    /// EGL presenter creation failed after or during native-window setup.
    PresentationCreation(PresentationError),
    /// Same-surface WebRender browser-compositor creation failed after
    /// consuming the direct native presenter.
    BrowserPresentationCreation(WebRenderWindowStartupFailure),
    /// Winit returned an event-loop execution error.
    EventLoopRun(String),
    /// Surface retirement violated the exactly-once lifecycle contract.
    SurfaceIdentityLifecycle,
    /// Winit returned without delivering the terminal callback.
    MissingShutdownReport,
}

impl fmt::Display for LinuxShellError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(error) => write!(formatter, "invalid Linux shell configuration: {error}"),
            Self::EventLoopCreation(message) => {
                write!(
                    formatter,
                    "failed to initialize Linux event loop: {message}"
                )
            }
            Self::WindowCreation(message) => {
                write!(formatter, "failed to create Linux window: {message}")
            }
            Self::PresentationCreation(error) => {
                write!(formatter, "failed to create Linux presenter: {error}")
            }
            Self::BrowserPresentationCreation(error) => {
                write!(
                    formatter,
                    "failed to create Linux browser compositor: {error}"
                )
            }
            Self::EventLoopRun(message) => {
                write!(formatter, "Linux event loop failed: {message}")
            }
            Self::SurfaceIdentityLifecycle => {
                formatter.write_str("surface identity was not retired exactly once")
            }
            Self::MissingShutdownReport => {
                formatter.write_str("Linux event loop returned without a shutdown report")
            }
        }
    }
}

impl Error for LinuxShellError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Config(error) => Some(error),
            Self::PresentationCreation(error) => Some(error),
            Self::BrowserPresentationCreation(error) => Some(error),
            Self::EventLoopCreation(_)
            | Self::WindowCreation(_)
            | Self::EventLoopRun(_)
            | Self::SurfaceIdentityLifecycle
            | Self::MissingShutdownReport => None,
        }
    }
}

impl From<ConfigError> for LinuxShellError {
    fn from(error: ConfigError) -> Self {
        Self::Config(error)
    }
}

struct ShellApplication<'handler, H> {
    config: LinuxShellConfig,
    handler: &'handler mut H,
    window: Option<AttachedWindow>,
    desired_surface: Option<SurfaceDescriptor>,
    surface_allocator: SurfaceIdAllocator,
    normalizer: Option<InputNormalizer>,
    state: ShellState,
    surface_lifecycle: NativeSurfaceLifecycle,
    wake_owner: WakeOwner,
    delivered_events: u64,
    ignored_native_events: u64,
    destroyed_delivered: bool,
    presentation_shutdown: LinuxPresentationShutdown,
    fatal_error: Option<LinuxShellError>,
}

struct WindowStartupFailure {
    error: Box<LinuxShellError>,
    reason: LinuxStopReason,
}

impl WindowStartupFailure {
    fn new(error: LinuxShellError, reason: LinuxStopReason) -> Self {
        Self {
            error: Box::new(error),
            reason,
        }
    }
}

fn apply_surface_size(
    mut descriptor: SurfaceDescriptor,
    size: PhysicalSize,
) -> (SurfaceDescriptor, LinuxWindowEvent) {
    descriptor.size = size;
    let event = LinuxWindowEvent::Resized {
        surface: descriptor.id,
        size,
        scale: descriptor.scale,
    };
    (descriptor, event)
}

fn apply_surface_scale(
    mut descriptor: SurfaceDescriptor,
    scale: ScaleFactor,
) -> (SurfaceDescriptor, LinuxWindowEvent) {
    descriptor.scale = scale;
    let event = LinuxWindowEvent::ScaleFactorChanged {
        surface: descriptor.id,
        scale,
        size: descriptor.size,
    };
    (descriptor, event)
}

fn surface_size_changed(descriptor: SurfaceDescriptor, size: PhysicalSize) -> bool {
    descriptor.size != size
}

fn surface_scale_changed(descriptor: SurfaceDescriptor, scale: ScaleFactor) -> bool {
    descriptor.scale != scale
}

impl<'handler, H: LinuxWindowHandler> ShellApplication<'handler, H> {
    fn new(config: LinuxShellConfig, handler: &'handler mut H, wake_owner: WakeOwner) -> Self {
        Self {
            surface_allocator: SurfaceIdAllocator::new(config.surface_namespace),
            state: ShellState::new(config.limits.event_capacity),
            surface_lifecycle: NativeSurfaceLifecycle::default(),
            config,
            handler,
            window: None,
            desired_surface: None,
            normalizer: None,
            wake_owner,
            delivered_events: 0,
            ignored_native_events: 0,
            destroyed_delivered: false,
            presentation_shutdown: LinuxPresentationShutdown::NotCreated,
            fatal_error: None,
        }
    }

    fn create_window(&mut self, event_loop: &ActiveEventLoop) -> Result<(), WindowStartupFailure> {
        let backend = if ActiveEventLoopExtWayland::is_wayland(event_loop) {
            LinuxBackend::Wayland
        } else {
            LinuxBackend::X11
        };
        let presentation_backend = match backend {
            LinuxBackend::Wayland => LinuxPresentationBackend::Wayland,
            LinuxBackend::X11 => LinuxPresentationBackend::X11,
        };
        let surface = self.surface_allocator.allocate().map_err(|_| {
            WindowStartupFailure::new(
                LinuxShellError::WindowCreation(
                    "surface identity exhausted before window admission".to_owned(),
                ),
                LinuxStopReason::SurfaceIdentityExhausted,
            )
        })?;
        let initial_size = WinitLogicalSize::new(
            self.config.initial_size.width,
            self.config.initial_size.height,
        );
        let title = self.config.title.clone();
        let application_id = self.config.application_id.clone();
        let desired_pixel_format = self.config.desired_pixel_format;
        let creation = prepare_and_attach(event_loop, presentation_backend, move |preparation| {
            let mut attributes = Window::default_attributes()
                .with_title(title)
                .with_inner_size(initial_size);
            attributes = match backend {
                LinuxBackend::Wayland => WindowAttributesExtWayland::with_name(
                    attributes,
                    application_id.clone(),
                    application_id.clone(),
                ),
                LinuxBackend::X11 => {
                    let visual = preparation.x11_visual_id().ok_or_else(|| {
                        WindowStartupFailure::new(
                            LinuxShellError::WindowCreation(
                                "prepared X11 presenter omitted its required visual".to_owned(),
                            ),
                            LinuxStopReason::PresentationFailed(
                                PresentationFailureStage::SelectConfig,
                            ),
                        )
                    })?;
                    WindowAttributesExtX11::with_x11_visual(
                        WindowAttributesExtX11::with_name(
                            attributes,
                            application_id.clone(),
                            application_id.clone(),
                        ),
                        visual,
                    )
                }
            };
            let window = event_loop.create_window(attributes).map_err(|error| {
                WindowStartupFailure::new(
                    LinuxShellError::WindowCreation(error.to_string()),
                    LinuxStopReason::WindowCreationFailed,
                )
            })?;
            let scale = scale_factor(window.scale_factor()).map_err(|_| {
                WindowStartupFailure::new(
                    LinuxShellError::WindowCreation(
                        "backend returned an invalid scale factor".to_owned(),
                    ),
                    LinuxStopReason::InvalidPlatformGeometry,
                )
            })?;
            let size = physical_size(window.inner_size()).map_err(|_| {
                WindowStartupFailure::new(
                    LinuxShellError::WindowCreation(
                        "backend returned an invalid initial size".to_owned(),
                    ),
                    LinuxStopReason::InvalidPlatformGeometry,
                )
            })?;
            let descriptor = SurfaceDescriptor {
                id: surface,
                size,
                scale,
                format: desired_pixel_format,
                role: SurfaceRole::Window,
            };
            Ok((window, descriptor))
        });
        let window = match creation {
            Ok(window) => window,
            Err(error) => {
                let failure = match error {
                    LinuxPresenterCreationError::Presentation(error) => {
                        let stage = error.stage();
                        WindowStartupFailure::new(
                            LinuxShellError::PresentationCreation(error),
                            LinuxStopReason::PresentationFailed(stage),
                        )
                    }
                    LinuxPresenterCreationError::PresentationWithTeardown(failure) => {
                        let (error, teardown) = failure.into_parts();
                        if teardown.surface() != surface {
                            WindowStartupFailure::new(
                                LinuxShellError::SurfaceIdentityLifecycle,
                                LinuxStopReason::SurfaceIdentityViolation,
                            )
                        } else {
                            self.presentation_shutdown = teardown.into();
                            let stage = error.stage();
                            WindowStartupFailure::new(
                                LinuxShellError::PresentationCreation(error),
                                LinuxStopReason::PresentationFailed(stage),
                            )
                        }
                    }
                    LinuxPresenterCreationError::Window(failure) => failure,
                };
                if self.surface_allocator.release(surface).is_err() {
                    return Err(WindowStartupFailure::new(
                        LinuxShellError::SurfaceIdentityLifecycle,
                        LinuxStopReason::SurfaceIdentityViolation,
                    ));
                }
                return Err(failure);
            }
        };
        let desired_surface = window.descriptor();
        let window = match self.config.presentation_mode {
            LinuxPresentationMode::DirectDiagnostic => AttachedWindow::Direct(Box::new(window)),
            LinuxPresentationMode::BrowserCompositor => {
                let browser = match window.into_browser_compositor() {
                    Ok(browser) => browser,
                    Err(failure) => {
                        let stage = failure.primary().stage();
                        let shutdown_surface = match failure.presentation_teardown() {
                            wild_buzzard_linux_presenter::PresentationTeardownOutcome::WrappersReleased(
                                report,
                            ) => report.surface(),
                            wild_buzzard_linux_presenter::PresentationTeardownOutcome::RetainedAfterTeardownFailure(
                                report,
                            ) => report.surface(),
                        };
                        self.presentation_shutdown = match failure.teardown() {
                            Ok(report) => {
                                LinuxPresentationShutdown::BrowserWrappersReleased(*report)
                            }
                            Err(teardown) => LinuxPresentationShutdown::BrowserTeardownFailed(
                                LinuxBrowserShutdownFailure::from_failure(teardown),
                            ),
                        };
                        let release_ok = self.surface_allocator.release(surface).is_ok();
                        if shutdown_surface != surface || !release_ok {
                            return Err(WindowStartupFailure::new(
                                LinuxShellError::SurfaceIdentityLifecycle,
                                LinuxStopReason::SurfaceIdentityViolation,
                            ));
                        }
                        return Err(WindowStartupFailure::new(
                            LinuxShellError::BrowserPresentationCreation(failure),
                            LinuxStopReason::BrowserPresentationFailed(stage),
                        ));
                    }
                };
                AttachedWindow::Browser(Box::new(browser))
            }
        };
        self.normalizer = Some(InputNormalizer::new(
            surface,
            desired_surface.scale,
            self.config.limits,
        ));
        self.desired_surface = Some(desired_surface);
        self.window = Some(window);
        self.surface_lifecycle
            .presenter_created(desired_surface.size);
        self.enqueue(
            LinuxWindowEvent::Ready {
                backend,
                desired_surface,
            },
            event_loop,
        );
        if self.state.is_running()
            && let Some(window) = self.window.as_ref()
        {
            window.request_redraw();
        }
        Ok(())
    }

    fn enqueue(&mut self, event: LinuxWindowEvent, event_loop: &ActiveEventLoop) {
        if !self.state.is_running() {
            return;
        }
        match self.state.push(event) {
            Ok(()) => {}
            Err(PushError::Saturated { capacity }) => {
                self.begin_stop(
                    LinuxStopReason::EventQueueSaturated { capacity },
                    event_loop,
                );
            }
            Err(PushError::Sealed) => {
                // Lifecycle and queue admission are advanced together. If
                // they ever diverge, fail closed instead of reopening input.
                self.begin_stop(LinuxStopReason::BackendExited, event_loop);
            }
        }
    }

    fn enqueue_batch(&mut self, batch: InputBatch, event_loop: &ActiveEventLoop) {
        if !self.state.is_running() {
            return;
        }
        if batch.is_empty() {
            self.ignored_native_events = self.ignored_native_events.saturating_add(1);
            return;
        }
        for normalized in batch {
            self.enqueue(
                LinuxWindowEvent::Input {
                    event: normalized.event,
                    origin: normalized.origin,
                },
                event_loop,
            );
            if !self.state.is_running() {
                break;
            }
        }
    }

    fn normalize_batch(
        &mut self,
        result: Result<InputBatch, LinuxStopReason>,
        event_loop: &ActiveEventLoop,
    ) {
        if !self.state.is_running() {
            return;
        }
        match result {
            Ok(batch) => self.enqueue_batch(batch, event_loop),
            Err(reason) => self.begin_stop(reason, event_loop),
        }
    }

    fn begin_stop(&mut self, reason: LinuxStopReason, event_loop: &ActiveEventLoop) {
        if self.state.begin_stopping(reason) {
            self.wake_owner.close();
            event_loop.exit();
        }
    }

    fn drain(&mut self, event_loop: &ActiveEventLoop) {
        while self.state.is_running() {
            let Some(event) = self.state.pop() else {
                break;
            };
            let delivering_close = matches!(event, LinuxWindowEvent::CloseRequested { .. });
            let mut close_cancelled = false;
            let mut requested_stop = None;
            let resize_request = Cell::new(CallbackResizeRequest::NotRequested);
            {
                let window = self.window.as_mut();
                let close_slot = delivering_close.then_some(&mut close_cancelled);
                let mut control = LinuxWindowControl {
                    window,
                    close_cancelled: close_slot,
                    requested_stop: &mut requested_stop,
                    resize_request: &resize_request,
                };
                self.handler.handle_event(event, &mut control);
            }
            self.delivered_events = self.delivered_events.saturating_add(1);

            if let Some(reason) = requested_stop {
                self.begin_stop(reason, event_loop);
                break;
            } else if delivering_close && !close_cancelled {
                self.begin_stop(LinuxStopReason::CloseRequested, event_loop);
                break;
            } else if let CallbackResizeRequest::SynchronouslyApplied(size) = resize_request.get() {
                self.update_size(
                    winit::dpi::PhysicalSize::new(size.width, size.height),
                    event_loop,
                );
            }
        }
    }

    fn current_surface(&self) -> Option<SurfaceId> {
        self.desired_surface.map(|surface| surface.id)
    }

    fn update_size(&mut self, size: winit::dpi::PhysicalSize<u32>, event_loop: &ActiveEventLoop) {
        let Ok(size) = physical_size(size) else {
            self.begin_stop(LinuxStopReason::InvalidPlatformGeometry, event_loop);
            return;
        };
        let Some(descriptor) = self.desired_surface else {
            self.ignored_native_events = self.ignored_native_events.saturating_add(1);
            return;
        };
        if !surface_size_changed(descriptor, size) {
            self.ignore_native_event();
            return;
        }
        let Some(window) = self.window.as_mut() else {
            self.ignore_native_event();
            return;
        };
        if let Err(error) = window.resize(descriptor.id, size) {
            self.begin_stop(error.stop_reason(), event_loop);
            return;
        }
        if let Some(action) = self.surface_lifecycle.resized(size) {
            debug_assert_eq!(action, PresenterLifecycleAction::Suspend);
            if let Err(error) = window.suspend() {
                self.begin_stop(error.stop_reason(), event_loop);
                return;
            }
            self.surface_lifecycle.presenter_action_completed(action);
        }
        let (descriptor, event) = apply_surface_size(descriptor, size);
        self.desired_surface = Some(descriptor);
        self.enqueue(event, event_loop);
    }

    fn update_scale(&mut self, value: f64, event_loop: &ActiveEventLoop) {
        let Ok(scale) = scale_factor(value) else {
            self.begin_stop(LinuxStopReason::InvalidPlatformGeometry, event_loop);
            return;
        };
        let Some(descriptor) = self.desired_surface else {
            self.ignored_native_events = self.ignored_native_events.saturating_add(1);
            return;
        };
        if !surface_scale_changed(descriptor, scale) {
            self.ignore_native_event();
            return;
        }
        let Some(window) = self.window.as_mut() else {
            self.ignore_native_event();
            return;
        };
        if let Err(error) = window.update_scale(descriptor.id, scale) {
            self.begin_stop(error.stop_reason(), event_loop);
            return;
        }
        let (descriptor, event) = apply_surface_scale(descriptor, scale);
        self.desired_surface = Some(descriptor);
        if let Some(normalizer) = self.normalizer.as_mut() {
            normalizer.set_scale(scale);
        }
        self.enqueue(event, event_loop);
    }

    fn destroy_window_and_deliver(&mut self) {
        self.normalizer.take();
        let Some(descriptor) = self.desired_surface.take() else {
            self.window.take();
            return;
        };
        let Some(window) = self.window.take() else {
            self.fatal_error = Some(LinuxShellError::SurfaceIdentityLifecycle);
            return;
        };
        let (shutdown_surface, released_normally) = match window {
            AttachedWindow::Direct(window) => match (*window).shutdown() {
                Ok(report) => {
                    self.presentation_shutdown =
                        LinuxPresentationShutdown::WrappersReleased(report);
                    (report.surface(), true)
                }
                Err(report) => {
                    self.presentation_shutdown =
                        LinuxPresentationShutdown::RetainedAfterTeardownFailure(report);
                    (report.surface(), false)
                }
            },
            AttachedWindow::Browser(window) => match (*window).shutdown() {
                Ok(report) => {
                    let surface = report.presentation().surface();
                    self.presentation_shutdown =
                        LinuxPresentationShutdown::BrowserWrappersReleased(report);
                    (surface, true)
                }
                Err(failure) => {
                    let presentation = failure.presentation();
                    let (surface, released) = match presentation {
                        wild_buzzard_linux_presenter::PresentationTeardownOutcome::WrappersReleased(
                            report,
                        ) => (report.surface(), true),
                        wild_buzzard_linux_presenter::PresentationTeardownOutcome::RetainedAfterTeardownFailure(
                            report,
                        ) => (report.surface(), false),
                    };
                    self.presentation_shutdown = LinuxPresentationShutdown::BrowserTeardownFailed(
                        LinuxBrowserShutdownFailure::from_failure(&failure),
                    );
                    (surface, released)
                }
            },
        };
        let identity_matches = shutdown_surface == descriptor.id;
        let identity_released = self.surface_allocator.release(descriptor.id).is_ok();
        if !identity_matches || !identity_released {
            self.fatal_error = Some(LinuxShellError::SurfaceIdentityLifecycle);
            return;
        }
        if released_normally && !self.destroyed_delivered {
            self.destroyed_delivered = true;
            let mut requested_stop = None;
            let resize_request = Cell::new(CallbackResizeRequest::NotRequested);
            let mut control = LinuxWindowControl {
                window: None,
                close_cancelled: None,
                requested_stop: &mut requested_stop,
                resize_request: &resize_request,
            };
            self.handler.handle_event(
                LinuxWindowEvent::Destroyed {
                    surface: descriptor.id,
                },
                &mut control,
            );
            self.delivered_events = self.delivered_events.saturating_add(1);
        }
    }

    fn deliver_terminal(&mut self) {
        if self.state.report().is_some() {
            return;
        }
        let Some(reason) = self.state.stop_reason() else {
            return;
        };
        let report = LinuxShutdownReport {
            reason,
            delivered_events: self.delivered_events,
            coalesced_events: self.state.coalesced(),
            ignored_native_events: self.ignored_native_events,
            presentation: self.presentation_shutdown,
        };
        if !self.state.finish(report) {
            return;
        }
        let mut requested_stop = None;
        let resize_request = Cell::new(CallbackResizeRequest::NotRequested);
        let mut control = LinuxWindowControl {
            window: None,
            close_cancelled: None,
            requested_stop: &mut requested_stop,
            resize_request: &resize_request,
        };
        self.handler
            .handle_event(LinuxWindowEvent::Stopped(report), &mut control);
    }

    fn ignore_native_event(&mut self) {
        self.ignored_native_events = self.ignored_native_events.saturating_add(1);
    }
}

impl<H: LinuxWindowHandler> ApplicationHandler<WakeEvent> for ShellApplication<'_, H> {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        event_loop.set_control_flow(ControlFlow::Wait);
        if !self.state.is_running() {
            return;
        }
        let transition = self
            .surface_lifecycle
            .resumed(self.desired_surface.map(|surface| surface.size));
        let WinitSurfaceTransition::Deliver(action) = transition else {
            self.ignore_native_event();
            return;
        };
        if let Some(action) = action
            && let Some(window) = self.window.as_mut()
        {
            debug_assert_eq!(action, PresenterLifecycleAction::Resume);
            if let Err(error) = window.resume() {
                self.begin_stop(error.stop_reason(), event_loop);
                return;
            }
            self.surface_lifecycle.presenter_action_completed(action);
        }
        self.enqueue(LinuxWindowEvent::Resumed, event_loop);
        self.drain(event_loop);
        if !self.state.is_running() {
            return;
        }
        if self.window.is_none()
            && let Err(failure) = self.create_window(event_loop)
        {
            self.fatal_error = Some(*failure.error);
            self.begin_stop(failure.reason, event_loop);
        }
        self.drain(event_loop);
    }

    fn suspended(&mut self, event_loop: &ActiveEventLoop) {
        if !self.state.is_running() {
            return;
        }
        let transition = self.surface_lifecycle.suspended();
        let WinitSurfaceTransition::Deliver(action) = transition else {
            self.ignore_native_event();
            return;
        };
        if let Some(action) = action
            && let Some(window) = self.window.as_mut()
        {
            debug_assert_eq!(action, PresenterLifecycleAction::Suspend);
            if let Err(error) = window.suspend() {
                self.begin_stop(error.stop_reason(), event_loop);
                return;
            }
            self.surface_lifecycle.presenter_action_completed(action);
        }
        self.enqueue(LinuxWindowEvent::Suspended, event_loop);
        self.drain(event_loop);
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: WakeEvent) {
        let WakeEvent::Wake = event;
        if !self.wake_owner.gate().acknowledge() || !self.state.is_running() {
            return;
        }
        self.enqueue(LinuxWindowEvent::WakeRequested, event_loop);
        self.drain(event_loop);
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        if !self.state.is_running() {
            return;
        }
        let Some(window) = self.window.as_ref() else {
            self.ignore_native_event();
            return;
        };
        if !window.matches_window_id(window_id) {
            self.ignore_native_event();
            return;
        }

        match event {
            WindowEvent::Resized(size) => self.update_size(size, event_loop),
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                self.update_scale(scale_factor, event_loop);
            }
            WindowEvent::Focused(focused) => {
                if !focused && let Some(normalizer) = self.normalizer.as_mut() {
                    normalizer.focus_lost();
                }
                if let Some(surface) = self.current_surface() {
                    self.enqueue(
                        LinuxWindowEvent::FocusChanged { surface, focused },
                        event_loop,
                    );
                } else {
                    self.ignore_native_event();
                }
            }
            WindowEvent::KeyboardInput {
                device_id,
                event,
                is_synthetic,
            } => {
                let result = self.normalizer.as_mut().map(|normalizer| {
                    normalizer.keyboard(
                        device_id,
                        event.physical_key,
                        event.state,
                        event.location,
                        event.repeat,
                        is_synthetic,
                    )
                });
                match result {
                    Some(result) => self.normalize_batch(result, event_loop),
                    None => self.ignore_native_event(),
                }
                // `event.text` is deliberately not converted into a text
                // commit. Only Ime::Commit enters TextInputEvent.
            }
            WindowEvent::ModifiersChanged(modifiers) => {
                if let Some(normalizer) = self.normalizer.as_mut() {
                    normalizer.modifiers_changed(modifiers.state());
                } else {
                    self.ignore_native_event();
                }
            }
            WindowEvent::CursorEntered { device_id } => {
                let result = self
                    .normalizer
                    .as_mut()
                    .map(|normalizer| normalizer.cursor_entered(device_id));
                match result {
                    Some(Ok(())) => {}
                    Some(Err(reason)) => self.begin_stop(reason, event_loop),
                    None => self.ignore_native_event(),
                }
            }
            WindowEvent::CursorMoved {
                device_id,
                position,
            } => {
                let result = self
                    .normalizer
                    .as_mut()
                    .map(|normalizer| normalizer.cursor_moved(device_id, position));
                match result {
                    Some(result) => self.normalize_batch(result, event_loop),
                    None => self.ignore_native_event(),
                }
            }
            WindowEvent::CursorLeft { device_id } => {
                let result = self
                    .normalizer
                    .as_mut()
                    .map(|normalizer| normalizer.cursor_left(device_id));
                match result {
                    Some(result) => self.normalize_batch(result, event_loop),
                    None => self.ignore_native_event(),
                }
            }
            WindowEvent::MouseInput {
                device_id,
                state,
                button,
            } => {
                let result = self
                    .normalizer
                    .as_mut()
                    .map(|normalizer| normalizer.mouse_button(device_id, state, button));
                match result {
                    Some(result) => self.normalize_batch(result, event_loop),
                    None => self.ignore_native_event(),
                }
            }
            WindowEvent::MouseWheel {
                device_id,
                delta,
                phase,
            } => {
                let result = self
                    .normalizer
                    .as_mut()
                    .map(|normalizer| normalizer.mouse_wheel(device_id, delta, phase));
                match result {
                    Some(result) => self.normalize_batch(result, event_loop),
                    None => self.ignore_native_event(),
                }
            }
            WindowEvent::Touch(touch) => {
                let result = self
                    .normalizer
                    .as_mut()
                    .map(|normalizer| normalizer.touch(touch));
                match result {
                    Some(result) => self.normalize_batch(result, event_loop),
                    None => self.ignore_native_event(),
                }
            }
            WindowEvent::Ime(ime) => {
                let Some(surface) = self.current_surface() else {
                    self.ignore_native_event();
                    self.drain(event_loop);
                    return;
                };
                match ime {
                    Ime::Enabled => {
                        self.enqueue(LinuxWindowEvent::ImeEnabled { surface }, event_loop);
                    }
                    Ime::Preedit(text, selection) => {
                        let result = self
                            .normalizer
                            .as_ref()
                            .map(|normalizer| normalizer.ime_preedit(text, selection));
                        match result {
                            Some(Ok(preedit)) => self.enqueue(
                                LinuxWindowEvent::ImePreedit { surface, preedit },
                                event_loop,
                            ),
                            Some(Err(reason)) => self.begin_stop(reason, event_loop),
                            None => self.ignore_native_event(),
                        }
                    }
                    Ime::Commit(text) => {
                        let result = self
                            .normalizer
                            .as_mut()
                            .map(|normalizer| normalizer.ime_commit(text));
                        match result {
                            Some(result) => self.normalize_batch(result, event_loop),
                            None => self.ignore_native_event(),
                        }
                    }
                    Ime::Disabled => {
                        self.enqueue(LinuxWindowEvent::ImeDisabled { surface }, event_loop);
                    }
                }
            }
            WindowEvent::RedrawRequested => {
                if let Some(surface) = self.current_surface() {
                    self.enqueue(LinuxWindowEvent::RedrawRequested { surface }, event_loop);
                } else {
                    self.ignore_native_event();
                }
            }
            WindowEvent::CloseRequested => {
                if let Some(surface) = self.current_surface() {
                    self.enqueue(LinuxWindowEvent::CloseRequested { surface }, event_loop);
                } else {
                    self.ignore_native_event();
                }
            }
            WindowEvent::Destroyed => {
                if self.desired_surface.is_some() {
                    self.begin_stop(LinuxStopReason::WindowDestroyed, event_loop);
                } else {
                    self.ignore_native_event();
                }
            }
            _ => self.ignore_native_event(),
        }
        self.drain(event_loop);
    }

    fn device_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        device_id: DeviceId,
        event: DeviceEvent,
    ) {
        if !self.state.is_running() {
            return;
        }
        if matches!(event, DeviceEvent::Removed)
            && let Some(normalizer) = self.normalizer.as_mut()
        {
            normalizer.device_removed(device_id);
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self.state.is_running() {
            self.drain(event_loop);
        }
    }

    fn exiting(&mut self, event_loop: &ActiveEventLoop) {
        if self.state.is_running() {
            self.begin_stop(LinuxStopReason::BackendExited, event_loop);
        }
        self.wake_owner.close();
        self.destroy_window_and_deliver();
        self.deliver_terminal();
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CallbackResizeRequest, ControlError, LinuxWindowControl, NativeSurfaceLifecycle,
        PresenterLifecycleAction, WinitSurfaceActivity, WinitSurfaceTransition,
        apply_surface_scale, apply_surface_size, latch_browser_presentation_stop,
        record_callback_resize_response, reserve_callback_resize_request, surface_scale_changed,
        surface_size_changed,
    };
    use crate::event::{LinuxStopReason, LinuxWindowEvent};
    use std::cell::Cell;
    use wild_buzzard_linux_presenter::WebRenderWindowFailureStage;
    use wild_buzzard_platform::{
        PhysicalSize, PixelFormat, ScaleFactor, SurfaceDescriptor, SurfaceIdAllocator,
        SurfaceNamespace, SurfaceRole,
    };

    fn surface() -> SurfaceDescriptor {
        let mut allocator = SurfaceIdAllocator::new(SurfaceNamespace::new(73).unwrap());
        SurfaceDescriptor {
            id: allocator.allocate().unwrap(),
            size: PhysicalSize::new(800, 600).unwrap(),
            scale: ScaleFactor::new(1.0).unwrap(),
            format: PixelFormat::Rgba8Srgb,
            role: SurfaceRole::Window,
        }
    }

    #[test]
    fn callback_resize_response_is_exact_and_a_second_request_is_rejected() {
        let resize_request = Cell::new(CallbackResizeRequest::NotRequested);
        reserve_callback_resize_request(&resize_request).unwrap();
        record_callback_resize_response(&resize_request, None);
        assert_eq!(
            resize_request.get(),
            CallbackResizeRequest::AwaitNativeEvent
        );
        assert_eq!(
            reserve_callback_resize_request(&resize_request),
            Err(ControlError::InnerSizeAlreadyRequested)
        );

        let applied = PhysicalSize::new(640, 480).unwrap();
        let resize_request = Cell::new(CallbackResizeRequest::NotRequested);
        reserve_callback_resize_request(&resize_request).unwrap();
        record_callback_resize_response(&resize_request, Some(applied));
        assert_eq!(
            resize_request.get(),
            CallbackResizeRequest::SynchronouslyApplied(applied)
        );

        let (updated, event) = apply_surface_size(surface(), applied);
        assert!(matches!(
            event,
            LinuxWindowEvent::Resized { size, .. } if size == applied
        ));
        assert!(!surface_size_changed(updated, applied));
    }

    #[test]
    fn close_cancellation_is_invalid_without_exact_close_delivery() {
        let mut requested_stop = None;
        let resize_request = Cell::new(CallbackResizeRequest::NotRequested);
        let mut control = LinuxWindowControl {
            window: None,
            close_cancelled: None,
            requested_stop: &mut requested_stop,
            resize_request: &resize_request,
        };
        assert_eq!(
            control.cancel_close(),
            Err(ControlError::NotDeliveringCloseIntent)
        );
        control.request_exit();
        assert_eq!(requested_stop, Some(LinuxStopReason::Requested));
    }

    #[test]
    fn exact_close_delivery_can_be_cancelled_once_scoped() {
        let mut cancelled = false;
        let mut requested_stop = None;
        let resize_request = Cell::new(CallbackResizeRequest::NotRequested);
        {
            let mut control = LinuxWindowControl {
                window: None,
                close_cancelled: Some(&mut cancelled),
                requested_stop: &mut requested_stop,
                resize_request: &resize_request,
            };
            control.cancel_close().unwrap();
        }
        assert!(cancelled);
        assert_eq!(requested_stop, None);
    }

    #[test]
    fn terminal_browser_hit_test_failure_latches_its_exact_native_stop_stage() {
        let stage = WebRenderWindowFailureStage::RenderFrame;
        let mut requested_stop = None;
        latch_browser_presentation_stop(&mut requested_stop, stage, false);
        assert_eq!(requested_stop, None);
        latch_browser_presentation_stop(&mut requested_stop, stage, true);
        assert_eq!(
            requested_stop,
            Some(LinuxStopReason::BrowserPresentationFailed(stage))
        );

        let retained = WebRenderWindowFailureStage::SwapBuffers;
        latch_browser_presentation_stop(&mut requested_stop, retained, true);
        assert_eq!(
            requested_stop,
            Some(LinuxStopReason::BrowserPresentationFailed(stage)),
            "the first terminal stage remains authoritative",
        );
    }

    #[test]
    fn scale_change_preserves_current_known_size_without_fabricating_resize() {
        let initial = surface();
        let scale = ScaleFactor::new(2.0).unwrap();
        let (scaled, event) = apply_surface_scale(initial, scale);

        assert_eq!(scaled.size, initial.size);
        assert_eq!(scaled.scale, scale);
        assert_eq!(
            event,
            LinuxWindowEvent::ScaleFactorChanged {
                surface: initial.id,
                scale,
                size: initial.size,
            }
        );
    }

    #[test]
    fn native_resize_after_scale_change_publishes_new_size_at_current_scale() {
        let initial = surface();
        let scale = ScaleFactor::new(2.0).unwrap();
        let (scaled, _) = apply_surface_scale(initial, scale);
        let size = PhysicalSize::new(1_200, 900).unwrap();
        let (resized, event) = apply_surface_size(scaled, size);

        assert_eq!(resized.size, size);
        assert_eq!(resized.scale, scale);
        assert_eq!(
            event,
            LinuxWindowEvent::Resized {
                surface: initial.id,
                size,
                scale,
            }
        );
    }

    #[test]
    fn exact_duplicate_size_and_scale_callbacks_are_not_material_transitions() {
        let initial = surface();
        assert!(!surface_size_changed(initial, initial.size));
        assert!(!surface_scale_changed(initial, initial.scale));
        assert!(surface_size_changed(
            initial,
            PhysicalSize::new(801, 600).unwrap()
        ));
        assert!(surface_scale_changed(
            initial,
            ScaleFactor::new(2.0).unwrap()
        ));
    }

    #[test]
    fn explicit_and_zero_size_suspension_overlap_is_idempotent_and_ordered() {
        let nonzero = PhysicalSize::new(800, 600).unwrap();
        let zero = PhysicalSize::new(0, 600).unwrap();
        let mut lifecycle = NativeSurfaceLifecycle::default();
        let mut delivered = 0;

        assert_eq!(
            lifecycle.resumed(None),
            WinitSurfaceTransition::Deliver(None)
        );
        delivered += 1;
        lifecycle.presenter_created(nonzero);
        assert_eq!(lifecycle.resized(zero), None);
        assert!(lifecycle.presenter_suspended);
        assert_eq!(lifecycle.suspended(), WinitSurfaceTransition::Deliver(None));
        delivered += 1;
        assert_eq!(lifecycle.suspended(), WinitSurfaceTransition::Suppressed);
        assert_eq!(
            lifecycle.resized(nonzero),
            Some(PresenterLifecycleAction::Suspend)
        );
        lifecycle.presenter_action_completed(PresenterLifecycleAction::Suspend);
        assert_eq!(
            lifecycle.resumed(Some(nonzero)),
            WinitSurfaceTransition::Deliver(Some(PresenterLifecycleAction::Resume))
        );
        lifecycle.presenter_action_completed(PresenterLifecycleAction::Resume);
        delivered += 1;
        assert_eq!(
            lifecycle.resumed(Some(nonzero)),
            WinitSurfaceTransition::Suppressed
        );

        assert_eq!(delivered, 3);
        assert_eq!(lifecycle.activity, WinitSurfaceActivity::Active);
        assert!(!lifecycle.presenter_suspended);
    }

    #[test]
    fn resume_at_zero_delivers_model_transition_and_defers_presenter_resume() {
        let nonzero = PhysicalSize::new(800, 600).unwrap();
        let zero = PhysicalSize::new(800, 0).unwrap();
        let mut lifecycle = NativeSurfaceLifecycle::default();
        assert_eq!(
            lifecycle.resumed(None),
            WinitSurfaceTransition::Deliver(None)
        );
        lifecycle.presenter_created(nonzero);
        assert_eq!(lifecycle.resized(zero), None);
        assert_eq!(lifecycle.suspended(), WinitSurfaceTransition::Deliver(None));
        assert_eq!(
            lifecycle.resumed(Some(zero)),
            WinitSurfaceTransition::Deliver(None)
        );
        assert!(lifecycle.presenter_suspended);
        assert_eq!(lifecycle.resized(nonzero), None);
        assert!(!lifecycle.presenter_suspended);
        assert_eq!(lifecycle.activity, WinitSurfaceActivity::Active);
    }
}
