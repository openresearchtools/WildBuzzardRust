use std::error::Error;
use std::io;
use std::thread;
use std::time::Duration;

use wild_buzzard_linux::{
    LinuxBackend, LinuxBackendPreference, LinuxShellConfig, LinuxShutdownReport, LinuxStopReason,
    LinuxWakeStatus, LinuxWindowControl, LinuxWindowEvent, LinuxWindowHandler, LinuxWindowShell,
    SurfaceNamespace,
};

type SurfaceIdentity = (u64, u32, u32);

struct SmokeHandler {
    expected_backend: LinuxBackend,
    ready: bool,
    redraw: bool,
    wake: bool,
    surface: Option<SurfaceIdentity>,
    destroyed_count: usize,
    stopped_count: usize,
    stopped_report: Option<LinuxShutdownReport>,
    lifecycle: Vec<&'static str>,
    failure: Option<String>,
}

impl SmokeHandler {
    fn fail(&mut self, message: impl Into<String>, control: &mut LinuxWindowControl<'_>) {
        if self.failure.is_none() {
            self.failure = Some(message.into());
        }
        control.request_exit();
    }
}

impl LinuxWindowHandler for SmokeHandler {
    fn handle_event(&mut self, event: LinuxWindowEvent, control: &mut LinuxWindowControl<'_>) {
        match event {
            LinuxWindowEvent::Resumed => self.lifecycle.push("Resumed"),
            LinuxWindowEvent::Ready {
                backend,
                desired_surface,
            } => {
                self.lifecycle.push("Ready");
                self.ready = true;
                if backend != self.expected_backend {
                    self.fail(
                        format!(
                            "requested {:?} but Ready reported {backend:?}",
                            self.expected_backend
                        ),
                        control,
                    );
                    return;
                }
                let identity = (
                    desired_surface.id.namespace().get(),
                    desired_surface.id.slot(),
                    desired_surface.id.generation(),
                );
                if let Some(previous) = self.surface {
                    self.fail(
                        format!("duplicate Ready identities: {previous:?} then {identity:?}"),
                        control,
                    );
                    return;
                }
                self.surface = Some(identity);
                if control.set_ime_allowed(true).is_err() {
                    self.fail("failed to enable IME", control);
                } else if control.request_redraw().is_err() {
                    self.fail("failed to request redraw", control);
                }
            }
            LinuxWindowEvent::RedrawRequested { .. } => {
                self.redraw = true;
            }
            LinuxWindowEvent::WakeRequested => {
                self.wake = true;
                control.request_exit();
            }
            LinuxWindowEvent::Destroyed { surface } => {
                self.lifecycle.push("Destroyed");
                self.destroyed_count += 1;
                let identity = (
                    surface.namespace().get(),
                    surface.slot(),
                    surface.generation(),
                );
                if self.surface != Some(identity) {
                    self.fail(
                        format!(
                            "Destroyed identity {identity:?} did not match Ready {:?}",
                            self.surface
                        ),
                        control,
                    );
                }
            }
            LinuxWindowEvent::Stopped(report) => {
                self.lifecycle.push("Stopped");
                self.stopped_count += 1;
                if self.stopped_report.replace(report).is_some() {
                    self.fail("Stopped was delivered more than once", control);
                }
            }
            _ => {}
        }
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    if std::env::var("WILDBUZZARD_REAL_DISPLAY_TEST").as_deref() != Ok("1") {
        return Err(io::Error::other(
            "refusing to open a display without WILDBUZZARD_REAL_DISPLAY_TEST=1",
        )
        .into());
    }

    let requested_backend = std::env::var("WILDBUZZARD_DISPLAY_BACKEND").map_err(|_| {
        io::Error::other("WILDBUZZARD_DISPLAY_BACKEND must be exactly wayland or x11")
    })?;
    let (backend_preference, expected_backend) = match requested_backend.as_str() {
        "wayland" => (LinuxBackendPreference::Wayland, LinuxBackend::Wayland),
        "x11" => (LinuxBackendPreference::X11, LinuxBackend::X11),
        value => {
            return Err(io::Error::other(format!(
                "WILDBUZZARD_DISPLAY_BACKEND must be wayland or x11, not {value:?}"
            ))
            .into());
        }
    };

    let namespace = SurfaceNamespace::new(90_061)
        .ok_or_else(|| io::Error::other("smoke namespace must be non-zero"))?;
    let mut config = LinuxShellConfig::wild_buzzard_default(namespace);
    config.backend = backend_preference;

    let shell = LinuxWindowShell::new(config)?;
    let wake = shell.wake_handle();
    let surviving_wake = wake.clone();
    let wake_thread = thread::Builder::new()
        .name("wild-buzzard-window-smoke-wake".to_owned())
        .spawn(move || {
            thread::sleep(Duration::from_millis(750));
            wake.wake()
        })?;
    let mut handler = SmokeHandler {
        expected_backend,
        ready: false,
        redraw: false,
        wake: false,
        surface: None,
        destroyed_count: 0,
        stopped_count: 0,
        stopped_report: None,
        lifecycle: Vec::new(),
        failure: None,
    };
    let report = shell.run(&mut handler)?;
    let wake_status = wake_thread
        .join()
        .map_err(|_| io::Error::other("wake thread panicked"))?;

    if let Some(error) = handler.failure.take() {
        return Err(io::Error::other(error).into());
    }
    if wake_status != LinuxWakeStatus::Queued {
        return Err(io::Error::other(format!("wake was not admitted: {wake_status:?}")).into());
    }
    if surviving_wake.wake() != LinuxWakeStatus::Closed {
        return Err(io::Error::other("surviving wake handle was not closed after run").into());
    }
    if !(handler.ready && handler.redraw && handler.wake) {
        return Err(io::Error::other(format!(
            "incomplete smoke: ready={}, redraw={}, wake={}",
            handler.ready, handler.redraw, handler.wake
        ))
        .into());
    }
    if handler.lifecycle != ["Resumed", "Ready", "Destroyed", "Stopped"] {
        return Err(io::Error::other(format!(
            "unexpected lifecycle order: {:?}",
            handler.lifecycle
        ))
        .into());
    }
    if handler.destroyed_count != 1 || handler.stopped_count != 1 {
        return Err(io::Error::other(format!(
            "terminal lifecycle counts: Destroyed={}, Stopped={}",
            handler.destroyed_count, handler.stopped_count
        ))
        .into());
    }
    if handler.stopped_report != Some(report) {
        return Err(io::Error::other("Stopped report did not match run result").into());
    }
    if report.reason != LinuxStopReason::Requested {
        return Err(
            io::Error::other(format!("unexpected shutdown reason: {:?}", report.reason)).into(),
        );
    }
    Ok(())
}
