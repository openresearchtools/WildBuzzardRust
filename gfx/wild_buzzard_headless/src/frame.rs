use std::time::Duration;

use crate::error::{HeadlessError, ResourceKind};

/// Fixed offscreen framebuffer dimensions in device pixels.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct FrameSize {
    width: u32,
    height: u32,
}

impl FrameSize {
    /// Creates dimensions representable by `WebRender` and EGL.
    ///
    /// # Errors
    ///
    /// Rejects zero dimensions and values greater than `i32::MAX`.
    pub const fn new(width: u32, height: u32) -> Result<Self, HeadlessError> {
        if width == 0 || height == 0 || width > i32::MAX as u32 || height > i32::MAX as u32 {
            return Err(HeadlessError::InvalidFrameSize { width, height });
        }
        Ok(Self { width, height })
    }

    /// Returns the width in device pixels.
    #[must_use]
    pub const fn width(self) -> u32 {
        self.width
    }

    /// Returns the height in device pixels.
    #[must_use]
    pub const fn height(self) -> u32 {
        self.height
    }

    pub(crate) fn rgba8_len(self) -> Result<usize, HeadlessError> {
        let pixels = usize::try_from(self.width)
            .ok()
            .and_then(|width| {
                usize::try_from(self.height)
                    .ok()
                    .and_then(|height| width.checked_mul(height))
            })
            .ok_or(HeadlessError::PixelSizeOverflow {
                width: self.width,
                height: self.height,
            })?;
        pixels
            .checked_mul(4)
            .ok_or(HeadlessError::PixelSizeOverflow {
                width: self.width,
                height: self.height,
            })
    }
}

/// Resource and deadline limits for one fixed-size headless renderer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HeadlessLimits {
    max_width: u32,
    max_height: u32,
    max_pixel_bytes: usize,
    max_scene_items: usize,
    max_pending_text_runs: usize,
    max_display_list_bytes: usize,
    frame_timeout: Duration,
    shutdown_timeout: Duration,
    allow_device_contexts: bool,
    allow_x11_fallback: bool,
}

impl Default for HeadlessLimits {
    fn default() -> Self {
        Self {
            max_width: 4_096,
            max_height: 4_096,
            max_pixel_bytes: 64 << 20,
            max_scene_items: 1_000_000,
            max_pending_text_runs: 100_000,
            max_display_list_bytes: 128 << 20,
            frame_timeout: Duration::from_secs(10),
            shutdown_timeout: Duration::from_secs(5),
            allow_device_contexts: true,
            allow_x11_fallback: true,
        }
    }
}

impl HeadlessLimits {
    /// Returns the maximum framebuffer width.
    #[must_use]
    pub const fn max_width(self) -> u32 {
        self.max_width
    }

    /// Returns the maximum framebuffer height.
    #[must_use]
    pub const fn max_height(self) -> u32 {
        self.max_height
    }

    /// Returns the maximum owned RGBA8 byte count.
    #[must_use]
    pub const fn max_pixel_bytes(self) -> usize {
        self.max_pixel_bytes
    }

    /// Returns the maximum scene-item count accepted at submission.
    #[must_use]
    pub const fn max_scene_items(self) -> usize {
        self.max_scene_items
    }

    /// Returns the maximum pending-text count accepted at submission.
    #[must_use]
    pub const fn max_pending_text_runs(self) -> usize {
        self.max_pending_text_runs
    }

    /// Returns the maximum serialized display-list byte count.
    #[must_use]
    pub const fn max_display_list_bytes(self) -> usize {
        self.max_display_list_bytes
    }

    /// Returns the total build-and-render deadline per frame.
    #[must_use]
    pub const fn frame_timeout(self) -> Duration {
        self.frame_timeout
    }

    /// Returns the backend-shutdown acknowledgement deadline.
    #[must_use]
    pub const fn shutdown_timeout(self) -> Duration {
        self.shutdown_timeout
    }

    /// Returns whether an X11-default EGL pbuffer may follow surfaceless attempts.
    #[must_use]
    pub const fn allow_x11_fallback(self) -> bool {
        self.allow_x11_fallback
    }

    /// Returns whether surfaceless EGL device contexts may be attempted.
    #[must_use]
    pub const fn allow_device_contexts(self) -> bool {
        self.allow_device_contexts
    }

    /// Replaces the maximum framebuffer width.
    #[must_use]
    pub const fn with_max_width(mut self, value: u32) -> Self {
        self.max_width = value;
        self
    }

    /// Replaces the maximum framebuffer height.
    #[must_use]
    pub const fn with_max_height(mut self, value: u32) -> Self {
        self.max_height = value;
        self
    }

    /// Replaces the maximum owned RGBA8 byte count.
    #[must_use]
    pub const fn with_max_pixel_bytes(mut self, value: usize) -> Self {
        self.max_pixel_bytes = value;
        self
    }

    /// Replaces the maximum scene-item count.
    #[must_use]
    pub const fn with_max_scene_items(mut self, value: usize) -> Self {
        self.max_scene_items = value;
        self
    }

    /// Replaces the maximum pending-text count.
    #[must_use]
    pub const fn with_max_pending_text_runs(mut self, value: usize) -> Self {
        self.max_pending_text_runs = value;
        self
    }

    /// Replaces the maximum serialized display-list byte count.
    #[must_use]
    pub const fn with_max_display_list_bytes(mut self, value: usize) -> Self {
        self.max_display_list_bytes = value;
        self
    }

    /// Replaces the total frame deadline.
    #[must_use]
    pub const fn with_frame_timeout(mut self, value: Duration) -> Self {
        self.frame_timeout = value;
        self
    }

    /// Replaces the shutdown acknowledgement deadline.
    #[must_use]
    pub const fn with_shutdown_timeout(mut self, value: Duration) -> Self {
        self.shutdown_timeout = value;
        self
    }

    /// Enables or disables the X11-default EGL pbuffer fallback.
    #[must_use]
    pub const fn with_x11_fallback(mut self, value: bool) -> Self {
        self.allow_x11_fallback = value;
        self
    }

    /// Enables or disables surfaceless EGL device contexts.
    #[must_use]
    pub const fn with_device_contexts(mut self, value: bool) -> Self {
        self.allow_device_contexts = value;
        self
    }

    pub(crate) fn validate(self, size: FrameSize) -> Result<usize, HeadlessError> {
        const MAX_FRAME_TIMEOUT: Duration = Duration::from_secs(60);
        const MAX_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);
        if self.max_width == 0 {
            return Err(HeadlessError::InvalidLimit {
                field: "max_width",
                value: 0,
            });
        }
        if self.max_height == 0 {
            return Err(HeadlessError::InvalidLimit {
                field: "max_height",
                value: 0,
            });
        }
        for (field, value) in [
            ("max_pixel_bytes", self.max_pixel_bytes),
            ("max_scene_items", self.max_scene_items),
            ("max_pending_text_runs", self.max_pending_text_runs),
            ("max_display_list_bytes", self.max_display_list_bytes),
        ] {
            if value == 0 {
                return Err(HeadlessError::InvalidLimit { field, value: 0 });
            }
        }
        if self.frame_timeout.is_zero() {
            return Err(HeadlessError::InvalidLimit {
                field: "frame_timeout_nanos",
                value: 0,
            });
        }
        if self.frame_timeout > MAX_FRAME_TIMEOUT {
            return Err(HeadlessError::InvalidLimit {
                field: "frame_timeout_nanos",
                value: self.frame_timeout.as_nanos(),
            });
        }
        if self.shutdown_timeout.is_zero() {
            return Err(HeadlessError::InvalidLimit {
                field: "shutdown_timeout_nanos",
                value: 0,
            });
        }
        if self.shutdown_timeout > MAX_SHUTDOWN_TIMEOUT {
            return Err(HeadlessError::InvalidLimit {
                field: "shutdown_timeout_nanos",
                value: self.shutdown_timeout.as_nanos(),
            });
        }
        enforce(
            ResourceKind::FrameWidth,
            size.width as usize,
            self.max_width as usize,
        )?;
        enforce(
            ResourceKind::FrameHeight,
            size.height as usize,
            self.max_height as usize,
        )?;
        let pixel_bytes = size.rgba8_len()?;
        enforce(ResourceKind::PixelBytes, pixel_bytes, self.max_pixel_bytes)?;
        Ok(pixel_bytes)
    }
}

pub(crate) fn enforce(
    resource: ResourceKind,
    observed: usize,
    limit: usize,
) -> Result<(), HeadlessError> {
    if observed > limit {
        Err(HeadlessError::ResourceLimitExceeded {
            resource,
            observed,
            limit,
        })
    } else {
        Ok(())
    }
}

/// Revision and monotonic `WebRender` epoch for one consumed scene.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameRequest {
    expected_document_revision: u64,
    epoch: u32,
}

impl FrameRequest {
    /// Creates a frame submission request.
    #[must_use]
    pub const fn new(expected_document_revision: u64, epoch: u32) -> Self {
        Self {
            expected_document_revision,
            epoch,
        }
    }

    /// Returns the exact immutable document revision expected by the caller.
    #[must_use]
    pub const fn expected_document_revision(self) -> u64 {
        self.expected_document_revision
    }

    /// Returns the caller-owned monotonically increasing `WebRender` epoch.
    #[must_use]
    pub const fn epoch(self) -> u32 {
        self.epoch
    }
}

/// A bounded owned RGBA8 screenshot in top-left row order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RgbaFrame {
    size: FrameSize,
    stride: usize,
    document_revision: u64,
    epoch: u32,
    pending_text_runs: usize,
    pixels: Box<[u8]>,
}

impl RgbaFrame {
    pub(crate) fn new(
        size: FrameSize,
        document_revision: u64,
        epoch: u32,
        pending_text_runs: usize,
        pixels: Vec<u8>,
    ) -> Self {
        Self {
            size,
            stride: size.width as usize * 4,
            document_revision,
            epoch,
            pending_text_runs,
            pixels: pixels.into_boxed_slice(),
        }
    }

    /// Returns fixed device-pixel dimensions.
    #[must_use]
    pub const fn size(&self) -> FrameSize {
        self.size
    }

    /// Returns bytes per top-left-oriented row.
    #[must_use]
    pub const fn stride(&self) -> usize {
        self.stride
    }

    /// Returns the rendered immutable document revision.
    #[must_use]
    pub const fn document_revision(&self) -> u64 {
        self.document_revision
    }

    /// Returns the submitted `WebRender` epoch.
    #[must_use]
    pub const fn epoch(&self) -> u32 {
        self.epoch
    }

    /// Returns text runs intentionally omitted pending font selection and shaping.
    #[must_use]
    pub const fn pending_text_runs(&self) -> usize {
        self.pending_text_runs
    }

    /// Returns the exact owned RGBA8 bytes in top-left row order.
    #[must_use]
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    /// Returns one pixel when `(x, y)` lies inside the frame.
    #[must_use]
    pub fn pixel(&self, x: u32, y: u32) -> Option<[u8; 4]> {
        if x >= self.size.width || y >= self.size.height {
            return None;
        }
        let offset = y as usize * self.stride + x as usize * 4;
        self.pixels[offset..offset + 4].try_into().ok()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{FrameSize, HeadlessLimits};
    use crate::{HeadlessError, ResourceKind};

    #[test]
    fn frame_size_rejects_zero_and_i32_overflow() {
        assert!(matches!(
            FrameSize::new(0, 1),
            Err(HeadlessError::InvalidFrameSize { .. })
        ));
        assert!(matches!(
            FrameSize::new(i32::MAX as u32 + 1, 1),
            Err(HeadlessError::InvalidFrameSize { .. })
        ));
    }

    #[test]
    fn exact_pixel_limit_is_accepted_and_one_less_is_rejected() {
        let size = FrameSize::new(4, 3).unwrap();
        let limits = HeadlessLimits::default().with_max_pixel_bytes(48);
        assert_eq!(limits.validate(size).unwrap(), 48);
        assert!(matches!(
            limits.with_max_pixel_bytes(47).validate(size),
            Err(HeadlessError::ResourceLimitExceeded {
                resource: ResourceKind::PixelBytes,
                observed: 48,
                limit: 47
            })
        ));
    }

    #[test]
    fn waits_are_nonzero_and_capped() {
        let size = FrameSize::new(4, 3).unwrap();
        assert!(matches!(
            HeadlessLimits::default()
                .with_frame_timeout(Duration::ZERO)
                .validate(size),
            Err(HeadlessError::InvalidLimit {
                field: "frame_timeout_nanos",
                value: 0
            })
        ));
        assert!(matches!(
            HeadlessLimits::default()
                .with_shutdown_timeout(Duration::from_secs(31))
                .validate(size),
            Err(HeadlessError::InvalidLimit {
                field: "shutdown_timeout_nanos",
                ..
            })
        ));
    }
}
