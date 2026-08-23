use std::error::Error;
use std::fmt;
use std::ops::Range;

use wild_buzzard_linux_presenter::{
    LinuxPresentationCapabilities, PresentationErrorKind, PresentationFailureStage,
    PresentationRetentionReport, PresentationShutdownReport, PresentationTeardownOutcome,
    WebRenderTeardownEvidence, WebRenderWindowErrorKind, WebRenderWindowFailureStage,
    WebRenderWindowShutdownFailure, WebRenderWindowShutdownReport,
};
use wild_buzzard_platform::{InputDeviceId, InputEvent, ScaleFactor, SurfaceDescriptor, SurfaceId};

/// Linux display protocol selected by winit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LinuxBackend {
    /// Wayland client connection.
    Wayland,
    /// X11 client connection.
    X11,
}

/// Provenance attached to a normalized input event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputOrigin {
    /// Event came directly from the selected Linux backend.
    Native,
    /// Event was explicitly marked synthetic by winit.
    Synthetic,
}

/// Validated, bounded IME preedit text and optional byte-index selection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BoundedImeText {
    text: String,
    selection: Option<Range<usize>>,
}

impl BoundedImeText {
    pub(crate) fn new(
        text: String,
        selection: Option<(usize, usize)>,
        maximum_bytes: usize,
    ) -> Result<Self, ImeTextError> {
        if text.len() > maximum_bytes {
            return Err(ImeTextError::TooLong {
                actual: text.len(),
                maximum: maximum_bytes,
            });
        }
        let selection = match selection {
            Some((start, end)) => {
                if start > end
                    || end > text.len()
                    || !text.is_char_boundary(start)
                    || !text.is_char_boundary(end)
                {
                    return Err(ImeTextError::InvalidSelection {
                        start,
                        end,
                        text_bytes: text.len(),
                    });
                }
                Some(start..end)
            }
            None => None,
        };
        Ok(Self { text, selection })
    }

    /// Returns the preedit string.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Returns the byte-indexed IME selection when the cursor is visible.
    #[must_use]
    pub fn selection(&self) -> Option<Range<usize>> {
        self.selection.clone()
    }
}

/// Invalid IME text rejected at the Linux adapter boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImeTextError {
    /// A preedit or commit exceeded its configured and hard-capped limit.
    TooLong { actual: usize, maximum: usize },
    /// A selection was reversed, out of bounds, or split a UTF-8 code point.
    InvalidSelection {
        start: usize,
        end: usize,
        text_bytes: usize,
    },
}

impl fmt::Display for ImeTextError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLong { actual, maximum } => {
                write!(
                    formatter,
                    "IME text contains {actual} bytes; maximum is {maximum}"
                )
            }
            Self::InvalidSelection {
                start,
                end,
                text_bytes,
            } => write!(
                formatter,
                "IME selection {start}..{end} is invalid for {text_bytes} UTF-8 bytes"
            ),
        }
    }
}

impl Error for ImeTextError {}

/// Observable event emitted by the one-window shell.
#[derive(Clone, Debug, PartialEq)]
pub enum LinuxWindowEvent {
    /// The top-level window and its exact attached EGL presentation surface are ready.
    /// The callback-scoped control returns capabilities bound to this exact surface.
    Ready {
        backend: LinuxBackend,
        /// Descriptor names the exact native presenter identity, size, scale, and format.
        desired_surface: SurfaceDescriptor,
    },
    /// The application event loop resumed.
    Resumed,
    /// The application event loop suspended.
    Suspended,
    /// Physical client size changed after validation.
    Resized {
        surface: SurfaceId,
        size: wild_buzzard_platform::PhysicalSize,
        scale: ScaleFactor,
    },
    /// Device scale changed; size is the last validated physical size.
    /// A later native resize is published separately rather than fabricated.
    ScaleFactorChanged {
        surface: SurfaceId,
        scale: ScaleFactor,
        size: wild_buzzard_platform::PhysicalSize,
    },
    /// Keyboard focus changed for the top-level surface.
    FocusChanged { surface: SurfaceId, focused: bool },
    /// Platform-neutral input with explicit native/synthetic provenance.
    Input {
        event: InputEvent,
        origin: InputOrigin,
    },
    /// One normalized native input-device identity was permanently retired.
    InputDeviceRemoved {
        surface: SurfaceId,
        device: InputDeviceId,
    },
    /// The native input method became available.
    ImeEnabled { surface: SurfaceId },
    /// Bounded composition text changed.
    ImePreedit {
        surface: SurfaceId,
        preedit: BoundedImeText,
    },
    /// The native input method became unavailable.
    ImeDisabled { surface: SurfaceId },
    /// Winit requested a redraw; the callback control may submit one direct frame.
    RedrawRequested { surface: SurfaceId },
    /// A coalesced cross-thread wake reached the owner thread.
    WakeRequested,
    /// Window-manager close intent. It is cancellable only during delivery.
    CloseRequested { surface: SurfaceId },
    /// Native wrappers released normally and surface identity retired exactly once.
    Destroyed { surface: SurfaceId },
    /// Reserved terminal notification, never stored in ordinary queue capacity.
    Stopped(LinuxShutdownReport),
}

/// Exact terminal reason for one shell run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LinuxStopReason {
    /// The handler explicitly requested exit.
    Requested,
    /// An uncancelled close intent requested exit.
    CloseRequested,
    /// A non-coalescible event could not enter the bounded queue.
    EventQueueSaturated { capacity: usize },
    /// The input event sequence reached `u64::MAX` without wrapping.
    EventSequenceExhausted,
    /// Monotonic elapsed microseconds could not fit in `u64`.
    EventTimestampExhausted,
    /// Too many simultaneous devices were observed.
    DeviceCapacityExhausted { capacity: usize },
    /// The monotonically allocated device identity reached `u64::MAX`.
    DeviceIdentityExhausted,
    /// Too many simultaneous touch contacts were observed.
    TouchCapacityExhausted { capacity: usize },
    /// The monotonically allocated pointer identity reached `u64::MAX`.
    PointerIdentityExhausted,
    /// A surface identity could not be allocated.
    SurfaceIdentityExhausted,
    /// An already allocated surface identity could not be retired exactly once.
    SurfaceIdentityViolation,
    /// The native top-level window could not be created.
    WindowCreationFailed,
    /// EGL presentation failed and permanently sealed this shell.
    PresentationFailed(PresentationFailureStage),
    /// The same-surface browser compositor failed at an exact stable stage.
    BrowserPresentationFailed(WebRenderWindowFailureStage),
    /// The only native window was destroyed without an earlier exit request.
    WindowDestroyed,
    /// The selected backend supplied invalid geometry or scale.
    InvalidPlatformGeometry,
    /// A touch pressure value was not finite and normalized.
    InvalidTouchPressure,
    /// An IME payload violated its configured bound or UTF-8 selection.
    InvalidImeText,
    /// The native event loop began exiting without a prior local request.
    BackendExited,
}

/// Explicit result of the native presentation-owner teardown attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LinuxPresentationShutdown {
    /// No native presenter owner was established and no presenter teardown occurred.
    NotCreated,
    /// EGL was checked non-current and every Rust native-owner wrapper released normally.
    WrappersReleased(PresentationShutdownReport),
    /// Teardown failed or panicked; every still-extant native owner was retained fail-closed.
    RetainedAfterTeardownFailure(PresentationRetentionReport),
    /// The `WebRender` worker/renderer and nested native presenter released in
    /// the required order.
    BrowserWrappersReleased(WebRenderWindowShutdownReport),
    /// Browser-compositor shutdown failed; the first error and every available
    /// ordered teardown proof are retained without native authority.
    BrowserTeardownFailed(LinuxBrowserShutdownFailure),
}

impl LinuxPresentationShutdown {
    /// Exact selected profile when native presentation ownership was established.
    #[must_use]
    pub const fn capabilities(self) -> Option<LinuxPresentationCapabilities> {
        match self {
            Self::NotCreated => None,
            Self::WrappersReleased(report) => Some(report.capabilities()),
            Self::RetainedAfterTeardownFailure(report) => Some(report.capabilities()),
            Self::BrowserWrappersReleased(report) => Some(report.capabilities()),
            Self::BrowserTeardownFailed(failure) => Some(failure.capabilities()),
        }
    }
}

/// Copyable terminal summary of one browser-compositor teardown failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LinuxBrowserShutdownFailure {
    stage: WebRenderWindowFailureStage,
    kind: WebRenderWindowErrorKind,
    backend_shutdown: WebRenderTeardownEvidence,
    renderer_deinitialization: WebRenderTeardownEvidence,
    presentation: PresentationTeardownOutcome,
}

impl LinuxBrowserShutdownFailure {
    pub(crate) fn from_failure(failure: &WebRenderWindowShutdownFailure) -> Self {
        Self {
            stage: failure.primary().stage(),
            kind: failure.primary().kind(),
            backend_shutdown: failure.backend_shutdown(),
            renderer_deinitialization: failure.renderer_deinitialization(),
            presentation: failure.presentation(),
        }
    }

    #[must_use]
    pub const fn stage(self) -> WebRenderWindowFailureStage {
        self.stage
    }

    #[must_use]
    pub const fn kind(self) -> WebRenderWindowErrorKind {
        self.kind
    }

    #[must_use]
    pub const fn backend_shutdown(self) -> WebRenderTeardownEvidence {
        self.backend_shutdown
    }

    #[must_use]
    pub const fn renderer_deinitialization(self) -> WebRenderTeardownEvidence {
        self.renderer_deinitialization
    }

    #[must_use]
    pub const fn presentation(self) -> PresentationTeardownOutcome {
        self.presentation
    }

    /// Exact immutable profile whose nested native owners reached teardown.
    #[must_use]
    pub const fn capabilities(self) -> LinuxPresentationCapabilities {
        self.presentation.capabilities()
    }
}

impl From<PresentationTeardownOutcome> for LinuxPresentationShutdown {
    fn from(outcome: PresentationTeardownOutcome) -> Self {
        match outcome {
            PresentationTeardownOutcome::WrappersReleased(report) => Self::WrappersReleased(report),
            PresentationTeardownOutcome::RetainedAfterTeardownFailure(report) => {
                Self::RetainedAfterTeardownFailure(report)
            }
        }
    }
}

/// Stable summary returned after the shell has retired presentation admission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LinuxShutdownReport {
    /// First reason which initiated shutdown.
    pub reason: LinuxStopReason,
    /// Non-`Stopped` events delivered; includes `Destroyed` only after normal wrapper release.
    pub delivered_events: u64,
    /// Replaceable events overwritten before delivery.
    pub coalesced_events: u64,
    /// Unknown or internally incomplete native events ignored without fabrication.
    pub ignored_native_events: u64,
    /// Unambiguous native presentation teardown result.
    pub presentation: LinuxPresentationShutdown,
}

/// Invalid operation attempted through the callback-scoped control object.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlError {
    /// No live window exists for this operation.
    NoLiveWindow,
    /// A second native-size request was attempted in the same event callback.
    InnerSizeAlreadyRequested,
    /// The IME cursor rectangle bypassed validated geometry constructors.
    InvalidImeCursorArea,
    /// `cancel_close` was called outside delivery of the exact close intent.
    NotDeliveringCloseIntent,
    /// Direct native frame submission failed at an exact stable stage.
    PresentationFailed {
        stage: PresentationFailureStage,
        kind: PresentationErrorKind,
    },
    /// The requested operation belongs to the other configured presenter mode.
    WrongPresentationMode,
    /// Browser composition failed at one exact stage and error category.
    BrowserPresentationFailed {
        stage: WebRenderWindowFailureStage,
        kind: WebRenderWindowErrorKind,
        /// Whether the compositor/native owner permanently closed admission.
        terminal: bool,
    },
}

impl ControlError {
    /// Returns terminality only for an exact browser-presentation failure.
    /// Other control errors are not safe to reinterpret as retryable frame
    /// rejection.
    #[must_use]
    pub const fn browser_presentation_terminal(self) -> Option<bool> {
        match self {
            Self::BrowserPresentationFailed { terminal, .. } => Some(terminal),
            _ => None,
        }
    }
}

impl fmt::Display for ControlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoLiveWindow => formatter.write_str("no live window exists"),
            Self::InnerSizeAlreadyRequested => {
                formatter.write_str("an inner-size request was already made in this callback")
            }
            Self::InvalidImeCursorArea => {
                formatter.write_str("IME cursor area contains invalid logical geometry")
            }
            Self::NotDeliveringCloseIntent => {
                formatter.write_str("no close intent is currently being delivered")
            }
            Self::PresentationFailed { stage, kind } => {
                write!(
                    formatter,
                    "native frame submission failed at {stage:?}: {kind:?}"
                )
            }
            Self::WrongPresentationMode => {
                formatter.write_str("operation is unavailable in the configured presentation mode")
            }
            Self::BrowserPresentationFailed { stage, kind, .. } => {
                write!(
                    formatter,
                    "browser presentation failed at {stage:?}: {kind:?}"
                )
            }
        }
    }
}

impl Error for ControlError {}

#[cfg(test)]
mod tests {
    use super::{BoundedImeText, ImeTextError};

    #[test]
    fn ime_selection_is_bounded_and_on_utf8_boundaries() {
        let valid = BoundedImeText::new("a🦅b".to_owned(), Some((1, 5)), 16).unwrap();
        assert_eq!(valid.selection(), Some(1..5));

        assert_eq!(
            BoundedImeText::new("a🦅b".to_owned(), Some((2, 5)), 16),
            Err(ImeTextError::InvalidSelection {
                start: 2,
                end: 5,
                text_bytes: 6,
            })
        );
        assert_eq!(
            BoundedImeText::new("too long".to_owned(), None, 3),
            Err(ImeTextError::TooLong {
                actual: 8,
                maximum: 3,
            })
        );
    }
}
