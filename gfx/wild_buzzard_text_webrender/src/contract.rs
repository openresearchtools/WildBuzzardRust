use std::sync::Arc;

use webrender_api::PipelineId;
use wild_buzzard_text::ShapedText;

use crate::error::{TextRenderError, TextRenderResource};

/// Renderer-independent identity for one text-only pipeline.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct TextPipelineKey {
    source: u32,
    pipeline: u32,
}

impl TextPipelineKey {
    #[must_use]
    pub const fn new(source: u32, pipeline: u32) -> Self {
        Self { source, pipeline }
    }

    #[must_use]
    pub const fn source(self) -> u32 {
        self.source
    }

    #[must_use]
    pub const fn pipeline(self) -> u32 {
        self.pipeline
    }

    pub(crate) const fn as_webrender(self) -> PipelineId {
        PipelineId(self.source, self.pipeline)
    }
}

/// CSS-pixel offset added to every already-positioned glyph.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TextOrigin {
    x: f32,
    y: f32,
}

impl TextOrigin {
    #[must_use]
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }

    #[must_use]
    pub const fn x(self) -> f32 {
        self.x
    }

    #[must_use]
    pub const fn y(self) -> f32 {
        self.y
    }
}

/// Non-premultiplied sRGBA8 text color.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TextColor {
    red: u8,
    green: u8,
    blue: u8,
    alpha: u8,
}

impl TextColor {
    pub const BLACK: Self = Self::rgba(0, 0, 0, 255);

    #[must_use]
    pub const fn rgba(red: u8, green: u8, blue: u8, alpha: u8) -> Self {
        Self {
            red,
            green,
            blue,
            alpha,
        }
    }

    #[must_use]
    pub const fn channels(self) -> [u8; 4] {
        [self.red, self.green, self.blue, self.alpha]
    }

    pub(crate) fn as_webrender(self) -> webrender_api::ColorF {
        const BYTE_MAX: f32 = 255.0;
        webrender_api::ColorF::new(
            f32::from(self.red) / BYTE_MAX,
            f32::from(self.green) / BYTE_MAX,
            f32::from(self.blue) / BYTE_MAX,
            f32::from(self.alpha) / BYTE_MAX,
        )
    }
}

/// Exact immutable shaped result and placement for one text-only frame.
#[derive(Clone, Debug)]
pub struct ShapedTextFrame {
    document_revision: u64,
    pipeline: TextPipelineKey,
    shaped: Arc<ShapedText>,
    origin: TextOrigin,
    color: TextColor,
}

impl ShapedTextFrame {
    #[must_use]
    pub fn new(document_revision: u64, pipeline: TextPipelineKey, shaped: Arc<ShapedText>) -> Self {
        Self {
            document_revision,
            pipeline,
            shaped,
            origin: TextOrigin::default(),
            color: TextColor::BLACK,
        }
    }

    #[must_use]
    pub const fn document_revision(&self) -> u64 {
        self.document_revision
    }

    #[must_use]
    pub const fn pipeline(&self) -> TextPipelineKey {
        self.pipeline
    }

    #[must_use]
    pub fn shaped(&self) -> &Arc<ShapedText> {
        &self.shaped
    }

    #[must_use]
    pub const fn origin(&self) -> TextOrigin {
        self.origin
    }

    #[must_use]
    pub const fn color(&self) -> TextColor {
        self.color
    }

    #[must_use]
    pub const fn with_origin(mut self, origin: TextOrigin) -> Self {
        self.origin = origin;
        self
    }

    #[must_use]
    pub const fn with_color(mut self, color: TextColor) -> Self {
        self.color = color;
        self
    }
}

/// Fixed text-frame viewport in device pixels at scale one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TextViewport {
    width: u32,
    height: u32,
}

impl TextViewport {
    #[must_use]
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }

    #[must_use]
    pub const fn width(self) -> u32 {
        self.width
    }

    #[must_use]
    pub const fn height(self) -> u32 {
        self.height
    }
}

/// Independent renderer-boundary limits, rechecked after shaping.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(clippy::struct_field_names)]
pub struct TextRenderLimits {
    pub(crate) max_text_bytes: usize,
    pub(crate) max_runs: usize,
    pub(crate) max_clusters: usize,
    pub(crate) max_glyphs: usize,
    pub(crate) max_font_templates: usize,
    pub(crate) max_font_instances: usize,
    pub(crate) max_font_bytes: usize,
    pub(crate) max_registered_font_bytes: usize,
    pub(crate) max_display_list_bytes: usize,
    pub(crate) max_abs_coordinate_px: u32,
}

impl Default for TextRenderLimits {
    fn default() -> Self {
        Self {
            max_text_bytes: 1 << 20,
            max_runs: 4_096,
            max_clusters: 500_000,
            max_glyphs: 1_000_000,
            max_font_templates: 256,
            max_font_instances: 1_024,
            max_font_bytes: 64 << 20,
            max_registered_font_bytes: 256 << 20,
            max_display_list_bytes: 128 << 20,
            max_abs_coordinate_px: 1_000_000,
        }
    }
}

impl TextRenderLimits {
    #[must_use]
    pub const fn max_font_templates(self) -> usize {
        self.max_font_templates
    }

    #[must_use]
    pub const fn max_font_instances(self) -> usize {
        self.max_font_instances
    }

    #[must_use]
    pub const fn max_registered_font_bytes(self) -> usize {
        self.max_registered_font_bytes
    }

    #[must_use]
    pub const fn max_display_list_bytes(self) -> usize {
        self.max_display_list_bytes
    }

    #[must_use]
    pub const fn with_max_text_bytes(mut self, value: usize) -> Self {
        self.max_text_bytes = value;
        self
    }

    #[must_use]
    pub const fn with_max_runs(mut self, value: usize) -> Self {
        self.max_runs = value;
        self
    }

    #[must_use]
    pub const fn with_max_clusters(mut self, value: usize) -> Self {
        self.max_clusters = value;
        self
    }

    #[must_use]
    pub const fn with_max_glyphs(mut self, value: usize) -> Self {
        self.max_glyphs = value;
        self
    }

    #[must_use]
    pub const fn with_max_font_templates(mut self, value: usize) -> Self {
        self.max_font_templates = value;
        self
    }

    #[must_use]
    pub const fn with_max_font_instances(mut self, value: usize) -> Self {
        self.max_font_instances = value;
        self
    }

    #[must_use]
    pub const fn with_max_font_bytes(mut self, value: usize) -> Self {
        self.max_font_bytes = value;
        self
    }

    #[must_use]
    pub const fn with_max_registered_font_bytes(mut self, value: usize) -> Self {
        self.max_registered_font_bytes = value;
        self
    }

    #[must_use]
    pub const fn with_max_display_list_bytes(mut self, value: usize) -> Self {
        self.max_display_list_bytes = value;
        self
    }

    #[must_use]
    pub const fn with_max_abs_coordinate_px(mut self, value: u32) -> Self {
        self.max_abs_coordinate_px = value;
        self
    }

    pub(crate) fn validate(self) -> Result<(), TextRenderError> {
        for (field, value) in [
            ("max_text_bytes", self.max_text_bytes),
            ("max_runs", self.max_runs),
            ("max_clusters", self.max_clusters),
            ("max_glyphs", self.max_glyphs),
            ("max_font_templates", self.max_font_templates),
            ("max_font_instances", self.max_font_instances),
            ("max_font_bytes", self.max_font_bytes),
            ("max_registered_font_bytes", self.max_registered_font_bytes),
            ("max_display_list_bytes", self.max_display_list_bytes),
        ] {
            if value == 0 {
                return Err(TextRenderError::InvalidLimit { field, value: 0 });
            }
        }
        if self.max_abs_coordinate_px == 0 {
            return Err(TextRenderError::InvalidLimit {
                field: "max_abs_coordinate_px",
                value: 0,
            });
        }
        if self.max_font_bytes > self.max_registered_font_bytes {
            return Err(TextRenderError::InvalidLimit {
                field: "max_font_bytes",
                value: self.max_font_bytes,
            });
        }
        Ok(())
    }
}

/// Renderer-scoped registered resource counts.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TextRegistryStatistics {
    templates: usize,
    instances: usize,
    bytes: usize,
}

impl TextRegistryStatistics {
    pub(crate) const fn new(
        font_templates: usize,
        font_instances: usize,
        font_bytes: usize,
    ) -> Self {
        Self {
            templates: font_templates,
            instances: font_instances,
            bytes: font_bytes,
        }
    }

    #[must_use]
    pub const fn font_templates(self) -> usize {
        self.templates
    }

    #[must_use]
    pub const fn font_instances(self) -> usize {
        self.instances
    }

    #[must_use]
    pub const fn font_bytes(self) -> usize {
        self.bytes
    }
}

/// Resources removed from one renderer namespace during explicit teardown.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RegistryRelease {
    templates: usize,
    instances: usize,
    bytes: usize,
}

impl RegistryRelease {
    pub(crate) const fn new(
        font_templates: usize,
        font_instances: usize,
        font_bytes: usize,
    ) -> Self {
        Self {
            templates: font_templates,
            instances: font_instances,
            bytes: font_bytes,
        }
    }

    #[must_use]
    pub const fn font_templates(self) -> usize {
        self.templates
    }

    #[must_use]
    pub const fn font_instances(self) -> usize {
        self.instances
    }

    #[must_use]
    pub const fn font_bytes(self) -> usize {
        self.bytes
    }
}

pub(crate) fn enforce(
    resource: TextRenderResource,
    observed: usize,
    limit: usize,
) -> Result<(), TextRenderError> {
    if observed > limit {
        Err(TextRenderError::ResourceLimitExceeded {
            resource,
            observed,
            limit,
        })
    } else {
        Ok(())
    }
}
