use std::error::Error;
use std::io;
use std::thread;
use std::time::Duration;

use wild_buzzard_linux::{
    DirectFrameRequest, LinuxBackend, LinuxBackendPreference, LinuxPresentationShutdown,
    LinuxShellConfig, LinuxShutdownReport, LinuxStopReason, LinuxWakeStatus, LinuxWindowControl,
    LinuxWindowEvent, LinuxWindowHandler, LinuxWindowShell, PhysicalSize, SolidColor,
    SolidColorFrame, SurfaceId, SurfaceNamespace, SwapSubmissionReceipt,
};

type SurfaceIdentity = (u64, u32, u32);

struct SmokeHandler {
    expected_backend: LinuxBackend,
    ready: bool,
    redraw: bool,
    wake: bool,
    surface: Option<SurfaceIdentity>,
    surface_id: Option<SurfaceId>,
    size: Option<PhysicalSize>,
    swap_submission: Option<SwapSubmissionReceipt>,
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
                self.surface_id = Some(desired_surface.id);
                self.size = Some(desired_surface.size);
                if control.set_ime_allowed(true).is_err() {
                    self.fail("failed to enable IME", control);
                } else if control.request_redraw().is_err() {
                    self.fail("failed to request redraw", control);
                }
            }
            LinuxWindowEvent::Resized { surface, size, .. } => {
                if self.surface_id == Some(surface) {
                    self.size = Some(size);
                } else {
                    self.fail("resize named a foreign surface", control);
                }
            }
            LinuxWindowEvent::RedrawRequested { surface } => {
                self.redraw = true;
                if self.swap_submission.is_some() {
                    return;
                }
                if self.surface_id != Some(surface) {
                    self.fail("redraw named a foreign surface", control);
                    return;
                }
                let Some(size) = self.size else {
                    self.fail("redraw arrived without a known native size", control);
                    return;
                };
                let frame = SolidColorFrame::new(
                    DirectFrameRequest::new(surface, size, 1),
                    SolidColor::new(24, 92, 220, 255),
                );
                match control.submit_solid_frame(frame) {
                    Ok(receipt) => self.swap_submission = Some(receipt),
                    Err(error) => {
                        self.fail(format!("native frame submission failed: {error}"), control)
                    }
                }
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
            thread::sleep(Duration::from_millis(1_250));
            wake.wake()
        })?;
    let mut handler = SmokeHandler {
        expected_backend,
        ready: false,
        redraw: false,
        wake: false,
        surface: None,
        surface_id: None,
        size: None,
        swap_submission: None,
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
    let receipt = handler
        .swap_submission
        .ok_or_else(|| io::Error::other("no native draw-and-swap submission was recorded"))?;
    if receipt.surface() != handler.surface_id.expect("Ready identity checked above")
        || receipt.sequence() != 1
        || receipt.size() != handler.size.expect("Ready size checked above")
        || !rgba_within_one(receipt.diagnostic_sample(), [24, 92, 220, 255])
        || receipt.compositor_acknowledged()
    {
        return Err(io::Error::other(format!("invalid native swap receipt: {receipt:?}")).into());
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
    let LinuxPresentationShutdown::WrappersReleased(presentation) = report.presentation else {
        return Err(io::Error::other(format!(
            "normal shutdown did not release all presentation wrappers: {:?}",
            report.presentation
        ))
        .into());
    };
    if presentation.surface() != receipt.surface()
        || presentation.submitted_frames() != 1
        || presentation.last_sequence() != Some(1)
    {
        return Err(io::Error::other(format!(
            "invalid presentation teardown report: {presentation:?}"
        ))
        .into());
    }
    Ok(())
}

fn rgba_within_one(actual: [u8; 4], expected: [u8; 4]) -> bool {
    actual
        .into_iter()
        .zip(expected)
        .all(|(actual, expected)| actual.abs_diff(expected) <= 1)
}
