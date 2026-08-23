use std::any::Any;
use std::cmp::Reverse;
use std::ffi::CString;
use std::marker::PhantomData;
use std::mem;
use std::num::NonZeroU32;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::NonNull;
use std::rc::Rc;

use gleam::gl;
use glutin::api::egl::config::Config;
use glutin::api::egl::context::{NotCurrentContext, PossiblyCurrentContext};
use glutin::api::egl::display::Display;
use glutin::api::egl::surface::Surface;
use glutin::config::{Api, ColorBufferType, ConfigSurfaceTypes, ConfigTemplateBuilder, GlConfig};
use glutin::context::{
    ContextApi, ContextAttributesBuilder, GlProfile, NotCurrentGlContext, PossiblyCurrentGlContext,
    Robustness, Version,
};
use glutin::display::{AsRawDisplay, GlDisplay, RawDisplay};
use glutin::error::ErrorKind as GlutinErrorKind;
use glutin::platform::x11::X11GlConfigExt;
use glutin::surface::{
    AsRawSurface, GlSurface, RawSurface, SurfaceAttributesBuilder, SwapInterval, WindowSurface,
};
use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
use wild_buzzard_platform::{PhysicalSize, ScaleFactor, SurfaceDescriptor, SurfaceId};
use winit::dpi::{LogicalPosition, LogicalSize};
use winit::window::{Window, WindowId};

use crate::contract::{
    DirectFrameRequest, DirectRenderError, DirectRenderer, LinuxAccelerationClass,
    LinuxPresentationBackend, LinuxPresentationCapabilities, LinuxPresentationPolicy,
    LinuxResetProtection, PresentationContract, PresentationError, PresentationErrorKind,
    PresentationFailureStage, PresentationLimits, PresentationRetentionReport,
    PresentationShutdownReport, PresentationStartupFailure, PresentationState,
    PresentationTeardownOutcome, SolidColor, SolidColorFrame, SwapSubmissionReceipt,
};

// Core OpenGL 4.5 / ARB_robustness error token. Gleam's generated union
// omits this constant even though a robust 3.2 context may report it.
const GL_CONTEXT_LOST: u32 = 0x0507;
const GL_CONTEXT_FLAGS: u32 = 0x821e;
const GL_CONTEXT_FLAG_ROBUST_ACCESS_BIT: u32 = 0x0000_0004;
const GL_RESET_NOTIFICATION_STRATEGY: u32 = 0x8256;
const GL_LOSE_CONTEXT_ON_RESET: i32 = 0x8252;

// EGL 1.5 C ABI on the only supported target, x86_64-unknown-linux-gnu.
type EglBoolean = u32;
type EglInt = i32;
type EglQuerySurfaceFn = unsafe extern "C" fn(
    display: *mut std::ffi::c_void,
    surface: *mut std::ffi::c_void,
    attribute: EglInt,
    value: *mut EglInt,
) -> EglBoolean;

const EGL_FALSE: EglBoolean = 0;
const EGL_TRUE: EglBoolean = 1;
const EGL_WIDTH: EglInt = 0x3057;
const EGL_HEIGHT: EglInt = 0x3056;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LinuxEglProfile {
    AcceleratedRobust,
    AcceleratedCompatible,
    SoftwareRobust,
    SoftwareCompatible,
}

const STRICT_PROFILES: [LinuxEglProfile; 1] = [LinuxEglProfile::AcceleratedRobust];
const AUTOMATIC_PROFILES: [LinuxEglProfile; 4] = [
    LinuxEglProfile::AcceleratedRobust,
    LinuxEglProfile::AcceleratedCompatible,
    LinuxEglProfile::SoftwareRobust,
    LinuxEglProfile::SoftwareCompatible,
];

impl LinuxEglProfile {
    const fn capabilities(self) -> LinuxPresentationCapabilities {
        match self {
            Self::AcceleratedRobust => LinuxPresentationCapabilities::new(
                LinuxAccelerationClass::Accelerated,
                LinuxResetProtection::LoseContextOnReset,
            ),
            Self::AcceleratedCompatible => LinuxPresentationCapabilities::new(
                LinuxAccelerationClass::Accelerated,
                LinuxResetProtection::Unavailable,
            ),
            Self::SoftwareRobust => LinuxPresentationCapabilities::new(
                LinuxAccelerationClass::Software,
                LinuxResetProtection::LoseContextOnReset,
            ),
            Self::SoftwareCompatible => LinuxPresentationCapabilities::new(
                LinuxAccelerationClass::Software,
                LinuxResetProtection::Unavailable,
            ),
        }
    }

    const fn robustness(self) -> Robustness {
        match self {
            Self::AcceleratedRobust | Self::SoftwareRobust => Robustness::RobustLoseContextOnReset,
            Self::AcceleratedCompatible | Self::SoftwareCompatible => Robustness::NotRobust,
        }
    }
}

const fn profile_ladder(policy: LinuxPresentationPolicy) -> &'static [LinuxEglProfile] {
    match policy {
        LinuxPresentationPolicy::StrictHardware => &STRICT_PROFILES,
        LinuxPresentationPolicy::AutomaticCompatible => &AUTOMATIC_PROFILES,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EglExtentAttribute {
    Width,
    Height,
}

impl EglExtentAttribute {
    const fn token(self) -> EglInt {
        match self {
            Self::Width => EGL_WIDTH,
            Self::Height => EGL_HEIGHT,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EglExtentQueryFailure {
    MissingQuerySurface,
    WrongDisplayBackend,
    WrongSurfaceBackend,
    NullDisplay,
    NullSurface,
    QueryRejected(EglExtentAttribute),
    InvalidBoolean {
        attribute: EglExtentAttribute,
        value: EglBoolean,
    },
    InvalidValue {
        attribute: EglExtentAttribute,
        value: EglInt,
    },
    Mismatch {
        expected: (u32, u32),
        observed: (u32, u32),
    },
}

impl std::fmt::Display for EglExtentQueryFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingQuerySurface => formatter.write_str("eglQuerySurface is unavailable"),
            Self::WrongDisplayBackend => {
                formatter.write_str("retained display is not an EGL display")
            }
            Self::WrongSurfaceBackend => {
                formatter.write_str("retained surface is not an EGL surface")
            }
            Self::NullDisplay => formatter.write_str("retained EGL display pointer is null"),
            Self::NullSurface => formatter.write_str("retained EGL surface pointer is null"),
            Self::QueryRejected(attribute) => {
                write!(formatter, "eglQuerySurface rejected {attribute:?}")
            }
            Self::InvalidBoolean { attribute, value } => write!(
                formatter,
                "eglQuerySurface returned invalid {attribute:?} EGLBoolean {value}"
            ),
            Self::InvalidValue { attribute, value } => {
                write!(
                    formatter,
                    "eglQuerySurface returned invalid {attribute:?} value {value}"
                )
            }
            Self::Mismatch { expected, observed } => write!(
                formatter,
                "native EGL extent {observed:?} does not match expected {expected:?}"
            ),
        }
    }
}

#[derive(Clone, Copy)]
struct EglObjects {
    display: NonNull<std::ffi::c_void>,
    surface: NonNull<std::ffi::c_void>,
}

trait EglSurfaceAttributeQuery {
    fn query(
        &self,
        objects: EglObjects,
        attribute: EglExtentAttribute,
        value: &mut EglInt,
    ) -> EglBoolean;
}

#[derive(Clone, Copy, Debug)]
struct LoadedEglSurfaceQuery {
    query_surface: EglQuerySurfaceFn,
}

impl LoadedEglSurfaceQuery {
    fn load(display: &Display) -> Result<Self, EglExtentQueryFailure> {
        let address = checked_query_surface_address(display.get_proc_address(c"eglQuerySurface"))?;
        // SAFETY: the non-null pointer was returned for the exact core EGL
        // symbol `eglQuerySurface`. Its C signature is fixed by EGL 1.5 and the
        // local aliases above match the supported Linux x86-64 C ABI.
        let query_surface =
            unsafe { mem::transmute::<*mut std::ffi::c_void, EglQuerySurfaceFn>(address.as_ptr()) };
        Ok(Self { query_surface })
    }
}

fn checked_query_surface_address(
    address: *const std::ffi::c_void,
) -> Result<NonNull<std::ffi::c_void>, EglExtentQueryFailure> {
    NonNull::new(address.cast_mut()).ok_or(EglExtentQueryFailure::MissingQuerySurface)
}

impl EglSurfaceAttributeQuery for LoadedEglSurfaceQuery {
    fn query(
        &self,
        objects: EglObjects,
        attribute: EglExtentAttribute,
        value: &mut EglInt,
    ) -> EglBoolean {
        // SAFETY: `objects` can only be constructed from the retained glutin
        // EGL display/surface pair. Both wrappers remain borrowed for this
        // synchronous call, the function pointer was loaded from that exact
        // display, and `value` is valid writable EGLint storage.
        unsafe {
            (self.query_surface)(
                objects.display.as_ptr(),
                objects.surface.as_ptr(),
                attribute.token(),
                value,
            )
        }
    }
}

/// Value-only parameters supplied while the native display borrow is live.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LinuxWindowPreparation {
    x11_visual_id: Option<u32>,
    capabilities: LinuxPresentationCapabilities,
}

impl LinuxWindowPreparation {
    /// Exact EGL-compatible X11 visual to apply before window creation.
    #[must_use]
    pub const fn x11_visual_id(self) -> Option<u32> {
        self.x11_visual_id
    }

    /// Exact profile being attempted for this not-yet-published window.
    #[must_use]
    pub const fn capabilities(self) -> LinuxPresentationCapabilities {
        self.capabilities
    }
}

/// Value-only result of checking a requested extent against the retained EGL
/// window surface.
///
/// This carries no window, display, surface, or GL authority. `Pending` is an
/// ordinary observation while the native backend catches up; it does not
/// change the presenter's descriptor or lifecycle. Wayland requires the
/// presenter's existing checked resize transaction to recreate its EGL window
/// surface before EGL can report a synchronously accepted winit extent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeExtentConfirmation {
    /// The retained EGL surface reports the exact requested width and height.
    Confirmed,
    /// Wayland accepted the winit extent and now requires the presenter's
    /// checked resize transaction to recreate and exact-verify the EGL surface.
    ReadyForCheckedResize,
    /// The retained EGL surface does not yet report the requested extent.
    Pending,
}

/// Failure from the synchronous display/window/presenter creation transaction.
#[derive(Debug)]
pub enum LinuxPresenterCreationError<E> {
    /// EGL preparation or attachment failed before a presenter owner existed.
    Presentation(PresentationError),
    /// Startup failed after a presenter owner existed and was explicitly retired.
    PresentationWithTeardown(PresentationStartupFailure),
    /// The caller's native-window construction closure failed.
    Window(E),
}

impl<E: std::fmt::Display> std::fmt::Display for LinuxPresenterCreationError<E> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Presentation(error) => {
                write!(formatter, "Linux presentation creation failed: {error}")
            }
            Self::PresentationWithTeardown(error) => {
                write!(formatter, "Linux presentation startup failed: {error}")
            }
            Self::Window(error) => write!(formatter, "Linux window creation failed: {error}"),
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for LinuxPresenterCreationError<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Presentation(error) => Some(error),
            Self::PresentationWithTeardown(error) => Some(error),
            Self::Window(error) => Some(error),
        }
    }
}

/// Creates a native window and attaches its EGL presenter while retaining the
/// exact display-source borrow for the complete transaction.
///
/// The closure receives only the value-only X11 visual and attempted
/// capability requirements. It may be called once per profile when a typed
/// unsupported result has no native owner or a fully released unsupported
/// profile advances the automatic ladder. It must return a newly created
/// unpublished window and its Wild Buzzard surface descriptor.
/// The returned window's display identity is compared exactly with the held
/// display identity before any unsafe context or surface creation.
///
/// # Errors
///
/// Returns a staged presentation failure or the closure's window failure.
pub fn prepare_and_attach<E>(
    display_source: &impl HasDisplayHandle,
    backend: LinuxPresentationBackend,
    policy: LinuxPresentationPolicy,
    mut create_window: impl FnMut(LinuxWindowPreparation) -> Result<(Window, SurfaceDescriptor), E>,
) -> Result<LinuxPresentedWindow, LinuxPresenterCreationError<E>> {
    let display_handle = display_source.display_handle().map_err(|error| {
        LinuxPresenterCreationError::Presentation(PresentationError::contract(
            PresentationFailureStage::DisplayHandle,
            PresentationErrorKind::Driver,
            error,
        ))
    })?;
    let raw_display = display_handle.as_raw();
    let display_bootstrap = LinuxEglDisplayBootstrap::prepare(raw_display, backend)
        .map_err(LinuxPresenterCreationError::Presentation)?;
    let result = drive_profile_ladder(policy, |profile| {
        let Some(bootstrap) = display_bootstrap.profile(profile) else {
            return ProfileAttempt::UnsupportedBeforeOwnership(unsupported_profile_error(
                profile,
                PresentationFailureStage::SelectConfig,
                "no matching window-capable RGBA8 sRGB desktop-GL EGL configuration",
            ));
        };
        let preparation = LinuxWindowPreparation {
            x11_visual_id: bootstrap.x11_visual_id,
            capabilities: profile.capabilities(),
        };
        let (window, descriptor) = match create_window(preparation) {
            Ok(created) => created,
            Err(error) => return ProfileAttempt::Window(error),
        };
        match bootstrap.attach(window, descriptor) {
            Ok(presenter) => ProfileAttempt::Selected(presenter),
            Err(PresenterAttachError::UnsupportedAndReleased(error, release)) => {
                ProfileAttempt::UnsupportedAfterRelease(error, release)
            }
            Err(PresenterAttachError::BeforeOwnership(error)) => {
                ProfileAttempt::Presentation(error)
            }
            Err(PresenterAttachError::AfterOwnership(error)) => {
                ProfileAttempt::PresentationWithTeardown(error)
            }
        }
    });
    // This post-attachment use keeps the source display borrow live through
    // context and surface creation even though `DisplayHandle` is `Copy`.
    let _held_display_identity = display_handle.as_raw();
    result
}

enum ProfileAttempt<T, E> {
    Selected(T),
    UnsupportedBeforeOwnership(PresentationError),
    UnsupportedAfterRelease(PresentationError, PresentationShutdownReport),
    Presentation(PresentationError),
    PresentationWithTeardown(PresentationStartupFailure),
    Window(E),
}

fn drive_profile_ladder<T, E>(
    policy: LinuxPresentationPolicy,
    mut attempt: impl FnMut(LinuxEglProfile) -> ProfileAttempt<T, E>,
) -> Result<T, LinuxPresenterCreationError<E>> {
    let mut last_unsupported = None;
    for &profile in profile_ladder(policy) {
        match attempt(profile) {
            ProfileAttempt::Selected(owner) => return Ok(owner),
            ProfileAttempt::UnsupportedBeforeOwnership(error) => {
                validate_unsupported_attempt(profile, &error, None)?;
                last_unsupported = Some(error);
            }
            ProfileAttempt::UnsupportedAfterRelease(error, release) => {
                validate_unsupported_attempt(profile, &error, Some(release))?;
                last_unsupported = Some(error);
            }
            ProfileAttempt::Presentation(error) => {
                return Err(LinuxPresenterCreationError::Presentation(error));
            }
            ProfileAttempt::PresentationWithTeardown(error) => {
                return Err(LinuxPresenterCreationError::PresentationWithTeardown(error));
            }
            ProfileAttempt::Window(error) => {
                return Err(LinuxPresenterCreationError::Window(error));
            }
        }
    }
    Err(LinuxPresenterCreationError::Presentation(
        last_unsupported.unwrap_or_else(|| {
            PresentationError::contract(
                PresentationFailureStage::SelectConfig,
                PresentationErrorKind::UnsupportedCapability,
                "presentation policy contained no startup profiles",
            )
        }),
    ))
}

fn validate_unsupported_attempt<E>(
    profile: LinuxEglProfile,
    error: &PresentationError,
    release: Option<PresentationShutdownReport>,
) -> Result<(), LinuxPresenterCreationError<E>> {
    let expected = profile.capabilities();
    if error.kind() != PresentationErrorKind::UnsupportedCapability
        || error.capabilities() != Some(expected)
        || release.is_some_and(|report| {
            report.capabilities() != expected
                || report.submitted_frames() != 0
                || report.last_sequence().is_some()
        })
    {
        return Err(LinuxPresenterCreationError::Presentation(
            PresentationError::contract(
                PresentationFailureStage::ReleaseContext,
                PresentationErrorKind::Driver,
                "unsupported-profile evidence did not match the exact attempted capabilities",
            )
            .with_capabilities(expected),
        ));
    }
    Ok(())
}

fn unsupported_profile_error(
    profile: LinuxEglProfile,
    stage: PresentationFailureStage,
    detail: impl std::fmt::Display,
) -> PresentationError {
    PresentationError::contract(stage, PresentationErrorKind::UnsupportedCapability, detail)
        .with_capabilities(profile.capabilities())
}

enum PresenterAttachError {
    UnsupportedAndReleased(PresentationError, PresentationShutdownReport),
    BeforeOwnership(PresentationError),
    AfterOwnership(PresentationStartupFailure),
}

enum ContextCreationAttempt {
    Created(NotCurrentContext),
    Unsupported(PresentationError),
    Failed(PresentationError),
}

#[derive(Clone)]
struct SelectedEglConfig {
    config: Config,
    x11_visual_id: Option<u32>,
}

#[derive(Default)]
struct SelectedEglConfigs {
    accelerated: Option<SelectedEglConfig>,
    software: Option<SelectedEglConfig>,
}

struct LinuxEglDisplayBootstrap {
    backend: LinuxPresentationBackend,
    raw_display: RawDisplayHandle,
    display: Display,
    configs: SelectedEglConfigs,
    limits: PresentationLimits,
    _owner_thread: PhantomData<Rc<()>>,
}

impl LinuxEglDisplayBootstrap {
    fn prepare(
        raw_display: RawDisplayHandle,
        backend: LinuxPresentationBackend,
    ) -> Result<Self, PresentationError> {
        validate_display_backend(raw_display, backend)?;

        // SAFETY: `display_handle` is a live borrow from the winit event loop on
        // its owner thread. `Display::new` copies the native identity into an
        // owned EGL display reference; the event loop outlives every window and
        // presenter created from this bootstrap.
        let display = catch_glutin(PresentationFailureStage::CreateDisplay, || unsafe {
            Display::new(raw_display)
        })?;
        let template = ConfigTemplateBuilder::new()
            .with_alpha_size(8)
            .with_depth_size(0)
            .with_stencil_size(0)
            .with_float_pixels(false)
            .with_surface_type(ConfigSurfaceTypes::WINDOW)
            .with_api(Api::OPENGL)
            .build();
        // SAFETY: `display` is initialized above and the template contains no
        // native pointer. Returned configs are owned children of this display.
        let configs = catch_glutin(PresentationFailureStage::SelectConfig, || unsafe {
            display.find_configs(template)
        })?;
        let configs = catch_native(PresentationFailureStage::SelectConfig, || {
            select_configs(configs, backend)
        })??;
        Ok(Self {
            backend,
            raw_display,
            display,
            configs,
            limits: PresentationLimits::default(),
            _owner_thread: PhantomData,
        })
    }

    fn profile(&self, profile: LinuxEglProfile) -> Option<LinuxEglBootstrap> {
        let selected = match profile.capabilities().acceleration() {
            LinuxAccelerationClass::Accelerated => self.configs.accelerated.as_ref(),
            LinuxAccelerationClass::Software => self.configs.software.as_ref(),
        }?;
        Some(LinuxEglBootstrap {
            backend: self.backend,
            raw_display: self.raw_display,
            display: self.display.clone(),
            config: selected.config.clone(),
            x11_visual_id: selected.x11_visual_id,
            limits: self.limits,
            profile,
            _owner_thread: PhantomData,
        })
    }
}

struct LinuxEglBootstrap {
    backend: LinuxPresentationBackend,
    raw_display: RawDisplayHandle,
    display: Display,
    config: Config,
    x11_visual_id: Option<u32>,
    limits: PresentationLimits,
    profile: LinuxEglProfile,
    _owner_thread: PhantomData<Rc<()>>,
}

impl LinuxEglBootstrap {
    fn create_context(&self, raw_window: RawWindowHandle) -> ContextCreationAttempt {
        let capabilities = self.profile.capabilities();
        let attributes = ContextAttributesBuilder::new()
            .with_context_api(ContextApi::OpenGl(Some(Version::new(3, 2))))
            .with_profile(GlProfile::Core)
            .with_robustness(self.profile.robustness())
            .build(Some(raw_window));
        // SAFETY: the context attributes contain a handle borrowed from the
        // window retained by `attach`. The config belongs to this exact
        // display, and successful ownership keeps every wrapper together.
        match catch_unwind(AssertUnwindSafe(|| unsafe {
            self.display.create_context(&self.config, &attributes)
        })) {
            Ok(Ok(context)) => ContextCreationAttempt::Created(context),
            Ok(Err(error)) if matches!(error.error_kind(), GlutinErrorKind::NotSupported(_)) => {
                ContextCreationAttempt::Unsupported(unsupported_profile_error(
                    self.profile,
                    PresentationFailureStage::CreateContext,
                    error,
                ))
            }
            Ok(Err(error)) => ContextCreationAttempt::Failed(
                PresentationError::driver(PresentationFailureStage::CreateContext, &error)
                    .with_capabilities(capabilities),
            ),
            Err(payload) => ContextCreationAttempt::Failed(
                PresentationError::contract(
                    PresentationFailureStage::CreateContext,
                    PresentationErrorKind::Driver,
                    bounded_panic_payload(payload.as_ref()),
                )
                .with_capabilities(capabilities),
            ),
        }
    }

    fn attach(
        self,
        window: Window,
        descriptor: SurfaceDescriptor,
    ) -> Result<LinuxPresentedWindow, PresenterAttachError> {
        let capabilities = self.profile.capabilities();
        let contract =
            PresentationContract::new_with_capabilities(descriptor, self.limits, capabilities)
                .map_err(PresenterAttachError::BeforeOwnership)?;
        let window_display = window
            .display_handle()
            .map_err(|error| {
                PresentationError::contract(
                    PresentationFailureStage::DisplayHandle,
                    PresentationErrorKind::Driver,
                    error,
                )
                .with_capabilities(capabilities)
            })
            .map_err(PresenterAttachError::BeforeOwnership)?;
        if window_display.as_raw() != self.raw_display {
            return Err(PresenterAttachError::BeforeOwnership(
                PresentationError::contract(
                    PresentationFailureStage::DisplayHandle,
                    PresentationErrorKind::SurfaceMismatch,
                    "window display identity differs from the prepared EGL display",
                )
                .with_capabilities(capabilities),
            ));
        }
        let window_handle = window
            .window_handle()
            .map_err(|error| {
                PresentationError::contract(
                    PresentationFailureStage::WindowHandle,
                    PresentationErrorKind::Driver,
                    error,
                )
                .with_capabilities(capabilities)
            })
            .map_err(PresenterAttachError::BeforeOwnership)?;
        let raw_window = window_handle.as_raw();
        validate_window_backend(raw_window, self.backend, self.x11_visual_id).map_err(|error| {
            PresenterAttachError::BeforeOwnership(error.with_capabilities(capabilities))
        })?;
        let not_current = match self.create_context(raw_window) {
            ContextCreationAttempt::Created(context) => context,
            ContextCreationAttempt::Unsupported(primary) => {
                return match retire_unsupported_profile(self, window, contract, &primary) {
                    PresentationTeardownOutcome::WrappersReleased(report) => Err(
                        PresenterAttachError::UnsupportedAndReleased(primary, report),
                    ),
                    teardown @ PresentationTeardownOutcome::RetainedAfterTeardownFailure(_) => {
                        Err(PresenterAttachError::AfterOwnership(
                            PresentationStartupFailure::new(primary, teardown),
                        ))
                    }
                };
            }
            ContextCreationAttempt::Failed(error) => {
                return Err(PresenterAttachError::BeforeOwnership(error));
            }
        };

        let mut presenter = LinuxPresentedWindow {
            backend: self.backend,
            config: Some(self.config),
            display: Some(self.display),
            context: Some(not_current.treat_as_possibly_current()),
            surface: None,
            gl: None,
            extent_query: None,
            window: Some(window),
            contract,
            teardown_complete: false,
            _owner_thread: PhantomData,
        };
        if let Err(primary) = load_extent_query(&mut presenter) {
            return Err(PresenterAttachError::AfterOwnership(failed_owned_startup(
                presenter, primary,
            )));
        }
        let startup = if presenter.contract.state() == PresentationState::Active {
            presenter
                .activate_surface(PresentationFailureStage::CreateSurface)
                .and_then(|()| presenter.verify_selected_capabilities())
        } else {
            presenter.verify_suspended_startup_capabilities()
        };
        if let Err(primary) = startup.map_err(|error| error.with_capabilities(capabilities)) {
            return Err(PresenterAttachError::AfterOwnership(failed_owned_startup(
                presenter, primary,
            )));
        }
        Ok(presenter)
    }
}

fn load_extent_query(presenter: &mut LinuxPresentedWindow) -> Result<(), PresentationError> {
    let stage = PresentationFailureStage::LoadFunctions;
    let capabilities = presenter.contract.capabilities();
    let Some(display) = presenter.display.as_ref() else {
        return Err(PresentationError::contract(
            stage,
            PresentationErrorKind::TerminalState,
            "new presenter lost its retained EGL display",
        )
        .with_capabilities(capabilities));
    };
    let query = match catch_native(stage, || LoadedEglSurfaceQuery::load(display)) {
        Ok(Ok(query)) => query,
        Ok(Err(failure)) => {
            return Err(
                PresentationError::contract(stage, PresentationErrorKind::Driver, failure)
                    .with_capabilities(capabilities),
            );
        }
        Err(error) => return Err(error.with_capabilities(capabilities)),
    };
    presenter.extent_query = Some(query);
    Ok(())
}

fn retire_unsupported_profile(
    bootstrap: LinuxEglBootstrap,
    window: Window,
    mut contract: PresentationContract,
    primary: &PresentationError,
) -> PresentationTeardownOutcome {
    let mut config = Some(bootstrap.config);
    let mut display = Some(bootstrap.display);
    let mut window = Some(window);
    let release = release_wrapper(PresentationFailureStage::ReleaseContext, &mut config)
        .and_then(|()| release_wrapper(PresentationFailureStage::ReleaseContext, &mut display))
        .and_then(|()| release_wrapper(PresentationFailureStage::ReleaseContext, &mut window));
    match release {
        Ok(()) => PresentationTeardownOutcome::WrappersReleased(contract.shutdown()),
        Err(error) => {
            retain_wrapper(&mut config);
            retain_wrapper(&mut display);
            retain_wrapper(&mut window);
            contract.lose(PresentationFailureStage::ReleaseContext);
            let failure = if error.capabilities().is_some() {
                error
            } else {
                error.with_capabilities(contract.capabilities())
            };
            let report = contract.retention(&failure);
            debug_assert_eq!(Some(report.capabilities()), primary.capabilities());
            PresentationTeardownOutcome::RetainedAfterTeardownFailure(report)
        }
    }
}

fn failed_owned_startup(
    presenter: LinuxPresentedWindow,
    primary: PresentationError,
) -> PresentationStartupFailure {
    let panic_error = PresentationError::contract(
        PresentationFailureStage::ReleaseContext,
        PresentationErrorKind::Driver,
        "panic escaped partial-presenter shutdown",
    );
    let panic_fallback = presenter.contract.retention(&panic_error);
    let teardown = capture_startup_teardown(panic_fallback, || presenter.shutdown());
    PresentationStartupFailure::new(primary, teardown)
}

fn capture_startup_teardown(
    panic_fallback: PresentationRetentionReport,
    shutdown: impl FnOnce() -> Result<PresentationShutdownReport, PresentationRetentionReport>,
) -> PresentationTeardownOutcome {
    match catch_unwind(AssertUnwindSafe(shutdown)) {
        Ok(Ok(report)) => PresentationTeardownOutcome::WrappersReleased(report),
        Ok(Err(report)) => PresentationTeardownOutcome::RetainedAfterTeardownFailure(report),
        Err(_) => PresentationTeardownOutcome::RetainedAfterTeardownFailure(panic_fallback),
    }
}

trait FrameGl {
    fn get_error(&self) -> u32;
    fn bind_default_framebuffer(&self);
    fn draw_buffer(&self, buffer: u32);
    fn read_buffer(&self, buffer: u32);
    fn disable(&self, capability: u32);
    fn viewport(&self, width: i32, height: i32);
    fn clear_color(&self, rgba: [f32; 4]);
    fn clear_color_buffer(&self);
    fn reset_pack_row_length(&self);
    fn read_rgba8_pixel(&self, x: i32, y: i32, destination: &mut [u8; 4]);
}

struct GleamFrameGl<'gl>(&'gl dyn gl::Gl);

impl FrameGl for GleamFrameGl<'_> {
    fn get_error(&self) -> u32 {
        self.0.get_error()
    }

    fn bind_default_framebuffer(&self) {
        self.0.bind_framebuffer(gl::FRAMEBUFFER, 0);
    }

    fn draw_buffer(&self, buffer: u32) {
        self.0.draw_buffers(&[buffer]);
    }

    fn read_buffer(&self, buffer: u32) {
        self.0.read_buffer(buffer);
    }

    fn disable(&self, capability: u32) {
        self.0.disable(capability);
    }

    fn viewport(&self, width: i32, height: i32) {
        self.0.viewport(0, 0, width, height);
    }

    fn clear_color(&self, rgba: [f32; 4]) {
        self.0.clear_color(rgba[0], rgba[1], rgba[2], rgba[3]);
    }

    fn clear_color_buffer(&self) {
        self.0.clear(gl::COLOR_BUFFER_BIT);
    }

    fn reset_pack_row_length(&self) {
        self.0.pixel_store_i(gl::PACK_ROW_LENGTH, 0);
    }

    fn read_rgba8_pixel(&self, x: i32, y: i32, destination: &mut [u8; 4]) {
        self.0
            .read_pixels_into_buffer(x, y, 1, 1, gl::RGBA, gl::UNSIGNED_BYTE, destination);
    }
}

/// Callback-scoped, capability-limited direct GPU target.
///
/// There is deliberately no method returning GL, EGL, Wayland, X11, winit, or
/// raw handles. The target cannot outlive the submission callback.
pub struct DirectFrameTarget<'frame> {
    operations: &'frame dyn FrameGl,
    request: DirectFrameRequest,
    default_buffer: u32,
    complete_frames: u8,
    diagnostic_sample: Option<[u8; 4]>,
    terminal_fault: Option<DirectRenderError>,
}

impl DirectFrameTarget<'_> {
    /// Exact surface identity for this callback.
    #[must_use]
    pub const fn surface(&self) -> SurfaceId {
        self.request.surface()
    }

    /// Exact current physical target size.
    #[must_use]
    pub const fn size(&self) -> PhysicalSize {
        self.request.size()
    }

    /// Exact producer sequence.
    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.request.sequence()
    }

    /// Fills the complete native back buffer with one bounded color.
    ///
    /// A single center pixel is read back before swap as diagnostic evidence
    /// that drawing reached the native default framebuffer. This synchronous
    /// readback is limited to this transitional solid-frame proof; it is not a
    /// software presentation path and is not the future `WebRender` frame path.
    ///
    /// # Errors
    ///
    /// Returns a bounded rendering failure for repeated/missing completion,
    /// GL failure, or a native-back-buffer diagnostic mismatch.
    pub fn clear_solid(&mut self, color: SolidColor) -> Result<(), DirectRenderError> {
        if let Some(error) = self.terminal_fault {
            return Err(error);
        }
        if self.complete_frames != 0 {
            return Err(DirectRenderError::MultipleCompleteFrames);
        }
        let preexisting = self.operations.get_error();
        if preexisting != gl::NO_ERROR {
            return Err(self.latch_terminal(DirectRenderError::GlError(preexisting)));
        }
        let width =
            i32::try_from(self.request.size().width).map_err(|_| DirectRenderError::Rejected)?;
        let height =
            i32::try_from(self.request.size().height).map_err(|_| DirectRenderError::Rejected)?;
        self.operations.bind_default_framebuffer();
        self.operations.draw_buffer(self.default_buffer);
        self.operations.read_buffer(self.default_buffer);
        self.operations.disable(gl::SCISSOR_TEST);
        self.operations.disable(gl::FRAMEBUFFER_SRGB);
        self.operations.viewport(width, height);
        let [red, green, blue, alpha] = color.rgba();
        self.operations.clear_color([
            f32::from(red) / 255.0,
            f32::from(green) / 255.0,
            f32::from(blue) / 255.0,
            f32::from(alpha) / 255.0,
        ]);
        self.operations.clear_color_buffer();
        self.operations.reset_pack_row_length();
        let mut observed = [0_u8; 4];
        self.operations
            .read_rgba8_pixel(width / 2, height / 2, &mut observed);
        // Inspect GL before interpreting bytes. The destination is initialized,
        // but a failed/context-lost read has no diagnostic pixel semantics.
        let error = self.operations.get_error();
        if error != gl::NO_ERROR {
            return Err(self.latch_terminal(DirectRenderError::GlError(error)));
        }
        let expected = color.rgba();
        if !rgba_within_one(expected, observed) {
            return Err(
                self.latch_terminal(DirectRenderError::DiagnosticMismatch { expected, observed })
            );
        }
        self.complete_frames = 1;
        self.diagnostic_sample = Some(observed);
        Ok(())
    }

    fn terminal_fault(&self) -> Option<DirectRenderError> {
        self.terminal_fault
    }

    fn latch_terminal(&mut self, error: DirectRenderError) -> DirectRenderError {
        *self.terminal_fault.get_or_insert(error)
    }

    fn finish(
        self,
        renderer_result: Result<(), DirectRenderError>,
    ) -> Result<[u8; 4], DirectRenderError> {
        if let Some(error) = self.terminal_fault {
            return Err(error);
        }
        renderer_result?;
        if self.complete_frames != 1 {
            return Err(DirectRenderError::NoCompleteFrame);
        }
        self.diagnostic_sample
            .ok_or(DirectRenderError::NoCompleteFrame)
    }
}

/// Same-thread owner of one native window and its direct EGL presentation path.
///
/// ```compile_fail
/// fn assert_send<T: Send>() {}
/// assert_send::<wild_buzzard_linux_presenter::LinuxPresentedWindow>();
/// ```
pub struct LinuxPresentedWindow {
    backend: LinuxPresentationBackend,
    config: Option<Config>,
    display: Option<Display>,
    context: Option<PossiblyCurrentContext>,
    surface: Option<Surface<WindowSurface>>,
    gl: Option<Rc<dyn gl::Gl>>,
    extent_query: Option<LoadedEglSurfaceQuery>,
    window: Option<Window>,
    contract: PresentationContract,
    teardown_complete: bool,
    _owner_thread: PhantomData<Rc<()>>,
}

impl LinuxPresentedWindow {
    /// Selected native protocol.
    #[must_use]
    pub const fn backend(&self) -> LinuxPresentationBackend {
        self.backend
    }

    /// Verified immutable acceleration and reset facts for this owner.
    #[must_use]
    pub const fn capabilities(&self) -> LinuxPresentationCapabilities {
        self.contract.capabilities()
    }

    /// Exact current surface descriptor.
    #[must_use]
    pub const fn descriptor(&self) -> SurfaceDescriptor {
        self.contract.descriptor()
    }

    /// Current presentation lifecycle.
    #[must_use]
    pub const fn presentation_state(&self) -> PresentationState {
        self.contract.state()
    }

    /// Compares a value-only winit event identity without exporting a window handle.
    #[must_use]
    pub fn matches_window_id(&self, id: WindowId) -> bool {
        self.window.as_ref().is_some_and(|window| window.id() == id)
    }

    /// Returns the current native scale value for boundary validation.
    #[must_use]
    pub fn native_scale_factor(&self) -> f64 {
        self.window
            .as_ref()
            .map_or(1.0, winit::window::Window::scale_factor)
    }

    /// Returns current native inner dimensions for boundary validation.
    #[must_use]
    pub fn native_inner_size(&self) -> (u32, u32) {
        self.window.as_ref().map_or((0, 0), |window| {
            let size = window.inner_size();
            (size.width, size.height)
        })
    }

    /// Requests a native redraw callback.
    pub fn request_redraw(&self) {
        if let Some(window) = self.window.as_ref() {
            window.request_redraw();
        }
    }

    /// Requests a value-only native inner extent.
    ///
    /// A synchronously applied extent is returned when the window system
    /// provides one. This does not mutate the presenter's EGL contract; the
    /// caller must still process the resulting native resize event through the
    /// exact checked resize transition.
    #[must_use]
    pub fn request_inner_size(&self, size: PhysicalSize) -> Option<PhysicalSize> {
        let native = self
            .window
            .as_ref()?
            .request_inner_size(winit::dpi::PhysicalSize::new(size.width, size.height))?;
        PhysicalSize::new(native.width, native.height).ok()
    }

    /// Confirms a synchronously reported window size against the exact retained
    /// EGL surface before a caller publishes a checked resize transition.
    ///
    /// An extent mismatch is expected while a display server or EGL catches up.
    /// X11 returns [`NativeExtentConfirmation::Pending`] and waits for its
    /// native resize event. Wayland returns
    /// [`NativeExtentConfirmation::ReadyForCheckedResize`] because its EGL
    /// window surface cannot report the synchronously accepted winit extent
    /// until the ordinary checked resize transaction recreates it. Neither
    /// observation changes a native owner or presentation contract. Missing
    /// owners, rejected EGL queries, and query panics are typed terminal
    /// presentation faults.
    ///
    /// # Errors
    ///
    /// Returns a terminal native or owner error when the exact retained EGL
    /// surface cannot be queried safely.
    pub fn confirm_native_extent(
        &mut self,
        expected: PhysicalSize,
    ) -> Result<NativeExtentConfirmation, PresentationError> {
        let descriptor = self.contract.descriptor();
        self.contract.check_live(descriptor.id)?;
        if let Some(confirmation) =
            native_extent_confirmation_without_query(self.contract.state(), expected)
        {
            return Ok(confirmation);
        }

        match self.query_surface_dimensions(PresentationFailureStage::ResizeSurface, expected) {
            Ok(result) => match classify_native_extent_confirmation(self.backend, result) {
                Ok(confirmation) => Ok(confirmation),
                Err(failure) => {
                    self.contract.lose(PresentationFailureStage::ResizeSurface);
                    Err(PresentationError::contract(
                        PresentationFailureStage::ResizeSurface,
                        PresentationErrorKind::Driver,
                        failure,
                    ))
                }
            },
            Err(error) => {
                self.contract.lose(PresentationFailureStage::ResizeSurface);
                Err(error)
            }
        }
    }

    /// Clones the private GL dispatch table only for the crate's nested
    /// `WebRender` owner, after proving this exact surface/context current.
    ///
    /// This is deliberately crate-private: neither the GL table nor native
    /// ownership becomes part of the public presentation contract.
    pub(crate) fn clone_current_gl_for_webrender(
        &mut self,
    ) -> Result<Rc<dyn gl::Gl>, PresentationError> {
        let descriptor = self.contract.descriptor();
        self.contract.check_live(descriptor.id)?;
        if self.contract.state() == PresentationState::Suspended {
            return Err(PresentationError::contract(
                PresentationFailureStage::MakeCurrent,
                PresentationErrorKind::Suspended,
                "WebRender initialization requires a nonzero current window surface",
            ));
        }
        self.ensure_current(PresentationFailureStage::MakeCurrent)?;
        self.verify_surface_dimensions(PresentationFailureStage::MakeCurrent, descriptor.size)?;
        self.check_internal_gl_error(PresentationFailureStage::LoadFunctions)?;
        let Some(gl) = self.gl.as_ref() else {
            return Err(self.terminal_invariant(
                PresentationFailureStage::LoadFunctions,
                "current presenter has no private GL function table",
            ));
        };
        Ok(Rc::clone(gl))
    }

    /// Rechecks frame admission before any `WebRender` resource or transaction
    /// staging. The same request is checked again immediately before GL use.
    pub(crate) fn validate_webrender_frame(
        &self,
        request: DirectFrameRequest,
    ) -> Result<u64, PresentationError> {
        self.contract.admit_frame(request)
    }

    /// Makes the exact EGL surface current, rechecks its native extent, and
    /// prepares the native default framebuffer for one internally owned
    /// `WebRender` draw. No GL authority leaves this crate.
    pub(crate) fn prepare_webrender_frame(
        &mut self,
        request: DirectFrameRequest,
    ) -> Result<u64, PresentationError> {
        let rgba8_bytes = self.contract.admit_frame(request)?;
        self.ensure_current(PresentationFailureStage::MakeCurrent)?;
        self.verify_surface_dimensions(PresentationFailureStage::DrawFrame, request.size())?;
        let single_buffered = match self.surface.as_ref() {
            Some(surface) => match catch_native(PresentationFailureStage::DrawFrame, || {
                surface.is_single_buffered()
            }) {
                Ok(value) => value,
                Err(error) => {
                    self.contract.lose(PresentationFailureStage::DrawFrame);
                    return Err(error);
                }
            },
            None => {
                return Err(self.terminal_invariant(
                    PresentationFailureStage::DrawFrame,
                    "admitted WebRender frame has no owned native surface",
                ));
            }
        };
        let Some(gl) = self.gl.as_ref() else {
            return Err(self.terminal_invariant(
                PresentationFailureStage::LoadFunctions,
                "current presenter has no private GL function table",
            ));
        };
        let preexisting = catch_native(PresentationFailureStage::DrawFrame, || gl.get_error())?;
        if preexisting != gl::NO_ERROR {
            self.contract.lose(PresentationFailureStage::DrawFrame);
            return Err(PresentationError::contract(
                PresentationFailureStage::DrawFrame,
                classify_gl_error(preexisting),
                format_args!("preexisting GL error before WebRender {preexisting:#x}"),
            ));
        }
        let default_buffer = if single_buffered { gl::FRONT } else { gl::BACK };
        if let Err(error) = catch_native(PresentationFailureStage::DrawFrame, || {
            gl.bind_framebuffer(gl::FRAMEBUFFER, 0);
            gl.draw_buffers(&[default_buffer]);
        }) {
            self.contract.lose(PresentationFailureStage::DrawFrame);
            return Err(error);
        }
        self.check_internal_gl_error(PresentationFailureStage::DrawFrame)?;
        Ok(rgba8_bytes)
    }

    /// Checks GL immediately after `WebRender` update or rendering. The first
    /// observed GL/context fault is terminal and cannot be remapped by the
    /// higher-level adapter.
    pub(crate) fn verify_webrender_gl(&mut self) -> Result<(), PresentationError> {
        self.check_internal_gl_error(PresentationFailureStage::DrawFrame)
    }

    /// Submits the current native back buffer and commits the presenter's
    /// sequence only after EGL accepts the exact swap.
    pub(crate) fn swap_webrender_frame(
        &mut self,
        request: DirectFrameRequest,
    ) -> Result<(), PresentationError> {
        // Rechecking admission prevents a future refactor from committing a
        // sequence which was never valid for this exact presenter.
        self.contract.admit_frame(request)?;
        let Some(surface) = self.surface.as_ref() else {
            return Err(self.terminal_invariant(
                PresentationFailureStage::SwapBuffers,
                "rendered WebRender frame lost its native surface before swap",
            ));
        };
        let Some(context) = self.context.as_ref() else {
            return Err(self.terminal_invariant(
                PresentationFailureStage::SwapBuffers,
                "rendered WebRender frame lost its EGL context before swap",
            ));
        };
        if let Err(error) = catch_glutin(PresentationFailureStage::SwapBuffers, || {
            surface.swap_buffers(context)
        }) {
            self.contract.lose(PresentationFailureStage::SwapBuffers);
            return Err(error);
        }
        self.contract.commit_frame(request.sequence());
        Ok(())
    }

    /// Activates this exact context for renderer-owned GL deletion. When the
    /// window surface is suspended, a checked EGL surfaceless binding is used;
    /// failure leaves renderer and native owners eligible only for fail-closed
    /// retention.
    pub(crate) fn make_current_for_webrender_teardown(&mut self) -> Result<(), PresentationError> {
        if self.surface.is_some() {
            self.ensure_current(PresentationFailureStage::ReleaseContext)?;
        } else {
            let Some(context) = self.context.as_ref() else {
                return Err(self.terminal_invariant(
                    PresentationFailureStage::ReleaseContext,
                    "WebRender teardown lost its owned EGL context",
                ));
            };
            if let Err(error) = catch_glutin(PresentationFailureStage::ReleaseContext, || {
                context.make_current_surfaceless()
            }) {
                self.contract.lose(PresentationFailureStage::ReleaseContext);
                return Err(error);
            }
            let current = match catch_native(PresentationFailureStage::ReleaseContext, || {
                context.is_current()
            }) {
                Ok(value) => value,
                Err(error) => {
                    self.contract.lose(PresentationFailureStage::ReleaseContext);
                    return Err(error);
                }
            };
            if !current {
                return Err(self.terminal_invariant(
                    PresentationFailureStage::ReleaseContext,
                    "EGL reported surfaceless success without making the context current",
                ));
            }
        }
        self.check_internal_gl_error(PresentationFailureStage::ReleaseContext)
    }

    /// Retains every still-extant native presenter owner after the nested
    /// renderer can no longer be safely deinitialized.
    pub(crate) fn retain_after_webrender_failure(
        mut self,
        error: &PresentationError,
    ) -> PresentationRetentionReport {
        self.retain_after_teardown_failure(error)
    }

    /// Enables or disables IME event delivery.
    pub fn set_ime_allowed(&self, allowed: bool) {
        if let Some(window) = self.window.as_ref() {
            window.set_ime_allowed(allowed);
        }
    }

    /// Sets a logical IME candidate rectangle without exposing winit values.
    pub fn set_ime_cursor_area(&self, x: f64, y: f64, width: f64, height: f64) {
        if let Some(window) = self.window.as_ref() {
            window.set_ime_cursor_area(LogicalPosition::new(x, y), LogicalSize::new(width, height));
        }
    }

    /// Updates the exact native pixel extent before the corresponding event escapes.
    ///
    /// # Errors
    ///
    /// Returns a staged failure for a foreign identity, excessive dimensions,
    /// terminal presenter, or failed surface transition.
    pub fn resize(
        &mut self,
        surface: SurfaceId,
        size: PhysicalSize,
    ) -> Result<(), PresentationError> {
        self.contract.check_resize(surface, size)?;
        if size.width == 0 || size.height == 0 {
            self.deactivate_surface(PresentationFailureStage::ResizeSurface)?;
            self.contract.commit_resize(size);
            return Ok(());
        }

        if self.surface.is_none() {
            self.contract.commit_resize(size);
            self.activate_surface(PresentationFailureStage::ResizeSurface)?;
        } else {
            // A Wayland configure may precede observable EGL extent mutation
            // on an existing `wl_egl_window`. Recreate only the EGL window
            // surface wrapper around the persistent context, then require its
            // queried extent to match before committing the Rust contract.
            self.deactivate_surface(PresentationFailureStage::ResizeSurface)?;
            self.activate_surface_for_size(PresentationFailureStage::ResizeSurface, size)?;
            self.contract.commit_resize(size);
        }
        Ok(())
    }

    /// Updates logical scale independently of native surface pixel size.
    ///
    /// # Errors
    ///
    /// Returns an error for a foreign identity or terminal presenter.
    pub fn update_scale(
        &mut self,
        surface: SurfaceId,
        scale: ScaleFactor,
    ) -> Result<(), PresentationError> {
        self.contract.update_scale(surface, scale)
    }

    /// Removes the EGL window surface while retaining the native window/context owner.
    ///
    /// # Errors
    ///
    /// Returns an error for a terminal presenter or if EGL cannot be made
    /// non-current before removing the surface.
    pub fn suspend(&mut self) -> Result<(), PresentationError> {
        self.contract.check_live(self.contract.descriptor().id)?;
        self.deactivate_surface(PresentationFailureStage::ResizeSurface)?;
        self.contract.suspend();
        Ok(())
    }

    /// Recreates the exact EGL surface after suspension when size is nonzero.
    ///
    /// # Errors
    ///
    /// Returns a staged error if the presenter is terminal or native surface
    /// recreation/current-context setup fails.
    pub fn resume(&mut self) -> Result<(), PresentationError> {
        self.contract.check_live(self.contract.descriptor().id)?;
        if self.contract.descriptor().size.width == 0 || self.contract.descriptor().size.height == 0
        {
            return Ok(());
        }
        if self.surface.is_none() {
            self.activate_surface(PresentationFailureStage::ResizeSurface)?;
        }
        self.contract.resume();
        Ok(())
    }

    /// Executes a capability-scoped direct renderer and submits one EGL swap.
    ///
    /// # Errors
    ///
    /// Returns an admission, renderer, GL, current-context, integrity, or swap
    /// failure. Renderer panic is contained and permanently loses the presenter.
    pub fn submit_direct(
        &mut self,
        request: DirectFrameRequest,
        renderer: &mut dyn DirectRenderer,
    ) -> Result<SwapSubmissionReceipt, PresentationError> {
        enum RenderOutcome {
            Complete([u8; 4]),
            Rejected(DirectRenderError),
            Panicked(String),
        }

        let rgba8_bytes = self.contract.admit_frame(request)?;
        self.ensure_current(PresentationFailureStage::MakeCurrent)?;
        self.verify_surface_dimensions(PresentationFailureStage::DrawFrame, request.size())?;
        let single_buffered = match self.surface.as_ref() {
            Some(surface) => match catch_native(PresentationFailureStage::DrawFrame, || {
                surface.is_single_buffered()
            }) {
                Ok(value) => value,
                Err(error) => {
                    self.contract.lose(PresentationFailureStage::DrawFrame);
                    return Err(error);
                }
            },
            None => {
                return Err(self.terminal_invariant(
                    PresentationFailureStage::DrawFrame,
                    "admitted frame has no owned native surface",
                ));
            }
        };
        let default_buffer = if single_buffered { gl::FRONT } else { gl::BACK };
        let render_outcome = {
            let Some(gl) = self.gl.as_ref() else {
                return Err(self.terminal_invariant(
                    PresentationFailureStage::LoadFunctions,
                    "current presenter has no GL function table",
                ));
            };
            let operations = GleamFrameGl(gl.as_ref());
            let mut target = DirectFrameTarget {
                operations: &operations,
                request,
                default_buffer,
                complete_frames: 0,
                diagnostic_sample: None,
                terminal_fault: None,
            };
            match catch_unwind(AssertUnwindSafe(|| renderer.render(&mut target))) {
                Ok(renderer_result) => match target.finish(renderer_result) {
                    Ok(sample) => RenderOutcome::Complete(sample),
                    Err(error) => RenderOutcome::Rejected(error),
                },
                Err(payload) => match target.terminal_fault() {
                    Some(error) => RenderOutcome::Rejected(error),
                    None => RenderOutcome::Panicked(bounded_panic_payload(payload.as_ref())),
                },
            }
        };
        let diagnostic_sample = match render_outcome {
            RenderOutcome::Complete(sample) => sample,
            RenderOutcome::Rejected(error) => return Err(self.map_render_error(error)),
            RenderOutcome::Panicked(detail) => {
                self.contract.lose(PresentationFailureStage::DrawFrame);
                return Err(PresentationError::contract(
                    PresentationFailureStage::DrawFrame,
                    PresentationErrorKind::Driver,
                    detail,
                ));
            }
        };

        let Some(surface) = self.surface.as_ref() else {
            return Err(self.terminal_invariant(
                PresentationFailureStage::SwapBuffers,
                "completed frame lost its owned native surface before swap",
            ));
        };
        let Some(context) = self.context.as_ref() else {
            return Err(self.terminal_invariant(
                PresentationFailureStage::SwapBuffers,
                "completed frame lost its owned EGL context before swap",
            ));
        };
        let swap = catch_glutin(PresentationFailureStage::SwapBuffers, || {
            surface.swap_buffers(context)
        });
        if let Err(error) = swap {
            self.contract.lose(PresentationFailureStage::SwapBuffers);
            return Err(error);
        }
        self.contract.commit_frame(request.sequence());
        Ok(SwapSubmissionReceipt::new(
            request,
            self.contract.capabilities(),
            rgba8_bytes,
            diagnostic_sample,
        ))
    }

    /// Convenience source for the first direct-GPU native-window proof.
    ///
    /// # Errors
    ///
    /// Returns the same staged admission, GL, integrity, or swap failures as
    /// [`Self::submit_direct`].
    pub fn submit_solid_frame(
        &mut self,
        frame: SolidColorFrame,
    ) -> Result<SwapSubmissionReceipt, PresentationError> {
        let mut renderer = SolidRenderer(frame.color());
        self.submit_direct(frame.request(), &mut renderer)
    }

    /// Explicitly verifies EGL is non-current and releases every Rust wrapper
    /// in dependency order, with the native window wrapper last.
    ///
    /// Glutin does not report the result of native EGL destructor calls made by
    /// `Drop`. If an unbind/check/release operation errors or panics, every
    /// still-extant native owner is deliberately retained and the returned
    /// report identifies that terminal outcome.
    ///
    /// # Errors
    ///
    /// Returns retention evidence when teardown cannot prove normal wrapper
    /// release. The retained native objects intentionally remain leaked.
    pub fn shutdown(mut self) -> Result<PresentationShutdownReport, PresentationRetentionReport> {
        match catch_unwind(AssertUnwindSafe(|| self.teardown())) {
            Ok(result) => result,
            Err(payload) => {
                let error = PresentationError::contract(
                    PresentationFailureStage::ReleaseContext,
                    PresentationErrorKind::Driver,
                    bounded_panic_payload(payload.as_ref()),
                );
                Err(self.retain_after_teardown_failure(&error))
            }
        }
    }

    fn activate_surface(
        &mut self,
        stage: PresentationFailureStage,
    ) -> Result<(), PresentationError> {
        let descriptor = self.contract.descriptor();
        self.activate_surface_for_size(stage, descriptor.size)
    }

    fn activate_surface_for_size(
        &mut self,
        stage: PresentationFailureStage,
        size: PhysicalSize,
    ) -> Result<(), PresentationError> {
        if size.width == 0 || size.height == 0 {
            return Ok(());
        }
        let Some(window) = self.window.as_ref() else {
            return Err(self.terminal_invariant(stage, "presenter lost its owned native window"));
        };
        let raw_window = match window.window_handle() {
            Ok(handle) => handle.as_raw(),
            Err(error) => {
                self.contract.lose(PresentationFailureStage::WindowHandle);
                return Err(PresentationError::contract(
                    PresentationFailureStage::WindowHandle,
                    PresentationErrorKind::Driver,
                    error,
                ));
            }
        };
        let Some(width) = NonZeroU32::new(size.width) else {
            return Err(
                self.terminal_invariant(stage, "active surface unexpectedly has zero width")
            );
        };
        let Some(height) = NonZeroU32::new(size.height) else {
            return Err(
                self.terminal_invariant(stage, "active surface unexpectedly has zero height")
            );
        };
        let attributes = SurfaceAttributesBuilder::<WindowSurface>::new()
            .with_srgb(Some(true))
            .build(raw_window, width, height);
        let Some(display) = self.display.as_ref() else {
            return Err(self.terminal_invariant(stage, "presenter lost its owned EGL display"));
        };
        let Some(config) = self.config.as_ref() else {
            return Err(self.terminal_invariant(stage, "presenter lost its owned EGL config"));
        };
        // SAFETY: the raw handle is borrowed from the `Window` owned by `self`.
        // The matching display/config were selected before creation, the X11
        // visual was applied to that window, and teardown drops this surface
        // before the native window on the same owner thread.
        let surface = match catch_glutin(stage, || unsafe {
            display.create_window_surface(config, &attributes)
        }) {
            Ok(surface) => surface,
            Err(error) => {
                self.contract.lose(stage);
                return Err(error);
            }
        };
        self.surface = Some(surface);
        self.ensure_current(PresentationFailureStage::MakeCurrent)?;
        self.verify_surface_dimensions(stage, size)?;
        let Some(surface) = self.surface.as_ref() else {
            return Err(self.terminal_invariant(
                PresentationFailureStage::ConfigureSwap,
                "newly created EGL surface disappeared before swap configuration",
            ));
        };
        let Some(context) = self.context.as_ref() else {
            return Err(self.terminal_invariant(
                PresentationFailureStage::ConfigureSwap,
                "presenter lost its EGL context before swap configuration",
            ));
        };
        let swap = catch_glutin(PresentationFailureStage::ConfigureSwap, || {
            surface.set_swap_interval(context, SwapInterval::DontWait)
        });
        if let Err(error) = swap {
            self.contract.lose(PresentationFailureStage::ConfigureSwap);
            return Err(error);
        }
        self.load_gl_if_absent()?;
        Ok(())
    }

    fn verify_suspended_startup_capabilities(&mut self) -> Result<(), PresentationError> {
        let stage = PresentationFailureStage::MakeCurrent;
        let capabilities = self.contract.capabilities();
        let Some(context) = self.context.as_ref() else {
            return Err(self
                .terminal_invariant(stage, "suspended startup lost its owned EGL context")
                .with_capabilities(capabilities));
        };
        if let Err(error) = catch_glutin(stage, || context.make_current_surfaceless()) {
            self.contract.lose(stage);
            return Err(error.with_capabilities(capabilities));
        }
        let current = match catch_native(stage, || context.is_current()) {
            Ok(current) => current,
            Err(error) => {
                self.contract.lose(stage);
                return Err(error.with_capabilities(capabilities));
            }
        };
        if !current {
            return Err(self
                .terminal_invariant(
                    stage,
                    "EGL reported surfaceless startup success without a current context",
                )
                .with_capabilities(capabilities));
        }

        self.load_gl_if_absent()?;
        self.verify_selected_capabilities()?;

        let Some(context) = self.context.as_ref() else {
            return Err(self
                .terminal_invariant(stage, "verified suspended startup lost its EGL context")
                .with_capabilities(capabilities));
        };
        if let Err(error) = catch_glutin(stage, || context.make_not_current_in_place()) {
            self.contract.lose(stage);
            return Err(error.with_capabilities(capabilities));
        }
        let remains_current = match catch_native(stage, || context.is_current()) {
            Ok(current) => current,
            Err(error) => {
                self.contract.lose(stage);
                return Err(error.with_capabilities(capabilities));
            }
        };
        if remains_current {
            return Err(self
                .terminal_invariant(
                    stage,
                    "EGL context remained current after suspended startup verification",
                )
                .with_capabilities(capabilities));
        }
        Ok(())
    }

    fn load_gl_if_absent(&mut self) -> Result<(), PresentationError> {
        if self.gl.is_some() {
            return Ok(());
        }
        let stage = PresentationFailureStage::LoadFunctions;
        let capabilities = self.contract.capabilities();
        let Some(display) = self.display.as_ref() else {
            return Err(self
                .terminal_invariant(stage, "presenter lost its EGL display before GL loading")
                .with_capabilities(capabilities));
        };
        let gl = match catch_native(stage, || load_desktop_gl(display, self.backend)) {
            Ok(Ok(gl)) => gl,
            Ok(Err(error)) | Err(error) => {
                self.contract.lose(stage);
                return Err(error.with_capabilities(capabilities));
            }
        };
        self.gl = Some(gl);
        Ok(())
    }

    fn deactivate_surface(
        &mut self,
        stage: PresentationFailureStage,
    ) -> Result<(), PresentationError> {
        if self.surface.is_none() {
            return Ok(());
        }
        let Some(context) = self.context.as_ref() else {
            return Err(self
                .terminal_invariant(stage, "native surface exists without its owned EGL context"));
        };
        let is_current = match catch_native(stage, || context.is_current()) {
            Ok(value) => value,
            Err(error) => {
                self.contract.lose(stage);
                return Err(error);
            }
        };
        if is_current {
            let result = catch_glutin(stage, || context.make_not_current_in_place());
            if let Err(error) = result {
                self.contract.lose(stage);
                return Err(error);
            }
        }
        if let Err(error) = release_wrapper(stage, &mut self.surface) {
            self.contract.lose(stage);
            return Err(error);
        }
        Ok(())
    }

    fn ensure_current(&mut self, stage: PresentationFailureStage) -> Result<(), PresentationError> {
        let Some(surface) = self.surface.as_ref() else {
            if self.contract.state() != PresentationState::Suspended {
                return Err(self
                    .terminal_invariant(stage, "active presenter lost its owned native surface"));
            }
            return Err(PresentationError::contract(
                stage,
                PresentationErrorKind::Suspended,
                "presenter has no native surface while suspended",
            ));
        };
        let Some(context) = self.context.as_ref() else {
            return Err(self.terminal_invariant(stage, "presenter lost its owned EGL context"));
        };
        let current = match catch_native(stage, || {
            context.is_current() && surface.is_current(context)
        }) {
            Ok(value) => value,
            Err(error) => {
                self.contract.lose(stage);
                return Err(error);
            }
        };
        if current {
            return Ok(());
        }
        if let Err(error) = catch_glutin(stage, || context.make_current(surface)) {
            self.contract.lose(stage);
            return Err(error);
        }
        let exact_current = match catch_native(stage, || {
            context.is_current() && surface.is_current(context)
        }) {
            Ok(value) => value,
            Err(error) => {
                self.contract.lose(stage);
                return Err(error);
            }
        };
        if exact_current {
            Ok(())
        } else {
            self.contract.lose(stage);
            Err(PresentationError::contract(
                stage,
                PresentationErrorKind::Driver,
                "EGL reported success without making the exact surface current",
            ))
        }
    }

    fn verify_surface_dimensions(
        &mut self,
        stage: PresentationFailureStage,
        expected: PhysicalSize,
    ) -> Result<(), PresentationError> {
        let result = self.query_surface_dimensions(stage, expected);
        match result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(failure)) => {
                self.contract.lose(stage);
                Err(PresentationError::contract(
                    stage,
                    PresentationErrorKind::Driver,
                    failure,
                ))
            }
            Err(error) => {
                self.contract.lose(stage);
                Err(error)
            }
        }
    }

    fn query_surface_dimensions(
        &self,
        stage: PresentationFailureStage,
        expected: PhysicalSize,
    ) -> Result<Result<(), EglExtentQueryFailure>, PresentationError> {
        let Some(display) = self.display.as_ref() else {
            return Err(PresentationError::contract(
                stage,
                PresentationErrorKind::TerminalState,
                "native display is absent during extent verification",
            ));
        };
        let Some(surface) = self.surface.as_ref() else {
            return Err(PresentationError::contract(
                stage,
                PresentationErrorKind::TerminalState,
                "native surface is absent during extent verification",
            ));
        };
        let Some(query) = self.extent_query.as_ref() else {
            return Err(PresentationError::contract(
                stage,
                PresentationErrorKind::TerminalState,
                "checked EGL extent query is absent",
            ));
        };
        catch_native(stage, || {
            let objects = retained_egl_objects(display, surface)?;
            checked_egl_surface_extent(query, objects, expected)
        })
    }

    fn check_internal_gl_error(
        &mut self,
        stage: PresentationFailureStage,
    ) -> Result<(), PresentationError> {
        let Some(gl) = self.gl.as_ref() else {
            return Err(self.terminal_invariant(
                PresentationFailureStage::LoadFunctions,
                "presenter has no private GL function table",
            ));
        };
        let code = match catch_native(stage, || gl.get_error()) {
            Ok(code) => code,
            Err(error) => {
                self.contract.lose(stage);
                return Err(error);
            }
        };
        if code == gl::NO_ERROR {
            Ok(())
        } else {
            self.contract.lose(stage);
            Err(PresentationError::contract(
                stage,
                classify_gl_error(code),
                format_args!("GL error after internal WebRender operation {code:#x}"),
            ))
        }
    }

    fn verify_selected_capabilities(&mut self) -> Result<(), PresentationError> {
        let capabilities = self.contract.capabilities();
        let Some(gl) = self.gl.as_ref() else {
            return Err(self.terminal_invariant(
                PresentationFailureStage::LoadFunctions,
                "capability verification has no private GL function table",
            ));
        };
        let flags = query_context_integer(gl.as_ref(), GL_CONTEXT_FLAGS, capabilities)?;
        let flags = u32::try_from(flags).map_err(|_| {
            PresentationError::contract(
                PresentationFailureStage::LoadFunctions,
                PresentationErrorKind::Driver,
                "current context returned negative GL_CONTEXT_FLAGS",
            )
            .with_capabilities(capabilities)
        })?;
        let strategy =
            if capabilities.reset_protection() == LinuxResetProtection::LoseContextOnReset {
                Some(query_context_integer(
                    gl.as_ref(),
                    GL_RESET_NOTIFICATION_STRATEGY,
                    capabilities,
                )?)
            } else {
                None
            };
        if let Err(failure) = validate_context_capability_facts(capabilities, flags, strategy) {
            self.contract.lose(PresentationFailureStage::LoadFunctions);
            return Err(PresentationError::contract(
                PresentationFailureStage::LoadFunctions,
                PresentationErrorKind::Driver,
                failure,
            )
            .with_capabilities(capabilities));
        }
        Ok(())
    }

    fn map_render_error(&mut self, error: DirectRenderError) -> PresentationError {
        match error {
            DirectRenderError::GlError(code) => {
                self.contract.lose(PresentationFailureStage::DrawFrame);
                PresentationError::contract(
                    PresentationFailureStage::DrawFrame,
                    classify_gl_error(code),
                    error,
                )
            }
            DirectRenderError::DiagnosticMismatch { .. } => {
                self.contract.lose(PresentationFailureStage::DrawFrame);
                PresentationError::contract(
                    PresentationFailureStage::DrawFrame,
                    PresentationErrorKind::DiagnosticMismatch,
                    error,
                )
            }
            DirectRenderError::Rejected
            | DirectRenderError::NoCompleteFrame
            | DirectRenderError::MultipleCompleteFrames => PresentationError::contract(
                PresentationFailureStage::DrawFrame,
                PresentationErrorKind::RendererRejected,
                error,
            ),
        }
    }

    fn terminal_invariant(
        &mut self,
        stage: PresentationFailureStage,
        detail: &'static str,
    ) -> PresentationError {
        self.contract.lose(stage);
        PresentationError::contract(stage, PresentationErrorKind::TerminalState, detail)
    }

    fn teardown(&mut self) -> Result<PresentationShutdownReport, PresentationRetentionReport> {
        if self.teardown_complete {
            let error = PresentationError::contract(
                PresentationFailureStage::ReleaseContext,
                PresentationErrorKind::TerminalState,
                "presenter teardown already completed",
            );
            return Err(self.retain_after_teardown_failure(&error));
        }
        if let Some(context) = self.context.as_ref() {
            let is_current = match catch_native(PresentationFailureStage::ReleaseContext, || {
                context.is_current()
            }) {
                Ok(value) => value,
                Err(error) => return Err(self.retain_after_teardown_failure(&error)),
            };
            if is_current
                && let Err(error) = catch_glutin(PresentationFailureStage::ReleaseContext, || {
                    context.make_not_current_in_place()
                })
            {
                return Err(self.retain_after_teardown_failure(&error));
            }
            let remains_current =
                match catch_native(PresentationFailureStage::ReleaseContext, || {
                    context.is_current()
                }) {
                    Ok(value) => value,
                    Err(error) => return Err(self.retain_after_teardown_failure(&error)),
                };
            if remains_current {
                let error = PresentationError::contract(
                    PresentationFailureStage::ReleaseContext,
                    PresentationErrorKind::Driver,
                    "EGL context remained current after release admission",
                );
                return Err(self.retain_after_teardown_failure(&error));
            }
        }

        self.extent_query.take();
        if let Err(error) = release_wrapper(PresentationFailureStage::ReleaseContext, &mut self.gl)
            .and_then(|()| {
                release_wrapper(PresentationFailureStage::ReleaseContext, &mut self.surface)
            })
            .and_then(|()| {
                release_wrapper(PresentationFailureStage::ReleaseContext, &mut self.context)
            })
            .and_then(|()| {
                release_wrapper(PresentationFailureStage::ReleaseContext, &mut self.config)
            })
            .and_then(|()| {
                release_wrapper(PresentationFailureStage::ReleaseContext, &mut self.display)
            })
            .and_then(|()| {
                release_wrapper(PresentationFailureStage::ReleaseContext, &mut self.window)
            })
        {
            return Err(self.retain_after_teardown_failure(&error));
        }
        let report = self.contract.shutdown();
        self.teardown_complete = true;
        Ok(report)
    }

    fn retain_after_teardown_failure(
        &mut self,
        error: &PresentationError,
    ) -> PresentationRetentionReport {
        self.contract.lose(PresentationFailureStage::ReleaseContext);
        let report = self.contract.retention(error);
        self.extent_query.take();
        retain_wrapper(&mut self.gl);
        retain_wrapper(&mut self.surface);
        retain_wrapper(&mut self.context);
        retain_wrapper(&mut self.config);
        retain_wrapper(&mut self.display);
        retain_wrapper(&mut self.window);
        self.teardown_complete = true;
        report
    }
}

impl Drop for LinuxPresentedWindow {
    fn drop(&mut self) {
        if !self.teardown_complete && catch_unwind(AssertUnwindSafe(|| self.teardown())).is_err() {
            let error = PresentationError::contract(
                PresentationFailureStage::ReleaseContext,
                PresentationErrorKind::Driver,
                "panic escaped presenter teardown during Drop",
            );
            self.retain_after_teardown_failure(&error);
        }
    }
}

fn release_wrapper<T>(
    stage: PresentationFailureStage,
    slot: &mut Option<T>,
) -> Result<(), PresentationError> {
    catch_native(stage, || drop(slot.take()))
}

fn retain_wrapper<T>(slot: &mut Option<T>) {
    if let Some(value) = slot.take() {
        mem::forget(value);
    }
}

struct SolidRenderer(SolidColor);

impl DirectRenderer for SolidRenderer {
    fn render(&mut self, target: &mut DirectFrameTarget<'_>) -> Result<(), DirectRenderError> {
        target.clear_solid(self.0)
    }
}

fn validate_display_backend(
    handle: RawDisplayHandle,
    backend: LinuxPresentationBackend,
) -> Result<(), PresentationError> {
    let matches = matches!(
        (backend, handle),
        (
            LinuxPresentationBackend::Wayland,
            RawDisplayHandle::Wayland(_)
        ) | (LinuxPresentationBackend::X11, RawDisplayHandle::Xlib(_))
    );
    if matches {
        Ok(())
    } else {
        Err(PresentationError::contract(
            PresentationFailureStage::DisplayHandle,
            PresentationErrorKind::UnsupportedContract,
            "display handle does not match selected Wayland/X11 backend",
        ))
    }
}

fn validate_window_backend(
    handle: RawWindowHandle,
    backend: LinuxPresentationBackend,
    expected_x11_visual: Option<u32>,
) -> Result<(), PresentationError> {
    match (backend, handle) {
        (LinuxPresentationBackend::Wayland, RawWindowHandle::Wayland(_)) => Ok(()),
        (LinuxPresentationBackend::X11, RawWindowHandle::Xlib(handle)) => {
            if expected_x11_visual.is_some_and(|visual| u64::from(visual) == handle.visual_id) {
                Ok(())
            } else {
                Err(PresentationError::contract(
                    PresentationFailureStage::WindowHandle,
                    PresentationErrorKind::UnsupportedContract,
                    "X11 window visual does not match the selected EGL config",
                ))
            }
        }
        _ => Err(PresentationError::contract(
            PresentationFailureStage::WindowHandle,
            PresentationErrorKind::UnsupportedContract,
            "window handle does not match selected Wayland/X11 backend",
        )),
    }
}

fn select_configs(
    configs: impl Iterator<Item = Config>,
    backend: LinuxPresentationBackend,
) -> Result<SelectedEglConfigs, PresentationError> {
    let mut selected = SelectedEglConfigs::default();
    for config in configs {
        if !config_matches_window_contract(&config) {
            continue;
        }
        let x11_visual_id = match backend {
            LinuxPresentationBackend::Wayland => None,
            LinuxPresentationBackend::X11 => {
                let Some(visual) = config.x11_visual() else {
                    continue;
                };
                Some(u32::try_from(visual.visual_id()).map_err(|_| {
                    PresentationError::contract(
                        PresentationFailureStage::SelectConfig,
                        PresentationErrorKind::Driver,
                        "EGL X11 visual identity does not fit the winit contract",
                    )
                })?)
            }
        };
        let candidate = SelectedEglConfig {
            config,
            x11_visual_id,
        };
        let slot = if candidate.config.hardware_accelerated() {
            &mut selected.accelerated
        } else {
            &mut selected.software
        };
        let replace = slot.as_ref().is_none_or(|current| {
            config_preference_key(&candidate.config) >= config_preference_key(&current.config)
        });
        if replace {
            *slot = Some(candidate);
        }
    }
    Ok(selected)
}

fn config_matches_window_contract(config: &Config) -> bool {
    config.color_buffer_type()
        == Some(ColorBufferType::Rgb {
            r_size: 8,
            g_size: 8,
            b_size: 8,
        })
        && config.alpha_size() == 8
        && !config.float_pixels()
        && config.num_samples() == 0
        && config.srgb_capable()
        && config
            .config_surface_types()
            .contains(ConfigSurfaceTypes::WINDOW)
        && config.api().contains(Api::OPENGL)
}

fn config_preference_key(config: &Config) -> (Reverse<u8>, Reverse<u8>) {
    (Reverse(config.depth_size()), Reverse(config.stencil_size()))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ContextCapabilityFactError {
    RobustAccessMissing,
    UnexpectedRobustAccess,
    ResetStrategyMissing,
    ResetStrategyMismatch(i32),
}

impl std::fmt::Display for ContextCapabilityFactError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RobustAccessMissing => {
                formatter.write_str("current context omitted the requested robust-access flag")
            }
            Self::UnexpectedRobustAccess => formatter
                .write_str("compatible context unexpectedly reported the robust-access flag"),
            Self::ResetStrategyMissing => {
                formatter.write_str("robust context reset strategy was not queried")
            }
            Self::ResetStrategyMismatch(strategy) => write!(
                formatter,
                "robust context reported reset strategy {strategy:#x} instead of lose-context-on-reset"
            ),
        }
    }
}

fn validate_context_capability_facts(
    capabilities: LinuxPresentationCapabilities,
    flags: u32,
    reset_strategy: Option<i32>,
) -> Result<(), ContextCapabilityFactError> {
    let robust = flags & GL_CONTEXT_FLAG_ROBUST_ACCESS_BIT != 0;
    match capabilities.reset_protection() {
        LinuxResetProtection::LoseContextOnReset => {
            if !robust {
                return Err(ContextCapabilityFactError::RobustAccessMissing);
            }
            match reset_strategy {
                Some(GL_LOSE_CONTEXT_ON_RESET) => Ok(()),
                Some(strategy) => Err(ContextCapabilityFactError::ResetStrategyMismatch(strategy)),
                None => Err(ContextCapabilityFactError::ResetStrategyMissing),
            }
        }
        LinuxResetProtection::Unavailable => {
            if robust {
                Err(ContextCapabilityFactError::UnexpectedRobustAccess)
            } else {
                Ok(())
            }
        }
    }
}

fn query_context_integer(
    gl: &dyn gl::Gl,
    name: u32,
    capabilities: LinuxPresentationCapabilities,
) -> Result<i32, PresentationError> {
    let stage = PresentationFailureStage::LoadFunctions;
    let preexisting = catch_native(stage, || gl.get_error())
        .map_err(|error| error.with_capabilities(capabilities))?;
    if preexisting != gl::NO_ERROR {
        return Err(PresentationError::contract(
            stage,
            classify_gl_error(preexisting),
            format_args!("preexisting GL error before context-capability query {preexisting:#x}"),
        )
        .with_capabilities(capabilities));
    }
    let mut value = [0_i32; 1];
    // SAFETY: the exact EGL context is current on this owner thread, `gl` was
    // loaded from that display, `name` is one fixed scalar context token, and
    // `value` is initialized writable storage of the required length.
    unsafe { gl.get_integer_v(name, &mut value) };
    let code = catch_native(stage, || gl.get_error())
        .map_err(|error| error.with_capabilities(capabilities))?;
    if code != gl::NO_ERROR {
        return Err(PresentationError::contract(
            stage,
            classify_gl_error(code),
            format_args!("GL context-capability query {name:#x} failed with {code:#x}"),
        )
        .with_capabilities(capabilities));
    }
    Ok(value[0])
}

fn load_desktop_gl(
    display: &Display,
    backend: LinuxPresentationBackend,
) -> Result<Rc<dyn gl::Gl>, PresentationError> {
    let mut invalid_symbol = false;
    // SAFETY: the selected desktop GL context is current on this owner thread,
    // either against the exact EGL window surface or through the bounded
    // suspended-startup surfaceless probe. Glutin's proc lookup belongs to the
    // same retained display. No function table or native handle is exposed
    // outside `LinuxPresentedWindow`.
    let functions = unsafe {
        gl::GlFns::load_with(|symbol| {
            let Ok(symbol) = CString::new(symbol) else {
                invalid_symbol = true;
                return std::ptr::null();
            };
            display.get_proc_address(&symbol).cast()
        })
    };
    if invalid_symbol {
        return Err(PresentationError::contract(
            PresentationFailureStage::LoadFunctions,
            PresentationErrorKind::Driver,
            "GL function name contained an interior NUL",
        ));
    }
    let functions: Rc<dyn gl::Gl> = functions;
    let version = functions.get_string(gl::VERSION);
    if version.is_empty() {
        return Err(PresentationError::contract(
            PresentationFailureStage::LoadFunctions,
            PresentationErrorKind::Driver,
            format!("{backend:?} current context returned an empty GL version"),
        ));
    }
    Ok(functions)
}

fn rgba_within_one(expected: [u8; 4], observed: [u8; 4]) -> bool {
    expected
        .into_iter()
        .zip(observed)
        .all(|(expected, observed)| expected.abs_diff(observed) <= 1)
}

fn retained_egl_objects(
    display: &Display,
    surface: &Surface<WindowSurface>,
) -> Result<EglObjects, EglExtentQueryFailure> {
    #[allow(unreachable_patterns)]
    let display = match display.raw_display() {
        RawDisplay::Egl(display) => {
            NonNull::new(display.cast_mut()).ok_or(EglExtentQueryFailure::NullDisplay)?
        }
        _ => return Err(EglExtentQueryFailure::WrongDisplayBackend),
    };
    #[allow(unreachable_patterns)]
    let surface = match surface.raw_surface() {
        RawSurface::Egl(surface) => {
            NonNull::new(surface.cast_mut()).ok_or(EglExtentQueryFailure::NullSurface)?
        }
        _ => return Err(EglExtentQueryFailure::WrongSurfaceBackend),
    };
    Ok(EglObjects { display, surface })
}

fn query_positive_extent(
    query: &impl EglSurfaceAttributeQuery,
    objects: EglObjects,
    attribute: EglExtentAttribute,
) -> Result<u32, EglExtentQueryFailure> {
    let mut value: EglInt = 0;
    match query.query(objects, attribute, &mut value) {
        EGL_TRUE => {}
        EGL_FALSE => return Err(EglExtentQueryFailure::QueryRejected(attribute)),
        value => {
            return Err(EglExtentQueryFailure::InvalidBoolean { attribute, value });
        }
    }
    if value <= 0 {
        return Err(EglExtentQueryFailure::InvalidValue { attribute, value });
    }
    u32::try_from(value).map_err(|_| EglExtentQueryFailure::InvalidValue { attribute, value })
}

fn checked_egl_surface_extent(
    query: &impl EglSurfaceAttributeQuery,
    objects: EglObjects,
    expected: PhysicalSize,
) -> Result<(), EglExtentQueryFailure> {
    let width = query_positive_extent(query, objects, EglExtentAttribute::Width)?;
    let height = query_positive_extent(query, objects, EglExtentAttribute::Height)?;
    let observed = (width, height);
    let expected = (expected.width, expected.height);
    if observed == expected {
        Ok(())
    } else {
        Err(EglExtentQueryFailure::Mismatch { expected, observed })
    }
}

fn classify_native_extent_confirmation(
    backend: LinuxPresentationBackend,
    result: Result<(), EglExtentQueryFailure>,
) -> Result<NativeExtentConfirmation, EglExtentQueryFailure> {
    match result {
        Ok(()) => Ok(NativeExtentConfirmation::Confirmed),
        Err(EglExtentQueryFailure::Mismatch { .. }) => match backend {
            LinuxPresentationBackend::Wayland => {
                Ok(NativeExtentConfirmation::ReadyForCheckedResize)
            }
            LinuxPresentationBackend::X11 => Ok(NativeExtentConfirmation::Pending),
        },
        Err(failure) => Err(failure),
    }
}

const fn native_extent_confirmation_without_query(
    state: PresentationState,
    expected: PhysicalSize,
) -> Option<NativeExtentConfirmation> {
    if matches!(state, PresentationState::Suspended) || expected.width == 0 || expected.height == 0
    {
        Some(NativeExtentConfirmation::Pending)
    } else {
        None
    }
}

const fn classify_gl_error(code: u32) -> PresentationErrorKind {
    if code == GL_CONTEXT_LOST {
        PresentationErrorKind::ContextLost
    } else if code == gl::OUT_OF_MEMORY {
        PresentationErrorKind::OutOfMemory
    } else {
        PresentationErrorKind::Driver
    }
}

fn catch_native<T>(
    stage: PresentationFailureStage,
    operation: impl FnOnce() -> T,
) -> Result<T, PresentationError> {
    catch_unwind(AssertUnwindSafe(operation)).map_err(|payload| {
        PresentationError::contract(
            stage,
            PresentationErrorKind::Driver,
            bounded_panic_payload(payload.as_ref()),
        )
    })
}

fn catch_glutin<T>(
    stage: PresentationFailureStage,
    operation: impl FnOnce() -> Result<T, glutin::error::Error>,
) -> Result<T, PresentationError> {
    catch_native(stage, operation)?.map_err(|error| PresentationError::driver(stage, &error))
}

fn bounded_panic_payload(payload: &(dyn Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic at native presentation boundary".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;

    use super::{
        DirectFrameTarget, EGL_FALSE, EGL_TRUE, EglExtentAttribute, EglExtentQueryFailure,
        EglObjects, EglSurfaceAttributeQuery, FrameGl, GL_CONTEXT_FLAG_ROBUST_ACCESS_BIT,
        GL_CONTEXT_LOST, GL_LOSE_CONTEXT_ON_RESET, LinuxEglProfile, ProfileAttempt,
        capture_startup_teardown, catch_native, checked_egl_surface_extent,
        checked_query_surface_address, classify_gl_error, classify_native_extent_confirmation,
        drive_profile_ladder, native_extent_confirmation_without_query, profile_ladder,
        rgba_within_one, unsupported_profile_error, validate_context_capability_facts,
    };
    use crate::{
        DirectFrameRequest, DirectRenderError, LinuxAccelerationClass, LinuxPresentationBackend,
        LinuxPresentationCapabilities, LinuxPresentationPolicy, LinuxPresenterCreationError,
        LinuxResetProtection, NativeExtentConfirmation, PresentationError, PresentationErrorKind,
        PresentationFailureStage, PresentationRetentionReport, PresentationShutdownReport,
        PresentationStartupFailure, PresentationState, PresentationTeardownOutcome, SolidColor,
    };
    use wild_buzzard_platform::{PhysicalSize, SurfaceIdAllocator, SurfaceNamespace};

    struct MockFrameGl {
        errors: RefCell<VecDeque<u32>>,
        observed: Cell<[u8; 4]>,
        initial_destination: Cell<Option<[u8; 4]>>,
        write_destination: bool,
    }

    impl MockFrameGl {
        fn new(errors: impl IntoIterator<Item = u32>, observed: [u8; 4]) -> Self {
            Self {
                errors: RefCell::new(errors.into_iter().collect()),
                observed: Cell::new(observed),
                initial_destination: Cell::new(None),
                write_destination: true,
            }
        }

        fn without_write(mut self) -> Self {
            self.write_destination = false;
            self
        }
    }

    impl FrameGl for MockFrameGl {
        fn get_error(&self) -> u32 {
            self.errors
                .borrow_mut()
                .pop_front()
                .unwrap_or(gleam::gl::NO_ERROR)
        }

        fn bind_default_framebuffer(&self) {}
        fn draw_buffer(&self, _buffer: u32) {}
        fn read_buffer(&self, _buffer: u32) {}
        fn disable(&self, _capability: u32) {}
        fn viewport(&self, _width: i32, _height: i32) {}
        fn clear_color(&self, _rgba: [f32; 4]) {}
        fn clear_color_buffer(&self) {}
        fn reset_pack_row_length(&self) {}

        fn read_rgba8_pixel(&self, _x: i32, _y: i32, destination: &mut [u8; 4]) {
            self.initial_destination.set(Some(*destination));
            if self.write_destination {
                *destination = self.observed.get();
            }
        }
    }

    #[derive(Clone, Copy, Debug)]
    struct EglQueryReply {
        attribute: EglExtentAttribute,
        accepted: bool,
        value: i32,
    }

    struct MockEglQuery {
        replies: RefCell<VecDeque<EglQueryReply>>,
        calls: RefCell<Vec<EglExtentAttribute>>,
        panic_on_query: bool,
    }

    impl MockEglQuery {
        fn new(replies: impl IntoIterator<Item = EglQueryReply>) -> Self {
            Self {
                replies: RefCell::new(replies.into_iter().collect()),
                calls: RefCell::new(Vec::new()),
                panic_on_query: false,
            }
        }

        fn panicking() -> Self {
            Self {
                replies: RefCell::new(VecDeque::new()),
                calls: RefCell::new(Vec::new()),
                panic_on_query: true,
            }
        }
    }

    impl EglSurfaceAttributeQuery for MockEglQuery {
        fn query(
            &self,
            _objects: EglObjects,
            attribute: EglExtentAttribute,
            value: &mut i32,
        ) -> u32 {
            assert!(!self.panic_on_query, "injected eglQuerySurface panic");
            self.calls.borrow_mut().push(attribute);
            let reply = self
                .replies
                .borrow_mut()
                .pop_front()
                .expect("one scripted EGL reply per production query");
            assert_eq!(reply.attribute, attribute);
            *value = reply.value;
            if reply.accepted { EGL_TRUE } else { EGL_FALSE }
        }
    }

    fn request() -> DirectFrameRequest {
        let mut allocator = SurfaceIdAllocator::new(SurfaceNamespace::new(8_004).unwrap());
        DirectFrameRequest::new(
            allocator.allocate().unwrap(),
            PhysicalSize::new(800, 600).unwrap(),
            1,
        )
    }

    fn target(operations: &dyn FrameGl) -> DirectFrameTarget<'_> {
        DirectFrameTarget {
            operations,
            request: request(),
            default_buffer: gleam::gl::BACK,
            complete_frames: 0,
            diagnostic_sample: None,
            terminal_fault: None,
        }
    }

    fn fake_egl_objects() -> EglObjects {
        EglObjects {
            display: std::ptr::NonNull::new(std::ptr::dangling_mut()).unwrap(),
            surface: std::ptr::NonNull::new(std::ptr::dangling_mut()).unwrap(),
        }
    }

    fn startup_reports() -> (
        PresentationError,
        PresentationShutdownReport,
        PresentationRetentionReport,
    ) {
        let surface = request().surface();
        (
            PresentationError::contract(
                PresentationFailureStage::CreateSurface,
                PresentationErrorKind::Driver,
                "injected startup activation failure",
            ),
            PresentationShutdownReport::new(surface, 0, None),
            PresentationRetentionReport::new(
                surface,
                0,
                None,
                PresentationFailureStage::ReleaseContext,
                PresentationErrorKind::Driver,
            ),
        )
    }

    fn released_profile(profile: LinuxEglProfile) -> PresentationShutdownReport {
        PresentationShutdownReport::new_with_capabilities(
            request().surface(),
            0,
            None,
            profile.capabilities(),
        )
    }

    #[test]
    fn startup_policy_has_one_fixed_nonrepeating_profile_order() {
        assert_eq!(
            profile_ladder(LinuxPresentationPolicy::StrictHardware),
            [LinuxEglProfile::AcceleratedRobust]
        );
        assert_eq!(
            profile_ladder(LinuxPresentationPolicy::AutomaticCompatible),
            [
                LinuxEglProfile::AcceleratedRobust,
                LinuxEglProfile::AcceleratedCompatible,
                LinuxEglProfile::SoftwareRobust,
                LinuxEglProfile::SoftwareCompatible,
            ]
        );
    }

    #[test]
    fn strict_success_prevents_every_fallback_attempt() {
        let attempted = RefCell::new(Vec::new());
        let selected = drive_profile_ladder::<u8, &'static str>(
            LinuxPresentationPolicy::StrictHardware,
            |profile| {
                attempted.borrow_mut().push(profile);
                ProfileAttempt::Selected(17)
            },
        )
        .expect("strict hardware profile should be selected");
        assert_eq!(selected, 17);
        assert_eq!(*attempted.borrow(), [LinuxEglProfile::AcceleratedRobust]);
    }

    #[test]
    fn exact_typed_unsupported_release_advances_once_in_fixed_order() {
        let attempted = RefCell::new(Vec::new());
        let selected = drive_profile_ladder::<u8, &'static str>(
            LinuxPresentationPolicy::AutomaticCompatible,
            |profile| {
                attempted.borrow_mut().push(profile);
                if profile == LinuxEglProfile::AcceleratedRobust {
                    ProfileAttempt::UnsupportedAfterRelease(
                        unsupported_profile_error(
                            profile,
                            PresentationFailureStage::CreateContext,
                            "diagnostic words are not policy inputs",
                        ),
                        released_profile(profile),
                    )
                } else {
                    ProfileAttempt::Selected(23)
                }
            },
        )
        .expect("cleanly released unsupported robustness should advance");
        assert_eq!(selected, 23);
        assert_eq!(
            *attempted.borrow(),
            [
                LinuxEglProfile::AcceleratedRobust,
                LinuxEglProfile::AcceleratedCompatible,
            ]
        );
    }

    #[test]
    fn automatic_ladder_reaches_software_compatible_only_after_three_exact_rejections() {
        let attempted = RefCell::new(Vec::new());
        let selected = drive_profile_ladder::<LinuxPresentationCapabilities, &'static str>(
            LinuxPresentationPolicy::AutomaticCompatible,
            |profile| {
                attempted.borrow_mut().push(profile);
                match profile {
                    LinuxEglProfile::AcceleratedRobust
                    | LinuxEglProfile::AcceleratedCompatible
                    | LinuxEglProfile::SoftwareRobust => ProfileAttempt::UnsupportedAfterRelease(
                        unsupported_profile_error(
                            profile,
                            PresentationFailureStage::CreateContext,
                            "typed profile rejection",
                        ),
                        released_profile(profile),
                    ),
                    LinuxEglProfile::SoftwareCompatible => {
                        ProfileAttempt::Selected(profile.capabilities())
                    }
                }
            },
        )
        .expect("the fourth exact profile should be selected");
        assert_eq!(selected, LinuxEglProfile::SoftwareCompatible.capabilities());
        assert_eq!(
            *attempted.borrow(),
            [
                LinuxEglProfile::AcceleratedRobust,
                LinuxEglProfile::AcceleratedCompatible,
                LinuxEglProfile::SoftwareRobust,
                LinuxEglProfile::SoftwareCompatible,
            ]
        );
    }

    #[test]
    fn diagnostic_strings_cannot_change_profile_control_flow() {
        for detail in [
            "context robustness is not supported",
            "vendor renderer driver venus passthrough software llvmpipe",
        ] {
            let attempted = RefCell::new(Vec::new());
            let selected = drive_profile_ladder::<u8, &'static str>(
                LinuxPresentationPolicy::AutomaticCompatible,
                |profile| {
                    attempted.borrow_mut().push(profile);
                    if profile == LinuxEglProfile::AcceleratedRobust {
                        ProfileAttempt::UnsupportedBeforeOwnership(unsupported_profile_error(
                            profile,
                            PresentationFailureStage::SelectConfig,
                            detail,
                        ))
                    } else {
                        ProfileAttempt::Selected(29)
                    }
                },
            )
            .expect("only the typed result controls the ladder");
            assert_eq!(selected, 29);
            assert_eq!(attempted.borrow().len(), 2);
        }
    }

    #[test]
    fn nonunsupported_error_cannot_use_the_unsupported_lane() {
        let attempted = RefCell::new(Vec::new());
        let result = drive_profile_ladder::<u8, &'static str>(
            LinuxPresentationPolicy::AutomaticCompatible,
            |profile| {
                attempted.borrow_mut().push(profile);
                ProfileAttempt::UnsupportedBeforeOwnership(
                    PresentationError::contract(
                        PresentationFailureStage::CreateContext,
                        PresentationErrorKind::Driver,
                        "generic context failure",
                    )
                    .with_capabilities(profile.capabilities()),
                )
            },
        );
        let LinuxPresenterCreationError::Presentation(error) = result.unwrap_err() else {
            panic!("malformed unsupported evidence must be a presentation failure");
        };
        assert_eq!(error.kind(), PresentationErrorKind::Driver);
        assert_eq!(attempted.borrow().len(), 1);
    }

    #[test]
    fn mismatched_or_used_release_evidence_cannot_advance() {
        for report in [
            PresentationShutdownReport::new_with_capabilities(
                request().surface(),
                0,
                None,
                LinuxEglProfile::AcceleratedCompatible.capabilities(),
            ),
            PresentationShutdownReport::new_with_capabilities(
                request().surface(),
                1,
                Some(1),
                LinuxEglProfile::AcceleratedRobust.capabilities(),
            ),
        ] {
            let attempted = RefCell::new(Vec::new());
            let result = drive_profile_ladder::<u8, &'static str>(
                LinuxPresentationPolicy::AutomaticCompatible,
                |profile| {
                    attempted.borrow_mut().push(profile);
                    ProfileAttempt::UnsupportedAfterRelease(
                        unsupported_profile_error(
                            profile,
                            PresentationFailureStage::CreateContext,
                            "typed unsupported",
                        ),
                        report,
                    )
                },
            );
            let LinuxPresenterCreationError::Presentation(error) = result.unwrap_err() else {
                panic!("mismatched release evidence must stop startup");
            };
            assert_eq!(error.kind(), PresentationErrorKind::Driver);
            assert_eq!(attempted.borrow().len(), 1);
        }
    }

    #[test]
    fn fatal_error_classes_stop_without_a_second_attempt() {
        for kind in [
            PresentationErrorKind::Driver,
            PresentationErrorKind::OutOfMemory,
            PresentationErrorKind::ContextLost,
        ] {
            let attempted = RefCell::new(Vec::new());
            let result = drive_profile_ladder::<u8, &'static str>(
                LinuxPresentationPolicy::AutomaticCompatible,
                |profile| {
                    attempted.borrow_mut().push(profile);
                    ProfileAttempt::Presentation(
                        PresentationError::contract(
                            PresentationFailureStage::CreateContext,
                            kind,
                            "injected fatal profile failure",
                        )
                        .with_capabilities(profile.capabilities()),
                    )
                },
            );
            let LinuxPresenterCreationError::Presentation(error) = result.unwrap_err() else {
                panic!("fatal profile failure must stop startup");
            };
            assert_eq!(error.kind(), kind);
            assert_eq!(attempted.borrow().len(), 1);
        }
    }

    #[test]
    fn panic_and_uncertain_teardown_stop_without_fallback() {
        let panic_attempts = RefCell::new(Vec::new());
        let panic_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _: Result<u8, LinuxPresenterCreationError<&'static str>> =
                drive_profile_ladder(LinuxPresentationPolicy::AutomaticCompatible, |profile| {
                    panic_attempts.borrow_mut().push(profile);
                    panic!("injected profile-attempt panic")
                });
        }));
        assert!(panic_result.is_err());
        assert_eq!(panic_attempts.borrow().len(), 1);

        let (primary, _shutdown, retention) = startup_reports();
        let attempted = RefCell::new(Vec::new());
        let result = drive_profile_ladder::<u8, &'static str>(
            LinuxPresentationPolicy::AutomaticCompatible,
            |profile| {
                attempted.borrow_mut().push(profile);
                ProfileAttempt::PresentationWithTeardown(PresentationStartupFailure::new(
                    primary.clone(),
                    PresentationTeardownOutcome::RetainedAfterTeardownFailure(retention),
                ))
            },
        );
        assert!(matches!(
            result,
            Err(LinuxPresenterCreationError::PresentationWithTeardown(_))
        ));
        assert_eq!(attempted.borrow().len(), 1);
    }

    #[test]
    fn caller_window_error_stops_without_fallback() {
        let attempted = RefCell::new(Vec::new());
        let result = drive_profile_ladder::<u8, &'static str>(
            LinuxPresentationPolicy::AutomaticCompatible,
            |profile| {
                attempted.borrow_mut().push(profile);
                ProfileAttempt::Window("injected window creation failure")
            },
        );
        assert!(matches!(
            result,
            Err(LinuxPresenterCreationError::Window(
                "injected window creation failure"
            ))
        ));
        assert_eq!(attempted.borrow().len(), 1);
    }

    #[test]
    fn context_reset_facts_must_exactly_match_the_selected_profile() {
        let robust = LinuxPresentationCapabilities::new(
            LinuxAccelerationClass::Accelerated,
            LinuxResetProtection::LoseContextOnReset,
        );
        assert_eq!(
            validate_context_capability_facts(
                robust,
                GL_CONTEXT_FLAG_ROBUST_ACCESS_BIT,
                Some(GL_LOSE_CONTEXT_ON_RESET),
            ),
            Ok(())
        );
        assert!(
            validate_context_capability_facts(robust, 0, Some(GL_LOSE_CONTEXT_ON_RESET)).is_err()
        );
        assert!(
            validate_context_capability_facts(robust, GL_CONTEXT_FLAG_ROBUST_ACCESS_BIT, Some(0),)
                .is_err()
        );

        let compatible = LinuxPresentationCapabilities::new(
            LinuxAccelerationClass::Software,
            LinuxResetProtection::Unavailable,
        );
        assert_eq!(
            validate_context_capability_facts(compatible, 0, None),
            Ok(())
        );
        assert!(
            validate_context_capability_facts(compatible, GL_CONTEXT_FLAG_ROBUST_ACCESS_BIT, None,)
                .is_err()
        );
    }

    #[test]
    fn diagnostic_rgba_accepts_only_rounding_noise() {
        assert!(rgba_within_one([10, 20, 30, 255], [11, 19, 30, 255]));
        assert!(!rgba_within_one([10, 20, 30, 255], [12, 20, 30, 255]));
    }

    #[test]
    fn robust_context_loss_has_a_distinct_terminal_class() {
        assert_eq!(
            classify_gl_error(GL_CONTEXT_LOST),
            PresentationErrorKind::ContextLost
        );
        assert_eq!(
            classify_gl_error(gleam::gl::INVALID_OPERATION),
            PresentationErrorKind::Driver
        );
        assert_eq!(
            classify_gl_error(gleam::gl::OUT_OF_MEMORY),
            PresentationErrorKind::OutOfMemory
        );
    }

    #[test]
    fn post_read_gl_error_precedes_untrusted_diagnostic_bytes() {
        let operations = MockFrameGl::new([gleam::gl::NO_ERROR, GL_CONTEXT_LOST], [0, 0, 0, 0]);
        let mut target = target(&operations);
        let result = target.clear_solid(SolidColor::new(24, 92, 220, 255));
        assert_eq!(result, Err(DirectRenderError::GlError(GL_CONTEXT_LOST)));
        assert_eq!(
            target.finish(Ok(())),
            Err(DirectRenderError::GlError(GL_CONTEXT_LOST))
        );
    }

    #[test]
    fn initialized_readback_remains_safe_when_driver_writes_nothing() {
        let operations = MockFrameGl::new(
            [gleam::gl::NO_ERROR, gleam::gl::INVALID_OPERATION],
            [255, 255, 255, 255],
        )
        .without_write();
        let mut target = target(&operations);
        assert_eq!(
            target.clear_solid(SolidColor::new(24, 92, 220, 255)),
            Err(DirectRenderError::GlError(gleam::gl::INVALID_OPERATION))
        );
        assert_eq!(operations.initial_destination.get(), Some([0_u8; 4]));
    }

    #[test]
    fn terminal_diagnostic_fault_overrides_renderer_remapping() {
        let expected = [24, 92, 220, 255];
        let observed = [0, 0, 0, 0];
        let operations = MockFrameGl::new([gleam::gl::NO_ERROR, gleam::gl::NO_ERROR], observed);
        let mut target = target(&operations);
        let error = DirectRenderError::DiagnosticMismatch { expected, observed };
        assert_eq!(
            target.clear_solid(SolidColor::new(24, 92, 220, 255)),
            Err(error)
        );
        assert_eq!(target.finish(Err(DirectRenderError::Rejected)), Err(error));
    }

    #[test]
    fn first_terminal_target_fault_is_authoritative() {
        let expected = [24, 92, 220, 255];
        let observed = [0, 0, 0, 0];
        let operations = MockFrameGl::new(
            [gleam::gl::NO_ERROR, gleam::gl::NO_ERROR, GL_CONTEXT_LOST],
            observed,
        );
        let mut target = target(&operations);
        let first = DirectRenderError::DiagnosticMismatch { expected, observed };
        assert_eq!(
            target.clear_solid(SolidColor::new(24, 92, 220, 255)),
            Err(first)
        );
        assert_eq!(
            target.clear_solid(SolidColor::new(24, 92, 220, 255)),
            Err(first)
        );
        assert_eq!(target.finish(Ok(())), Err(first));
    }

    #[test]
    fn native_panics_are_staged_and_classified() {
        let error = catch_native(PresentationFailureStage::ConfigureSwap, || {
            panic!("injected swap-configuration panic")
        })
        .unwrap_err();
        assert_eq!(error.stage(), PresentationFailureStage::ConfigureSwap);
        assert_eq!(error.kind(), PresentationErrorKind::Driver);
        assert!(error.detail().contains("injected swap-configuration panic"));
    }

    #[test]
    fn missing_egl_query_surface_symbol_is_rejected() {
        assert_eq!(
            checked_query_surface_address(std::ptr::null()).unwrap_err(),
            EglExtentQueryFailure::MissingQuerySurface
        );
    }

    #[test]
    fn false_egl_query_surface_result_is_rejected_before_using_value() {
        let query = MockEglQuery::new([EglQueryReply {
            attribute: EglExtentAttribute::Width,
            accepted: false,
            value: 800,
        }]);
        let expected = PhysicalSize::new(800, 600).unwrap();
        assert_eq!(
            checked_egl_surface_extent(&query, fake_egl_objects(), expected),
            Err(EglExtentQueryFailure::QueryRejected(
                EglExtentAttribute::Width
            ))
        );
        assert_eq!(*query.calls.borrow(), [EglExtentAttribute::Width]);
    }

    #[test]
    fn nonpositive_egl_extent_is_rejected_before_conversion() {
        let query = MockEglQuery::new([EglQueryReply {
            attribute: EglExtentAttribute::Width,
            accepted: true,
            value: -1,
        }]);
        assert_eq!(
            checked_egl_surface_extent(
                &query,
                fake_egl_objects(),
                PhysicalSize::new(800, 600).unwrap(),
            ),
            Err(EglExtentQueryFailure::InvalidValue {
                attribute: EglExtentAttribute::Width,
                value: -1,
            })
        );
    }

    #[test]
    fn noncanonical_egl_boolean_is_rejected() {
        struct InvalidBooleanQuery;

        impl EglSurfaceAttributeQuery for InvalidBooleanQuery {
            fn query(
                &self,
                _objects: EglObjects,
                _attribute: EglExtentAttribute,
                value: &mut i32,
            ) -> u32 {
                *value = 800;
                2
            }
        }

        assert_eq!(
            checked_egl_surface_extent(
                &InvalidBooleanQuery,
                fake_egl_objects(),
                PhysicalSize::new(800, 600).unwrap(),
            ),
            Err(EglExtentQueryFailure::InvalidBoolean {
                attribute: EglExtentAttribute::Width,
                value: 2,
            })
        );
    }

    #[test]
    fn partial_native_extent_mismatch_uses_both_checked_queries() {
        let query = MockEglQuery::new([
            EglQueryReply {
                attribute: EglExtentAttribute::Width,
                accepted: true,
                value: 800,
            },
            EglQueryReply {
                attribute: EglExtentAttribute::Height,
                accepted: true,
                value: 599,
            },
        ]);
        assert_eq!(
            checked_egl_surface_extent(
                &query,
                fake_egl_objects(),
                PhysicalSize::new(800, 600).unwrap(),
            ),
            Err(EglExtentQueryFailure::Mismatch {
                expected: (800, 600),
                observed: (800, 599),
            })
        );
        assert_eq!(
            *query.calls.borrow(),
            [EglExtentAttribute::Width, EglExtentAttribute::Height]
        );
    }

    #[test]
    fn exact_checked_egl_extent_is_admitted() {
        let query = MockEglQuery::new([
            EglQueryReply {
                attribute: EglExtentAttribute::Width,
                accepted: true,
                value: 800,
            },
            EglQueryReply {
                attribute: EglExtentAttribute::Height,
                accepted: true,
                value: 600,
            },
        ]);
        assert_eq!(
            checked_egl_surface_extent(
                &query,
                fake_egl_objects(),
                PhysicalSize::new(800, 600).unwrap(),
            ),
            Ok(())
        );
    }

    #[test]
    fn exact_native_extent_is_confirmed_without_exporting_authority() {
        assert_eq!(
            classify_native_extent_confirmation(LinuxPresentationBackend::Wayland, Ok(())),
            Ok(NativeExtentConfirmation::Confirmed)
        );
        assert_eq!(
            classify_native_extent_confirmation(LinuxPresentationBackend::X11, Ok(())),
            Ok(NativeExtentConfirmation::Confirmed)
        );
    }

    #[test]
    fn wayland_extent_mismatch_requires_the_checked_resize_transaction() {
        assert_eq!(
            classify_native_extent_confirmation(
                LinuxPresentationBackend::Wayland,
                Err(EglExtentQueryFailure::Mismatch {
                    expected: (800, 600),
                    observed: (799, 600),
                }),
            ),
            Ok(NativeExtentConfirmation::ReadyForCheckedResize)
        );
    }

    #[test]
    fn x11_extent_mismatch_is_pending_not_a_query_fault() {
        assert_eq!(
            classify_native_extent_confirmation(
                LinuxPresentationBackend::X11,
                Err(EglExtentQueryFailure::Mismatch {
                    expected: (800, 600),
                    observed: (799, 600),
                }),
            ),
            Ok(NativeExtentConfirmation::Pending)
        );
    }

    #[test]
    fn native_extent_query_fault_is_not_downgraded_to_pending() {
        assert_eq!(
            classify_native_extent_confirmation(
                LinuxPresentationBackend::Wayland,
                Err(EglExtentQueryFailure::QueryRejected(
                    EglExtentAttribute::Height,
                )),
            ),
            Err(EglExtentQueryFailure::QueryRejected(
                EglExtentAttribute::Height,
            ))
        );
    }

    #[test]
    fn zero_axis_extent_is_pending_without_querying_egl() {
        for expected in [
            PhysicalSize::new(0, 600).unwrap(),
            PhysicalSize::new(800, 0).unwrap(),
            PhysicalSize::new(0, 0).unwrap(),
        ] {
            assert_eq!(
                native_extent_confirmation_without_query(PresentationState::Active, expected),
                Some(NativeExtentConfirmation::Pending)
            );
        }
        assert_eq!(
            native_extent_confirmation_without_query(
                PresentationState::Active,
                PhysicalSize::new(800, 600).unwrap(),
            ),
            None
        );
        assert_eq!(
            native_extent_confirmation_without_query(
                PresentationState::Suspended,
                PhysicalSize::new(800, 600).unwrap(),
            ),
            Some(NativeExtentConfirmation::Pending)
        );
    }

    #[test]
    fn panic_in_production_shaped_extent_query_is_staged() {
        let query = MockEglQuery::panicking();
        let error = catch_native(PresentationFailureStage::DrawFrame, || {
            checked_egl_surface_extent(
                &query,
                fake_egl_objects(),
                PhysicalSize::new(800, 600).unwrap(),
            )
        })
        .unwrap_err();
        assert_eq!(error.stage(), PresentationFailureStage::DrawFrame);
        assert_eq!(error.kind(), PresentationErrorKind::Driver);
        assert!(error.detail().contains("injected eglQuerySurface panic"));
    }

    #[test]
    fn startup_failure_preserves_primary_error_and_clean_release() {
        let (primary, shutdown, fallback) = startup_reports();
        let teardown = capture_startup_teardown(fallback, || Ok(shutdown));
        let failure = PresentationStartupFailure::new(primary.clone(), teardown);
        assert_eq!(failure.primary(), &primary);
        assert_eq!(
            failure.teardown(),
            PresentationTeardownOutcome::WrappersReleased(shutdown)
        );
    }

    #[test]
    fn startup_failure_preserves_explicit_retention_outcome() {
        let (primary, _shutdown, retention) = startup_reports();
        let teardown = capture_startup_teardown(retention, || Err(retention));
        let failure = PresentationStartupFailure::new(primary.clone(), teardown);
        assert_eq!(failure.primary(), &primary);
        assert_eq!(
            failure.teardown(),
            PresentationTeardownOutcome::RetainedAfterTeardownFailure(retention)
        );
    }

    #[test]
    fn startup_teardown_panic_falls_back_to_retention_without_replacing_primary() {
        let (primary, _shutdown, fallback) = startup_reports();
        let teardown = capture_startup_teardown(
            fallback,
            || -> Result<PresentationShutdownReport, PresentationRetentionReport> {
                panic!("injected partial-owner teardown panic")
            },
        );
        let failure = PresentationStartupFailure::new(primary.clone(), teardown);
        assert_eq!(failure.primary(), &primary);
        assert_eq!(
            failure.teardown(),
            PresentationTeardownOutcome::RetainedAfterTeardownFailure(fallback)
        );
    }
}
