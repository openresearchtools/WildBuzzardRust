use std::error::Error;
use std::fmt;
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::Arc;

use wild_buzzard_platform::{
    LogicalPoint, LogicalRect, LogicalSize, PhysicalSize, ScaleFactor, SurfaceDescriptor,
    SurfaceId, SurfaceIdAllocator, SurfaceRole,
};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalPosition as WinitLogicalPosition, LogicalSize as WinitLogicalSize};
use winit::event::{DeviceEvent, DeviceId, Ime, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::platform::wayland::{
    ActiveEventLoopExtWayland, EventLoopBuilderExtWayland, WindowAttributesExtWayland,
};
use winit::platform::x11::{EventLoopBuilderExtX11, WindowAttributesExtX11};
use winit::window::{Window, WindowId};

use crate::config::{ConfigError, LinuxBackendPreference, LinuxShellConfig};
use crate::event::{
    ControlError, LinuxBackend, LinuxShutdownReport, LinuxStopReason, LinuxWindowEvent,
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
    window: Option<&'a Window>,
    close_cancelled: Option<&'a mut bool>,
    requested_stop: &'a mut Option<LinuxStopReason>,
}

impl LinuxWindowControl<'_> {
    /// Requests a redraw event without performing rendering.
    pub fn request_redraw(&self) -> Result<(), ControlError> {
        let window = self.window.ok_or(ControlError::NoLiveWindow)?;
        window.request_redraw();
        Ok(())
    }

    /// Enables or disables delivery of native IME events.
    pub fn set_ime_allowed(&self, allowed: bool) -> Result<(), ControlError> {
        let window = self.window.ok_or(ControlError::NoLiveWindow)?;
        window.set_ime_allowed(allowed);
        Ok(())
    }

    /// Updates the logical rectangle used to position an IME candidate window.
    pub fn set_ime_cursor_area(&self, area: LogicalRect) -> Result<(), ControlError> {
        let window = self.window.ok_or(ControlError::NoLiveWindow)?;
        let origin = LogicalPoint::new(area.origin.x, area.origin.y)
            .map_err(|_| ControlError::InvalidImeCursorArea)?;
        let size = LogicalSize::new(area.size.width, area.size.height)
            .map_err(|_| ControlError::InvalidImeCursorArea)?;
        window.set_ime_cursor_area(
            WinitLogicalPosition::new(origin.x, origin.y),
            WinitLogicalSize::new(size.width, size.height),
        );
        Ok(())
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
    window: Option<Window>,
    desired_surface: Option<SurfaceDescriptor>,
    surface_allocator: SurfaceIdAllocator,
    normalizer: Option<InputNormalizer>,
    state: ShellState,
    wake_owner: WakeOwner,
    delivered_events: u64,
    ignored_native_events: u64,
    destroyed_delivered: bool,
    fatal_error: Option<LinuxShellError>,
}

struct WindowStartupFailure {
    error: LinuxShellError,
    reason: LinuxStopReason,
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

impl<'handler, H: LinuxWindowHandler> ShellApplication<'handler, H> {
    fn new(config: LinuxShellConfig, handler: &'handler mut H, wake_owner: WakeOwner) -> Self {
        Self {
            surface_allocator: SurfaceIdAllocator::new(config.surface_namespace),
            state: ShellState::new(config.limits.event_capacity),
            config,
            handler,
            window: None,
            desired_surface: None,
            normalizer: None,
            wake_owner,
            delivered_events: 0,
            ignored_native_events: 0,
            destroyed_delivered: false,
            fatal_error: None,
        }
    }

    fn create_window(&mut self, event_loop: &ActiveEventLoop) -> Result<(), WindowStartupFailure> {
        let backend = if ActiveEventLoopExtWayland::is_wayland(event_loop) {
            LinuxBackend::Wayland
        } else {
            LinuxBackend::X11
        };
        let initial_size = WinitLogicalSize::new(
            self.config.initial_size.width,
            self.config.initial_size.height,
        );
        let mut attributes = Window::default_attributes()
            .with_title(self.config.title.clone())
            .with_inner_size(initial_size);
        attributes = match backend {
            LinuxBackend::Wayland => WindowAttributesExtWayland::with_name(
                attributes,
                self.config.application_id.clone(),
                self.config.application_id.clone(),
            ),
            LinuxBackend::X11 => WindowAttributesExtX11::with_name(
                attributes,
                self.config.application_id.clone(),
                self.config.application_id.clone(),
            ),
        };
        let window =
            event_loop
                .create_window(attributes)
                .map_err(|error| WindowStartupFailure {
                    error: LinuxShellError::WindowCreation(error.to_string()),
                    reason: LinuxStopReason::WindowCreationFailed,
                })?;
        let scale = scale_factor(window.scale_factor()).map_err(|_| WindowStartupFailure {
            error: LinuxShellError::WindowCreation(
                "backend returned an invalid scale factor".to_owned(),
            ),
            reason: LinuxStopReason::InvalidPlatformGeometry,
        })?;
        let size = physical_size(window.inner_size()).map_err(|_| WindowStartupFailure {
            error: LinuxShellError::WindowCreation(
                "backend returned an invalid initial size".to_owned(),
            ),
            reason: LinuxStopReason::InvalidPlatformGeometry,
        })?;
        let surface = self
            .surface_allocator
            .allocate()
            .map_err(|_| WindowStartupFailure {
                error: LinuxShellError::WindowCreation(
                    "surface identity exhausted before window admission".to_owned(),
                ),
                reason: LinuxStopReason::SurfaceIdentityExhausted,
            })?;
        let desired_surface = SurfaceDescriptor {
            id: surface,
            size,
            scale,
            format: self.config.desired_pixel_format,
            role: SurfaceRole::Window,
        };
        self.normalizer = Some(InputNormalizer::new(surface, scale, self.config.limits));
        self.desired_surface = Some(desired_surface);
        self.window = Some(window);
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
            let window = self.window.as_ref();
            let close_slot = delivering_close.then_some(&mut close_cancelled);
            let mut control = LinuxWindowControl {
                window,
                close_cancelled: close_slot,
                requested_stop: &mut requested_stop,
            };
            self.handler.handle_event(event, &mut control);
            self.delivered_events = self.delivered_events.saturating_add(1);

            if let Some(reason) = requested_stop {
                self.begin_stop(reason, event_loop);
                break;
            } else if delivering_close && !close_cancelled {
                self.begin_stop(LinuxStopReason::CloseRequested, event_loop);
                break;
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
        let (descriptor, event) = apply_surface_scale(descriptor, scale);
        self.desired_surface = Some(descriptor);
        if let Some(normalizer) = self.normalizer.as_mut() {
            normalizer.set_scale(scale);
        }
        self.enqueue(event, event_loop);
    }

    fn destroy_window_and_deliver(&mut self) {
        self.window.take();
        self.normalizer.take();
        let Some(descriptor) = self.desired_surface.take() else {
            return;
        };
        if self.surface_allocator.release(descriptor.id).is_err() {
            self.fatal_error = Some(LinuxShellError::SurfaceIdentityLifecycle);
        }
        if !self.destroyed_delivered {
            self.destroyed_delivered = true;
            let mut requested_stop = None;
            let mut control = LinuxWindowControl {
                window: None,
                close_cancelled: None,
                requested_stop: &mut requested_stop,
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
        };
        if !self.state.finish(report) {
            return;
        }
        let mut requested_stop = None;
        let mut control = LinuxWindowControl {
            window: None,
            close_cancelled: None,
            requested_stop: &mut requested_stop,
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
        self.enqueue(LinuxWindowEvent::Resumed, event_loop);
        self.drain(event_loop);
        if !self.state.is_running() {
            return;
        }
        if self.window.is_none()
            && let Err(failure) = self.create_window(event_loop)
        {
            self.fatal_error = Some(failure.error);
            self.begin_stop(failure.reason, event_loop);
        }
        self.drain(event_loop);
    }

    fn suspended(&mut self, event_loop: &ActiveEventLoop) {
        if !self.state.is_running() {
            return;
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
        if window.id() != window_id {
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
    use super::{ControlError, LinuxWindowControl, apply_surface_scale, apply_surface_size};
    use crate::event::{LinuxStopReason, LinuxWindowEvent};
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
    fn close_cancellation_is_invalid_without_exact_close_delivery() {
        let mut requested_stop = None;
        let mut control = LinuxWindowControl {
            window: None,
            close_cancelled: None,
            requested_stop: &mut requested_stop,
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
        {
            let mut control = LinuxWindowControl {
                window: None,
                close_cancelled: Some(&mut cancelled),
                requested_stop: &mut requested_stop,
            };
            control.cancel_close().unwrap();
        }
        assert!(cancelled);
        assert_eq!(requested_stop, None);
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
}
