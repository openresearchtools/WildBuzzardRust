use std::any::Any;
use std::cmp::Reverse;
use std::ffi::CString;
use std::mem;
use std::num::NonZeroU32;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;

use gleam::gl;
use glutin::api::egl::config::Config;
use glutin::api::egl::context::PossiblyCurrentContext;
use glutin::api::egl::device::Device;
use glutin::api::egl::display::Display;
use glutin::api::egl::surface::Surface;
use glutin::config::{Api, ColorBufferType, ConfigSurfaceTypes, ConfigTemplateBuilder, GlConfig};
use glutin::context::{
    ContextApi, ContextAttributesBuilder, GlProfile, NotCurrentGlContext, PossiblyCurrentGlContext,
    Robustness, Version,
};
use glutin::display::GlDisplay;
use glutin::surface::{GlSurface, PbufferSurface, SurfaceAttributesBuilder};
use raw_window_handle::{RawDisplayHandle, XlibDisplayHandle};

use crate::error::{ContextAttempt, ContextBackend, ContextStep, HeadlessError};
use crate::frame::FrameSize;

const MAX_EGL_DEVICE_ATTEMPTS: usize = 16;

/// Driver information captured from the exact current Linux EGL/GL context.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LinuxGlInfo {
    backend: ContextBackend,
    egl_version: String,
    vendor: String,
    renderer: String,
    version: String,
    shading_language_version: String,
}

impl LinuxGlInfo {
    /// Returns whether this context came from an EGL device or X11 fallback.
    #[must_use]
    pub const fn backend(&self) -> &ContextBackend {
        &self.backend
    }

    /// Returns the EGL version string reported by glutin.
    #[must_use]
    pub fn egl_version(&self) -> &str {
        &self.egl_version
    }

    /// Returns the GL vendor string.
    #[must_use]
    pub fn vendor(&self) -> &str {
        &self.vendor
    }

    /// Returns the GL renderer string.
    #[must_use]
    pub fn renderer(&self) -> &str {
        &self.renderer
    }

    /// Returns the desktop GL version string.
    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }

    /// Returns the GLSL version string.
    #[must_use]
    pub fn shading_language_version(&self) -> &str {
        &self.shading_language_version
    }
}

/// A same-thread, fixed-size EGL pbuffer context.
pub(crate) struct LinuxEglContext {
    context: PossiblyCurrentContext,
    surface: Surface<PbufferSurface>,
    _display: Display,
    gl: Rc<dyn gl::Gl>,
    info: LinuxGlInfo,
}

impl LinuxEglContext {
    pub(crate) fn create(
        size: FrameSize,
        allow_device_contexts: bool,
        allow_x11_fallback: bool,
    ) -> Result<Self, HeadlessError> {
        let mut attempts = Vec::with_capacity(MAX_EGL_DEVICE_ATTEMPTS + 2);
        if allow_device_contexts {
            match Device::query_devices() {
                Ok(devices) => {
                    for (index, device) in devices.take(MAX_EGL_DEVICE_ATTEMPTS).enumerate() {
                        let backend = ContextBackend::EglDevice {
                            index,
                            name: device.name().map(bounded_driver_string),
                        };
                        // SAFETY: `device` was produced by glutin from the same process-global
                        // EGL loader. Passing no DRM display is explicitly supported, and the
                        // resulting `Display` owns the EGL display reference for all descendants.
                        let display = match unsafe { Display::with_device(&device, None) } {
                            Ok(display) => display,
                            Err(error) => {
                                attempts.push(ContextAttempt::new(
                                    Some(backend),
                                    ContextStep::CreateDisplay,
                                    error,
                                ));
                                continue;
                            }
                        };
                        match Self::from_display(size, display, backend.clone()) {
                            Ok(context) => return Ok(context),
                            Err(attempt) => attempts.push(attempt),
                        }
                    }
                }
                Err(error) => attempts.push(ContextAttempt::new(
                    None,
                    ContextStep::EnumerateDevices,
                    error,
                )),
            }
        } else if !allow_x11_fallback {
            attempts.push(ContextAttempt::new(
                None,
                ContextStep::EnumerateDevices,
                "all Linux EGL context sources disabled by policy",
            ));
        }

        if allow_x11_fallback {
            let backend = ContextBackend::X11Default;
            let raw_display = RawDisplayHandle::Xlib(XlibDisplayHandle::new(None, 0));
            // SAFETY: raw-window-handle and glutin explicitly define a null Xlib display
            // as a request for EGL's default X11 display. No borrowed native pointer is
            // supplied, and the created `Display` owns all EGL descendants.
            match unsafe { Display::new(raw_display) } {
                Ok(display) => match Self::from_display(size, display, backend.clone()) {
                    Ok(context) => return Ok(context),
                    Err(attempt) => attempts.push(attempt),
                },
                Err(error) => attempts.push(ContextAttempt::new(
                    Some(backend),
                    ContextStep::CreateDisplay,
                    error,
                )),
            }
        }

        Err(HeadlessError::ContextUnavailable { attempts })
    }

    fn from_display(
        size: FrameSize,
        display: Display,
        backend: ContextBackend,
    ) -> Result<Self, ContextAttempt> {
        let width = NonZeroU32::new(size.width()).expect("validated frame width is non-zero");
        let height = NonZeroU32::new(size.height()).expect("validated frame height is non-zero");
        let template = ConfigTemplateBuilder::new()
            .with_alpha_size(8)
            .with_depth_size(0)
            .with_stencil_size(0)
            .with_surface_type(ConfigSurfaceTypes::PBUFFER)
            .with_pbuffer_sizes(width, height)
            .with_api(Api::OPENGL)
            .build();
        // SAFETY: `display` is initialized and remains owned by the returned context.
        // The template contains no raw native handles and requests only an EGL pbuffer.
        let configs = unsafe { display.find_configs(template) }.map_err(|error| {
            ContextAttempt::new(Some(backend.clone()), ContextStep::SelectConfig, error)
        })?;
        let config = select_config(configs).ok_or_else(|| {
            ContextAttempt::new(
                Some(backend.clone()),
                ContextStep::SelectConfig,
                "no RGBA8 desktop-GL pbuffer configuration",
            )
        })?;

        let context_attributes = ContextAttributesBuilder::new()
            .with_context_api(ContextApi::OpenGl(Some(Version::new(3, 2))))
            .with_profile(GlProfile::Core)
            .with_robustness(Robustness::RobustLoseContextOnReset)
            .build(None);
        // SAFETY: `config` was enumerated from this exact display. The attributes
        // contain no raw window and request a context compatible with its OPENGL bit.
        let not_current =
            unsafe { display.create_context(&config, &context_attributes) }.map_err(|error| {
                ContextAttempt::new(Some(backend.clone()), ContextStep::CreateContext, error)
            })?;

        let surface_attributes = SurfaceAttributesBuilder::<PbufferSurface>::new()
            .with_single_buffer(true)
            .build(width, height);
        // SAFETY: `config` belongs to `display`; non-zero dimensions were validated
        // against the selected config and are retained for the pbuffer's lifetime.
        let surface = unsafe { display.create_pbuffer_surface(&config, &surface_attributes) }
            .map_err(|error| {
                ContextAttempt::new(Some(backend.clone()), ContextStep::CreateSurface, error)
            })?;
        let context = not_current.make_current(&surface).map_err(|error| {
            ContextAttempt::new(Some(backend.clone()), ContextStep::MakeCurrent, error)
        })?;

        let post_current_initialization = catch_unwind(AssertUnwindSafe(|| {
            let gl = load_desktop_gl(&display, &backend)?;
            let info = LinuxGlInfo {
                backend: backend.clone(),
                egl_version: bounded_driver_string(&display.version_string()),
                vendor: bounded_driver_string(&gl.get_string(gl::VENDOR)),
                renderer: bounded_driver_string(&gl.get_string(gl::RENDERER)),
                version: bounded_driver_string(&gl.get_string(gl::VERSION)),
                shading_language_version: bounded_driver_string(
                    &gl.get_string(gl::SHADING_LANGUAGE_VERSION),
                ),
            };
            if info.version.is_empty() || info.renderer.is_empty() {
                return Err(ContextAttempt::new(
                    Some(backend.clone()),
                    ContextStep::LoadFunctions,
                    "current context returned an empty GL version or renderer string",
                ));
            }
            Ok((gl, info))
        }));
        let initialized = match post_current_initialization {
            Ok(result) => result,
            Err(payload) => Err(ContextAttempt::new(
                Some(backend.clone()),
                ContextStep::LoadFunctions,
                bounded_panic_payload(payload.as_ref()),
            )),
        };
        let (gl, info) = match initialized {
            Ok(initialized) => initialized,
            Err(attempt) => {
                if let Err(detail) = release_after_failed_initialization(&context) {
                    let cleanup_attempt = ContextAttempt::new(
                        Some(backend),
                        ContextStep::ReleaseAfterFailure,
                        detail,
                    );
                    // EGL may defer destruction of a current context or surface.
                    // Retaining all native owners is the only fail-closed option
                    // once the driver refuses to unbind them.
                    mem::forget(context);
                    mem::forget(surface);
                    mem::forget(display);
                    return Err(cleanup_attempt);
                }
                return Err(attempt);
            }
        };

        Ok(Self {
            context,
            surface,
            _display: display,
            gl,
            info,
        })
    }

    pub(crate) fn gl(&self) -> Rc<dyn gl::Gl> {
        Rc::clone(&self.gl)
    }

    pub(crate) const fn info(&self) -> &LinuxGlInfo {
        &self.info
    }

    pub(crate) fn ensure_current(&self) -> Result<(), String> {
        if self.context.is_current() && self.surface.is_current(&self.context) {
            return Ok(());
        }
        self.context
            .make_current(&self.surface)
            .map_err(|error| bounded_driver_string(&error.to_string()))?;
        if self.context.is_current() && self.surface.is_current(&self.context) {
            Ok(())
        } else {
            Err(
                "EGL reported success without making the expected context and pbuffer current"
                    .to_owned(),
            )
        }
    }

    pub(crate) fn release_current(&self) -> Result<(), String> {
        if !self.context.is_current() {
            return Ok(());
        }
        self.context
            .make_not_current_in_place()
            .map_err(|error| bounded_driver_string(&error.to_string()))?;
        if self.context.is_current() {
            Err("EGL reported success but the context remains current".to_owned())
        } else {
            Ok(())
        }
    }

    pub(crate) fn release_or_leak(self) -> Result<(), String> {
        let release = catch_unwind(AssertUnwindSafe(|| self.release_current()));
        let error = match release {
            Ok(Ok(())) => {
                drop(self);
                return Ok(());
            }
            Ok(Err(detail)) => detail,
            Err(payload) => bounded_panic_payload(payload.as_ref()),
        };
        let detail = bounded_driver_string(&format!(
            "{error}; retained the EGL context, pbuffer, and display because destroying current native objects is unsafe"
        ));
        mem::forget(self);
        Err(detail)
    }
}

fn select_config(configs: impl Iterator<Item = Config>) -> Option<Config> {
    configs
        .filter(|config| {
            config.color_buffer_type()
                == Some(ColorBufferType::Rgb {
                    r_size: 8,
                    g_size: 8,
                    b_size: 8,
                })
                && config.alpha_size() == 8
                && !config.float_pixels()
                && config.num_samples() == 0
        })
        .max_by_key(|config| {
            (
                config.hardware_accelerated(),
                Reverse(config.depth_size()),
                Reverse(config.stencil_size()),
            )
        })
}

fn load_desktop_gl(
    display: &Display,
    backend: &ContextBackend,
) -> Result<Rc<dyn gl::Gl>, ContextAttempt> {
    let mut invalid_symbol = false;
    // SAFETY: a desktop GL 3.2 context from `display` is current on this thread and
    // remains current for the lifetime of the returned function table. glutin's
    // `get_proc_address` is the loader for that exact EGL display. Interior-NUL names
    // are rejected and represented by null rather than passed to EGL.
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
        return Err(ContextAttempt::new(
            Some(backend.clone()),
            ContextStep::LoadFunctions,
            "GL function name contained an interior NUL",
        ));
    }
    Ok(functions)
}

fn bounded_driver_string(value: &str) -> String {
    const MAX_DRIVER_STRING_BYTES: usize = 1_024;
    if value.len() <= MAX_DRIVER_STRING_BYTES {
        return value.to_owned();
    }
    let mut boundary = MAX_DRIVER_STRING_BYTES;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    let mut result = value[..boundary].to_owned();
    result.push_str("...");
    result
}

fn release_after_failed_initialization(context: &PossiblyCurrentContext) -> Result<(), String> {
    match catch_unwind(AssertUnwindSafe(|| {
        context
            .make_not_current_in_place()
            .map_err(|error| bounded_driver_string(&error.to_string()))?;
        if context.is_current() {
            Err("EGL reported success but the failed context remains current".to_owned())
        } else {
            Ok(())
        }
    })) {
        Ok(result) => result,
        Err(payload) => Err(bounded_panic_payload(payload.as_ref())),
    }
}

fn bounded_panic_payload(payload: &(dyn Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        bounded_driver_string(message)
    } else if let Some(message) = payload.downcast_ref::<String>() {
        bounded_driver_string(message)
    } else {
        "non-string panic payload while managing EGL".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::bounded_driver_string;

    #[test]
    fn driver_diagnostics_are_bounded_on_utf8_boundaries() {
        let source = "🦅".repeat(300);
        let bounded = bounded_driver_string(&source);
        assert!(bounded.len() <= 1_027);
        assert!(bounded.ends_with("..."));
        assert!(bounded.is_char_boundary(bounded.len()));
    }
}
