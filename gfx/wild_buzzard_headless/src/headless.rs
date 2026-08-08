use std::any::Any;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::Instant;

use webrender::{RenderApi, Renderer, Transaction, WebRenderOptions, create_webrender_instance};
use webrender_api::units::{DeviceIntSize, FramebufferIntSize};
use webrender_api::{Checkpoint, ColorF, DocumentId, Epoch, ImageFormat, RenderReasons};
use wild_buzzard_renderer::CompiledScene;

use crate::error::{FrameStage, HeadlessError, ResourceKind};
use crate::frame::{FrameRequest, FrameSize, HeadlessLimits, RgbaFrame, enforce};
use crate::linux_egl::LinuxEglContext;
pub use crate::linux_egl::LinuxGlInfo;
use crate::notifier::{HeadlessNotifier, StageWaiter};

const APP_UNITS_PER_CSS_PIXEL: i32 = 60;

/// Evidence that explicit shutdown reached the backend and released EGL.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ShutdownReport {
    backend_acknowledged: bool,
    context_released: bool,
    wake_notifications: u64,
    frame_ready_notifications: u64,
}

impl ShutdownReport {
    /// Returns whether `WebRender`'s backend thread acknowledged shutdown.
    #[must_use]
    pub const fn backend_acknowledged(self) -> bool {
        self.backend_acknowledged
    }

    /// Returns whether EGL accepted making the context non-current.
    #[must_use]
    pub const fn context_released(self) -> bool {
        self.context_released
    }

    /// Returns renderer wake notifications observed during this lifetime.
    #[must_use]
    pub const fn wake_notifications(self) -> u64 {
        self.wake_notifications
    }

    /// Returns completed-frame notifications observed during this lifetime.
    #[must_use]
    pub const fn frame_ready_notifications(self) -> u64 {
        self.frame_ready_notifications
    }
}

/// One same-thread `WebRender` instance bound to a fixed Linux EGL pbuffer.
pub struct HeadlessRenderer {
    size: FrameSize,
    limits: HeadlessLimits,
    pixel_bytes: usize,
    context: Option<LinuxEglContext>,
    renderer: Option<Renderer>,
    api: Option<RenderApi>,
    document_id: DocumentId,
    notifier: HeadlessNotifier,
    last_revision: Option<u64>,
    last_epoch: Option<u32>,
    unusable: bool,
    shutdown: bool,
}

impl HeadlessRenderer {
    /// Creates an actual imported `WebRender` renderer and document on a Linux EGL
    /// pbuffer of the requested fixed size.
    ///
    /// # Errors
    ///
    /// Returns structured validation, EGL, GL, or `WebRender` initialization
    /// diagnostics. No software display-list replacement is attempted.
    pub fn new(size: FrameSize, limits: HeadlessLimits) -> Result<Self, HeadlessError> {
        let pixel_bytes = limits.validate(size)?;
        let context = LinuxEglContext::create(
            size,
            limits.allow_device_contexts(),
            limits.allow_x11_fallback(),
        )?;
        let notifier = HeadlessNotifier::default();
        let options = WebRenderOptions {
            clear_color: ColorF::new(1.0, 1.0, 1.0, 1.0),
            enable_aa: false,
            enable_dithering: false,
            enable_subpixel_aa: false,
            testing: true,
            enable_gpu_markers: false,
            enable_debugger: false,
            panic_on_gl_error: false,
            reject_software_rasterizer: false,
            ..WebRenderOptions::default()
        };
        let initialization = catch_unwind(AssertUnwindSafe(|| {
            let (renderer, sender) =
                create_webrender_instance(context.gl(), Box::new(notifier.clone()), options, None)
                    .map_err(HeadlessError::renderer_initialization)?;
            let api = sender.create_api();
            let document_id = api.add_document(device_size(size));
            Ok((renderer, api, document_id))
        }));
        let initialized = match initialization {
            Ok(result) => result,
            Err(payload) => Err(HeadlessError::RendererInitialization {
                detail: bounded_panic_payload(payload.as_ref()),
            }),
        };
        let (renderer, api, document_id) = match initialized {
            Ok(initialized) => initialized,
            Err(initialization_error) => {
                return match context.release_or_leak() {
                    Ok(()) => Err(initialization_error),
                    Err(detail) => Err(HeadlessError::ContextRelease { detail }),
                };
            }
        };
        Ok(Self {
            size,
            limits,
            pixel_bytes,
            context: Some(context),
            renderer: Some(renderer),
            api: Some(api),
            document_id,
            notifier,
            last_revision: None,
            last_epoch: None,
            unusable: false,
            shutdown: false,
        })
    }

    /// Returns fixed pbuffer dimensions.
    #[must_use]
    pub const fn size(&self) -> FrameSize {
        self.size
    }

    /// Returns cached information from this renderer's owned EGL/GL context.
    ///
    /// # Errors
    ///
    /// Returns `AlreadyShutdown` after explicit shutdown.
    pub fn gl_info(&self) -> Result<&LinuxGlInfo, HeadlessError> {
        self.context
            .as_ref()
            .map(LinuxEglContext::info)
            .ok_or(HeadlessError::AlreadyShutdown)
    }

    /// Consumes and submits one validated scene, renders it with `WebRender`, and
    /// returns an owned bounded RGBA8 screenshot in top-left row order.
    ///
    /// Text records still awaiting shaping remain explicit in frame metadata and
    /// are not replaced with fake glyphs.
    ///
    /// # Errors
    ///
    /// Rejects stale revisions/epochs, mismatched dimensions, excess resources,
    /// deadline failures, backend failures, GL failures, and allocation failure.
    pub fn render(
        &mut self,
        scene: CompiledScene,
        request: FrameRequest,
    ) -> Result<RgbaFrame, HeadlessError> {
        if self.shutdown {
            return Err(HeadlessError::AlreadyShutdown);
        }
        if self.unusable {
            return Err(HeadlessError::RendererUnusable);
        }
        let revision = scene.scene().document_revision();
        let pending_text_runs = scene.scene().pending_text().len();
        self.validate_submission(&scene, request)?;
        self.activate_for_render()?;

        let mut pixels = self.allocate_pixels()?;
        let deadline = self.frame_deadline()?;
        let frame_ready_before = self.notifier.frame_ready_count();
        let (built_request, built_waiter) = StageWaiter::new(Checkpoint::FrameBuilt);
        let (rendered_request, rendered_waiter) = StageWaiter::new(Checkpoint::FrameRendered);
        let (pipeline_id, display_list) = scene.into_webrender();
        let mut transaction = Transaction::new();
        transaction.set_display_list(Epoch(request.epoch()), (pipeline_id, display_list));
        transaction.set_root_pipeline(pipeline_id);
        transaction.notify(built_request);
        transaction.notify(rendered_request);
        transaction.generate_frame(
            u64::from(request.epoch()),
            true,
            false,
            RenderReasons::empty(),
        );

        let api = self.api.as_mut().ok_or(HeadlessError::AlreadyShutdown)?;
        let send_result = catch_unwind(AssertUnwindSafe(|| {
            api.send_transaction(self.document_id, transaction);
        }));
        self.last_revision = Some(revision);
        self.last_epoch = Some(request.epoch());
        if send_result.is_err() {
            self.unusable = true;
            return Err(HeadlessError::BackendDisconnected);
        }

        if let Err(error) = built_waiter.wait_until(
            FrameStage::FrameBuilt,
            deadline,
            self.limits.frame_timeout(),
        ) {
            self.unusable = true;
            return Err(error);
        }
        // The backend invokes `FrameBuilt` immediately before enqueueing the
        // renderer-side notification requests. Waiting for `new_frame_ready`
        // closes that queue-ordering race: the callback happens only after the
        // requests have been sent, so the following nonblocking `update()` must
        // be able to ingest the matching `FrameRendered` request.
        if let Err(error) = self.notifier.wait_for_frame_ready_after(
            frame_ready_before,
            deadline,
            self.limits.frame_timeout(),
        ) {
            self.unusable = true;
            return Err(error);
        }
        let renderer = self
            .renderer
            .as_mut()
            .ok_or(HeadlessError::AlreadyShutdown)?;
        renderer.update();
        let actual_epoch = renderer
            .current_epoch(self.document_id, pipeline_id)
            .map(|epoch| epoch.0);
        if actual_epoch != Some(request.epoch()) {
            self.unusable = true;
            return Err(HeadlessError::EpochNotPublished {
                expected: request.epoch(),
                actual: actual_epoch,
            });
        }
        if let Err(errors) = renderer.render(device_size(self.size), 0) {
            self.unusable = true;
            return Err(HeadlessError::render_failed(errors));
        }
        if let Err(error) = rendered_waiter.wait_until(
            FrameStage::FrameRendered,
            deadline,
            self.limits.frame_timeout(),
        ) {
            self.unusable = true;
            return Err(error);
        }
        if self.notifier.saw_unexpected_external_event() {
            self.unusable = true;
            return Err(HeadlessError::UnexpectedExternalEvent);
        }

        let rect = FramebufferIntSize::new(
            self.size.width().cast_signed(),
            self.size.height().cast_signed(),
        )
        .into();
        renderer.read_pixels_into(rect, ImageFormat::RGBA8, &mut pixels);
        flip_vertical(&mut pixels, self.size);
        Ok(RgbaFrame::new(
            self.size,
            revision,
            request.epoch(),
            pending_text_runs,
            pixels,
        ))
    }

    /// Explicitly shuts down `WebRender` with a bounded acknowledgement wait,
    /// deletes GL resources while the context is current, and releases EGL.
    ///
    /// # Errors
    ///
    /// Returns a shutdown timeout or EGL release diagnostic after all local
    /// resources have still been cleaned up.
    pub fn shutdown(mut self) -> Result<ShutdownReport, HeadlessError> {
        self.cleanup(true)
    }

    fn validate_submission(
        &self,
        scene: &CompiledScene,
        request: FrameRequest,
    ) -> Result<(), HeadlessError> {
        let contract = scene.scene();
        let revision = contract.document_revision();
        if revision != request.expected_document_revision() {
            return Err(HeadlessError::StaleRevision {
                expected: request.expected_document_revision(),
                actual: revision,
            });
        }
        if let Some(previous) = self.last_revision
            && revision < previous
        {
            return Err(HeadlessError::RevisionRegressed {
                previous,
                actual: revision,
            });
        }
        if let Some(previous) = self.last_epoch
            && request.epoch() <= previous
        {
            return Err(HeadlessError::StaleEpoch {
                previous,
                actual: request.epoch(),
            });
        }
        let viewport = contract.viewport();
        if viewport.width() % APP_UNITS_PER_CSS_PIXEL != 0
            || viewport.height() % APP_UNITS_PER_CSS_PIXEL != 0
        {
            return Err(HeadlessError::FractionalViewport {
                width_app_units: viewport.width(),
                height_app_units: viewport.height(),
            });
        }
        let scene_width =
            u32::try_from(viewport.width() / APP_UNITS_PER_CSS_PIXEL).map_err(|_| {
                HeadlessError::FractionalViewport {
                    width_app_units: viewport.width(),
                    height_app_units: viewport.height(),
                }
            })?;
        let scene_height =
            u32::try_from(viewport.height() / APP_UNITS_PER_CSS_PIXEL).map_err(|_| {
                HeadlessError::FractionalViewport {
                    width_app_units: viewport.width(),
                    height_app_units: viewport.height(),
                }
            })?;
        if scene_width != self.size.width() || scene_height != self.size.height() {
            return Err(HeadlessError::ViewportMismatch {
                scene_width,
                scene_height,
                frame_width: self.size.width(),
                frame_height: self.size.height(),
            });
        }
        enforce(
            ResourceKind::SceneItems,
            contract.items().len(),
            self.limits.max_scene_items(),
        )?;
        enforce(
            ResourceKind::PendingTextRuns,
            contract.pending_text().len(),
            self.limits.max_pending_text_runs(),
        )?;
        enforce(
            ResourceKind::DisplayListBytes,
            scene.built_display_list().size_in_bytes(),
            self.limits.max_display_list_bytes(),
        )
    }

    fn allocate_pixels(&self) -> Result<Vec<u8>, HeadlessError> {
        let mut pixels = Vec::new();
        pixels.try_reserve_exact(self.pixel_bytes).map_err(|_| {
            HeadlessError::PixelAllocationFailed {
                requested: self.pixel_bytes,
            }
        })?;
        pixels.resize(self.pixel_bytes, 0);
        Ok(pixels)
    }

    fn frame_deadline(&self) -> Result<Instant, HeadlessError> {
        Instant::now()
            .checked_add(self.limits.frame_timeout())
            .ok_or(HeadlessError::InvalidLimit {
                field: "frame_timeout_nanos",
                value: self.limits.frame_timeout().as_nanos(),
            })
    }

    fn cleanup(&mut self, wait_for_backend: bool) -> Result<ShutdownReport, HeadlessError> {
        if self.shutdown {
            return Err(HeadlessError::AlreadyShutdown);
        }
        self.shutdown = true;
        let context_activation = if self.renderer.is_some() {
            self.activate_context()
        } else {
            Ok(())
        };
        let api_shutdown_failed = if let Some(api) = self.api.as_ref() {
            catch_unwind(AssertUnwindSafe(|| {
                api.delete_document(self.document_id);
                api.shut_down(false);
            }))
            .is_err()
        } else {
            false
        };
        let backend_acknowledged = if wait_for_backend {
            self.notifier
                .wait_for_shutdown(self.limits.shutdown_timeout())
        } else {
            false
        };
        self.api.take();
        let renderer_deinit = match self.renderer.take() {
            Some(renderer) if context_activation.is_ok() => {
                catch_unwind(AssertUnwindSafe(|| renderer.deinit()))
                    .map_err(|payload| bounded_panic_payload(payload.as_ref()))
            }
            Some(renderer) => {
                // Dropping the CPU-side owner is safer than issuing GL deletion
                // calls through whichever unrelated context may be current.
                drop(renderer);
                Ok(())
            }
            None => Ok(()),
        };
        let context_release = self
            .context
            .take()
            .map_or(Ok(()), LinuxEglContext::release_or_leak);
        let report = ShutdownReport {
            backend_acknowledged,
            context_released: context_release.is_ok(),
            wake_notifications: self.notifier.wake_count(),
            frame_ready_notifications: self.notifier.frame_ready_count(),
        };
        if let Err(detail) = context_release {
            return Err(HeadlessError::ContextRelease { detail });
        }
        if let Err(detail) = renderer_deinit {
            return Err(HeadlessError::RendererDeinitialization { detail });
        }
        context_activation?;
        if api_shutdown_failed {
            return Err(HeadlessError::BackendDisconnected);
        }
        if wait_for_backend && !backend_acknowledged {
            return Err(HeadlessError::FrameTimeout {
                stage: FrameStage::Shutdown,
                timeout: self.limits.shutdown_timeout(),
            });
        }
        Ok(report)
    }

    fn activate_context(&self) -> Result<(), HeadlessError> {
        let context = self
            .context
            .as_ref()
            .ok_or(HeadlessError::AlreadyShutdown)?;
        match catch_unwind(AssertUnwindSafe(|| context.ensure_current())) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(detail)) => Err(HeadlessError::ContextActivation { detail }),
            Err(payload) => Err(HeadlessError::ContextActivation {
                detail: bounded_panic_payload(payload.as_ref()),
            }),
        }
    }

    fn activate_for_render(&mut self) -> Result<(), HeadlessError> {
        self.activate_context().inspect_err(|_| {
            self.unusable = true;
        })
    }
}

impl Drop for HeadlessRenderer {
    fn drop(&mut self) {
        if !self.shutdown {
            let _ = self.cleanup(false);
        }
    }
}

fn device_size(size: FrameSize) -> DeviceIntSize {
    DeviceIntSize::new(size.width().cast_signed(), size.height().cast_signed())
}

fn flip_vertical(pixels: &mut [u8], size: FrameSize) {
    let stride = size.width() as usize * 4;
    for top_row in 0..size.height() as usize / 2 {
        let bottom_row = size.height() as usize - top_row - 1;
        let top_offset = top_row * stride;
        let bottom_offset = bottom_row * stride;
        let (top_and_middle, bottom_and_after) = pixels.split_at_mut(bottom_offset);
        top_and_middle[top_offset..top_offset + stride]
            .swap_with_slice(&mut bottom_and_after[..stride]);
    }
}

fn bounded_panic_payload(payload: &(dyn Any + Send)) -> String {
    const MAX_PANIC_BYTES: usize = 1_024;
    let detail = if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic payload".to_owned()
    };
    if detail.len() <= MAX_PANIC_BYTES {
        return detail;
    }
    let mut boundary = MAX_PANIC_BYTES;
    while !detail.is_char_boundary(boundary) {
        boundary -= 1;
    }
    let mut bounded = detail[..boundary].to_owned();
    bounded.push_str("...");
    bounded
}

#[cfg(test)]
mod tests {
    use super::flip_vertical;
    use crate::FrameSize;

    #[test]
    fn readback_rows_are_normalized_to_top_left_order() {
        let size = FrameSize::new(1, 3).unwrap();
        let mut pixels = vec![
            1, 2, 3, 4, // GL bottom row
            5, 6, 7, 8, 9, 10, 11, 12, // GL top row
        ];
        flip_vertical(&mut pixels, size);
        assert_eq!(pixels, vec![9, 10, 11, 12, 5, 6, 7, 8, 1, 2, 3, 4]);
    }
}
