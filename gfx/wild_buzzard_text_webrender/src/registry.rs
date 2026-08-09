use std::mem::size_of;
use std::ops::Range;
use std::panic::{AssertUnwindSafe, catch_unwind};

use webrender::{RenderApi, Transaction};
use webrender_api::units::{LayoutPoint, LayoutRect, LayoutSize};
use webrender_api::{
    BuiltDisplayList, CommonItemProperties, DisplayListBuilder, DocumentId, Epoch,
    FontInstanceFlags, FontInstanceKey, FontInstanceOptions, FontKey, FontRenderMode,
    GlyphInstance, IdNamespace, PipelineId, SpaceAndClipInfo, SyntheticItalics,
};
use wild_buzzard_text::{FontFace, FontFaceId, GlyphCluster, ShapedRun, ShapedText};

use crate::contract::{
    RegistryRelease, ShapedTextFrame, TextRegistryStatistics, TextRenderLimits, TextViewport,
    enforce,
};
use crate::error::{InvalidRenderField, TextRenderError, TextRenderResource};

#[derive(Clone)]
struct FontEntry {
    face: FontFace,
    key: FontKey,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct InstanceDescriptor {
    face_id: FontFaceId,
    font_size_bits: u32,
    normalized_coordinates: Box<[i16]>,
    embolden: bool,
    skew_degrees_bits: Option<u32>,
}

impl InstanceDescriptor {
    fn from_run(run: &ShapedRun) -> Self {
        Self {
            face_id: run.face().id(),
            font_size_bits: run.font_size_px().to_bits(),
            normalized_coordinates: run.normalized_variation_coordinates().into(),
            embolden: run.synthesis().embolden(),
            skew_degrees_bits: run.synthesis().skew_degrees().map(f32::to_bits),
        }
    }

    fn font_size(&self) -> f32 {
        f32::from_bits(self.font_size_bits)
    }

    fn skew_degrees(&self) -> Option<f32> {
        self.skew_degrees_bits.map(f32::from_bits)
    }

    fn options(&self) -> FontInstanceOptions {
        let mut flags = FontInstanceFlags::SUBPIXEL_POSITION;
        if self.embolden {
            flags |= FontInstanceFlags::SYNTHETIC_BOLD;
        }
        FontInstanceOptions {
            flags,
            synthetic_italics: self
                .skew_degrees()
                .map_or_else(SyntheticItalics::disabled, SyntheticItalics::from_degrees),
            render_mode: FontRenderMode::Alpha,
            _padding: 0,
        }
    }
}

struct InstanceEntry {
    descriptor: InstanceDescriptor,
    key: FontInstanceKey,
}

struct PreparedRun {
    descriptor: InstanceDescriptor,
    glyphs: Vec<GlyphInstance>,
}

struct FramePlan {
    new_faces: Vec<FontFace>,
    new_instances: Vec<InstanceDescriptor>,
    runs: Vec<PreparedRun>,
    registered_font_bytes_after: usize,
}

struct StagedFont {
    face: FontFace,
    key: FontKey,
    bytes: Vec<u8>,
}

struct StagedInstance {
    descriptor: InstanceDescriptor,
    key: FontInstanceKey,
    font_key: FontKey,
}

/// A validated display list and its staged renderer resources.
///
/// The value exclusively borrows its registry until it is either submitted or
/// dropped. This makes it impossible to record keys as live before the exact
/// transaction carrying their resource additions has been accepted by
/// [`RenderApi::send_transaction`].
pub struct PreparedTextFrame<'registry> {
    registry: &'registry mut TextFontRegistry,
    document_revision: u64,
    pipeline_id: PipelineId,
    display_list: BuiltDisplayList,
    staged_fonts: Vec<StagedFont>,
    staged_instances: Vec<StagedInstance>,
    registered_font_bytes_after: usize,
}

impl PreparedTextFrame<'_> {
    /// Returns the immutable document revision represented by this list.
    #[must_use]
    pub const fn document_revision(&self) -> u64 {
        self.document_revision
    }

    /// Returns the renderer pipeline that this list defines.
    #[must_use]
    pub const fn pipeline_id(&self) -> PipelineId {
        self.pipeline_id
    }

    /// Returns how many new raw font templates this transaction will add.
    #[must_use]
    pub fn added_font_templates(&self) -> usize {
        self.staged_fonts.len()
    }

    /// Returns how many new font instances this transaction will add.
    #[must_use]
    pub fn added_font_instances(&self) -> usize {
        self.staged_instances.len()
    }

    /// Returns the exact serialized display-list byte count.
    #[must_use]
    pub fn display_list_bytes(&self) -> usize {
        self.display_list.size_in_bytes()
    }

    /// Adds resources before their first use, installs the display list, and
    /// submits the caller's remaining operations as one transaction.
    ///
    /// Registry state is committed only after `WebRender` accepts the
    /// transaction. The registry remains unchanged if validation returns an
    /// error or if `send_transaction` unwinds because the backend disconnected.
    /// Callers that catch such an unwind must discard the renderer.
    ///
    /// # Errors
    ///
    /// Returns a namespace mismatch before changing the transaction, renderer,
    /// or registry.
    pub fn submit(
        mut self,
        api: &mut RenderApi,
        document_id: DocumentId,
        mut transaction: Transaction,
        epoch: Epoch,
    ) -> Result<PipelineId, TextRenderError> {
        self.registry.validate_namespace(api)?;
        for staged in &mut self.staged_fonts {
            transaction.add_raw_font(
                staged.key,
                std::mem::take(&mut staged.bytes),
                staged.face.collection_index(),
            );
        }
        for staged in &self.staged_instances {
            transaction.add_font_instance(
                staged.key,
                staged.font_key,
                staged.descriptor.font_size(),
                Some(staged.descriptor.options()),
                None,
                Vec::new(),
            );
        }
        transaction.set_display_list(epoch, (self.pipeline_id, self.display_list));
        transaction.set_root_pipeline(self.pipeline_id);

        api.send_transaction(document_id, transaction);

        for staged in self.staged_fonts {
            self.registry.fonts.push(FontEntry {
                face: staged.face,
                key: staged.key,
            });
        }
        for staged in self.staged_instances {
            self.registry.instances.push(InstanceEntry {
                descriptor: staged.descriptor,
                key: staged.key,
            });
        }
        self.registry.font_bytes = self.registered_font_bytes_after;
        Ok(self.pipeline_id)
    }
}

/// Renderer-scoped, bounded map from exact shaped font identities to
/// `WebRender` font and instance keys.
///
/// Entries are never evicted while their display lists may be in flight. The
/// fixed limits therefore fail closed, and [`Self::release_into`] explicitly
/// emits all matching deletion updates during renderer teardown.
pub struct TextFontRegistry {
    namespace: IdNamespace,
    limits: TextRenderLimits,
    fonts: Vec<FontEntry>,
    instances: Vec<InstanceEntry>,
    font_bytes: usize,
}

impl TextFontRegistry {
    /// Creates an empty registry with the validated built-in limits.
    #[must_use]
    pub fn with_default_limits(api: &RenderApi) -> Self {
        Self {
            namespace: api.get_namespace_id(),
            limits: TextRenderLimits::default(),
            fonts: Vec::new(),
            instances: Vec::new(),
            font_bytes: 0,
        }
    }

    /// Creates an empty registry bound to exactly one `RenderApi` namespace.
    ///
    /// # Errors
    ///
    /// Returns a structured error for internally inconsistent or zero limits.
    pub fn new(api: &RenderApi, limits: TextRenderLimits) -> Result<Self, TextRenderError> {
        limits.validate()?;
        Ok(Self {
            namespace: api.get_namespace_id(),
            limits,
            fonts: Vec::new(),
            instances: Vec::new(),
            font_bytes: 0,
        })
    }

    /// Returns this registry's immutable limits.
    #[must_use]
    pub const fn limits(&self) -> TextRenderLimits {
        self.limits
    }

    /// Returns resource counts currently retained by this renderer namespace.
    #[must_use]
    pub fn statistics(&self) -> TextRegistryStatistics {
        TextRegistryStatistics::new(self.fonts.len(), self.instances.len(), self.font_bytes)
    }

    /// Validates and prepares one exact shaped frame without mutating live
    /// registry state or submitting renderer resources.
    ///
    /// No font lookup or shaping occurs here. Non-empty normalized variation
    /// coordinates fail explicitly until the shaping contract carries the axis
    /// tags and user-space values required by `WebRender`.
    ///
    /// # Errors
    ///
    /// Rejects namespace mismatches, malformed or unbounded shaped output,
    /// identity collisions, unsupported variation coordinates, and allocation
    /// failure. Live registry counts are unchanged until the returned value's
    /// [`PreparedTextFrame::submit`] call succeeds.
    pub fn prepare_frame<'registry>(
        &'registry mut self,
        api: &RenderApi,
        frame: &ShapedTextFrame,
        viewport: TextViewport,
    ) -> Result<PreparedTextFrame<'registry>, TextRenderError> {
        self.validate_namespace(api)?;
        let plan = self.plan_frame(frame, viewport)?;
        let staged_fonts = self.stage_fonts(api, &plan.new_faces)?;
        let staged_instances = self.stage_instances(api, &plan.new_instances, &staged_fonts)?;
        let pipeline_id = frame.pipeline().as_webrender();
        let display_list = build_display_list(
            frame,
            viewport,
            pipeline_id,
            &plan.runs,
            &self.instances,
            &staged_instances,
        )?;
        enforce(
            TextRenderResource::DisplayListBytes,
            display_list.size_in_bytes(),
            self.limits.max_display_list_bytes,
        )?;

        self.fonts
            .try_reserve_exact(staged_fonts.len())
            .map_err(|_| allocation(TextRenderResource::FontTemplates, staged_fonts.len()))?;
        self.instances
            .try_reserve_exact(staged_instances.len())
            .map_err(|_| allocation(TextRenderResource::FontInstances, staged_instances.len()))?;

        Ok(PreparedTextFrame {
            registry: self,
            document_revision: frame.document_revision(),
            pipeline_id,
            display_list,
            staged_fonts,
            staged_instances,
            registered_font_bytes_after: plan.registered_font_bytes_after,
        })
    }

    /// Appends matching deletion updates for all owned keys and empties the
    /// registry. Instance deletions precede their backing font deletions.
    ///
    /// # Errors
    ///
    /// Returns a namespace mismatch without changing the transaction or
    /// registry.
    pub fn release_into(
        &mut self,
        api: &RenderApi,
        transaction: &mut Transaction,
    ) -> Result<RegistryRelease, TextRenderError> {
        self.validate_namespace(api)?;
        let report = RegistryRelease::new(self.fonts.len(), self.instances.len(), self.font_bytes);
        for instance in self.instances.drain(..).rev() {
            transaction.delete_font_instance(instance.key);
        }
        for font in self.fonts.drain(..).rev() {
            transaction.delete_font(font.key);
        }
        self.font_bytes = 0;
        Ok(report)
    }

    fn validate_namespace(&self, api: &RenderApi) -> Result<(), TextRenderError> {
        let actual = api.get_namespace_id();
        if actual == self.namespace {
            Ok(())
        } else {
            Err(TextRenderError::RendererNamespaceMismatch {
                expected: self.namespace,
                actual,
            })
        }
    }

    #[allow(clippy::too_many_lines)]
    fn plan_frame(
        &self,
        frame: &ShapedTextFrame,
        viewport: TextViewport,
    ) -> Result<FramePlan, TextRenderError> {
        validate_frame_header(frame, viewport, self.limits)?;
        let shaped = frame.shaped();
        validate_metrics(shaped, self.limits)?;
        enforce(
            TextRenderResource::Runs,
            shaped.runs().len(),
            self.limits.max_runs,
        )?;

        let mut new_faces = Vec::new();
        new_faces
            .try_reserve_exact(shaped.runs().len().min(self.limits.max_font_templates))
            .map_err(|_| {
                allocation(
                    TextRenderResource::FontTemplates,
                    shaped.runs().len().min(self.limits.max_font_templates),
                )
            })?;
        let mut new_instances = Vec::new();
        new_instances
            .try_reserve_exact(shaped.runs().len().min(self.limits.max_font_instances))
            .map_err(|_| {
                allocation(
                    TextRenderResource::FontInstances,
                    shaped.runs().len().min(self.limits.max_font_instances),
                )
            })?;
        let mut runs = Vec::new();
        runs.try_reserve_exact(shaped.runs().len())
            .map_err(|_| allocation(TextRenderResource::Runs, shaped.runs().len()))?;
        let mut cluster_count = 0_usize;
        let mut glyph_count = 0_usize;
        let mut new_font_bytes = 0_usize;

        for run in shaped.runs() {
            validate_run(shaped, run, frame.origin(), self.limits)?;
            cluster_count = checked_accumulate(
                TextRenderResource::Clusters,
                cluster_count,
                run.clusters().len(),
                self.limits.max_clusters,
            )?;
            glyph_count = checked_accumulate(
                TextRenderResource::Glyphs,
                glyph_count,
                run.glyphs().len(),
                self.limits.max_glyphs,
            )?;

            if self.find_font(run.face())?.is_none()
                && find_exact_face(&new_faces, run.face())?.is_none()
            {
                enforce(
                    TextRenderResource::FontTemplates,
                    self.fonts
                        .len()
                        .saturating_add(new_faces.len())
                        .saturating_add(1),
                    self.limits.max_font_templates,
                )?;
                enforce(
                    TextRenderResource::FontBytes,
                    run.face().bytes().len(),
                    self.limits.max_font_bytes,
                )?;
                new_font_bytes = checked_accumulate(
                    TextRenderResource::RegisteredFontBytes,
                    new_font_bytes,
                    run.face().bytes().len(),
                    self.limits
                        .max_registered_font_bytes
                        .saturating_sub(self.font_bytes),
                )?;
                new_faces.push(run.face().clone());
            }

            let descriptor = InstanceDescriptor::from_run(run);
            if self.find_instance(&descriptor).is_none() && !new_instances.contains(&descriptor) {
                enforce(
                    TextRenderResource::FontInstances,
                    self.instances
                        .len()
                        .saturating_add(new_instances.len())
                        .saturating_add(1),
                    self.limits.max_font_instances,
                )?;
                new_instances.push(descriptor.clone());
            }

            let mut glyphs = Vec::new();
            glyphs
                .try_reserve_exact(run.glyphs().len())
                .map_err(|_| allocation(TextRenderResource::Glyphs, run.glyphs().len()))?;
            for glyph in run.glyphs() {
                glyphs.push(GlyphInstance {
                    index: glyph.id(),
                    point: LayoutPoint::new(
                        frame.origin().x() + glyph.x(),
                        frame.origin().y() + glyph.y(),
                    ),
                });
            }
            runs.push(PreparedRun { descriptor, glyphs });
        }

        enforce(
            TextRenderResource::FontTemplates,
            self.fonts.len().saturating_add(new_faces.len()),
            self.limits.max_font_templates,
        )?;
        enforce(
            TextRenderResource::FontInstances,
            self.instances.len().saturating_add(new_instances.len()),
            self.limits.max_font_instances,
        )?;
        let registered_font_bytes_after = self.font_bytes.saturating_add(new_font_bytes);
        enforce(
            TextRenderResource::RegisteredFontBytes,
            registered_font_bytes_after,
            self.limits.max_registered_font_bytes,
        )?;
        preflight_display_list_bytes(shaped.runs().len(), glyph_count, self.limits)?;

        Ok(FramePlan {
            new_faces,
            new_instances,
            runs,
            registered_font_bytes_after,
        })
    }

    fn stage_fonts(
        &self,
        api: &RenderApi,
        faces: &[FontFace],
    ) -> Result<Vec<StagedFont>, TextRenderError> {
        let mut staged = Vec::new();
        staged
            .try_reserve_exact(faces.len())
            .map_err(|_| allocation(TextRenderResource::FontTemplates, faces.len()))?;
        for face in faces {
            let mut bytes = Vec::new();
            bytes
                .try_reserve_exact(face.bytes().len())
                .map_err(|_| allocation(TextRenderResource::FontBytes, face.bytes().len()))?;
            bytes.extend_from_slice(face.bytes());
            let key = api.generate_font_key();
            self.validate_generated_namespace(key.0)?;
            staged.push(StagedFont {
                face: face.clone(),
                key,
                bytes,
            });
        }
        Ok(staged)
    }

    fn stage_instances(
        &self,
        api: &RenderApi,
        descriptors: &[InstanceDescriptor],
        staged_fonts: &[StagedFont],
    ) -> Result<Vec<StagedInstance>, TextRenderError> {
        let mut staged = Vec::new();
        staged
            .try_reserve_exact(descriptors.len())
            .map_err(|_| allocation(TextRenderResource::FontInstances, descriptors.len()))?;
        for descriptor in descriptors {
            let font_key = self
                .fonts
                .iter()
                .find(|entry| entry.face.id() == descriptor.face_id)
                .map(|entry| entry.key)
                .or_else(|| {
                    staged_fonts
                        .iter()
                        .find(|entry| entry.face.id() == descriptor.face_id)
                        .map(|entry| entry.key)
                })
                .ok_or(TextRenderError::FontIdentityCollision {
                    id: descriptor.face_id,
                })?;
            let key = api.generate_font_instance_key();
            self.validate_generated_namespace(key.0)?;
            staged.push(StagedInstance {
                descriptor: descriptor.clone(),
                key,
                font_key,
            });
        }
        Ok(staged)
    }

    fn validate_generated_namespace(&self, actual: IdNamespace) -> Result<(), TextRenderError> {
        if actual == self.namespace {
            Ok(())
        } else {
            Err(TextRenderError::GeneratedKeyNamespaceMismatch {
                expected: self.namespace,
                actual,
            })
        }
    }

    fn find_font(&self, face: &FontFace) -> Result<Option<FontKey>, TextRenderError> {
        match self.fonts.iter().find(|entry| entry.face.id() == face.id()) {
            Some(entry) if entry.face.exactly_matches(face) => Ok(Some(entry.key)),
            Some(_) => Err(TextRenderError::FontIdentityCollision { id: face.id() }),
            None => Ok(None),
        }
    }

    fn find_instance(&self, descriptor: &InstanceDescriptor) -> Option<FontInstanceKey> {
        self.instances
            .iter()
            .find(|entry| entry.descriptor == *descriptor)
            .map(|entry| entry.key)
    }
}

fn validate_frame_header(
    frame: &ShapedTextFrame,
    viewport: TextViewport,
    limits: TextRenderLimits,
) -> Result<(), TextRenderError> {
    if frame.pipeline().as_webrender() == webrender_api::PipelineId::INVALID {
        return Err(TextRenderError::InvalidValue {
            field: InvalidRenderField::Pipeline,
        });
    }
    if viewport.width() == 0
        || viewport.height() == 0
        || viewport.width() > limits.max_abs_coordinate_px
        || viewport.height() > limits.max_abs_coordinate_px
        || viewport.width() > i32::MAX as u32
        || viewport.height() > i32::MAX as u32
    {
        return Err(TextRenderError::InvalidValue {
            field: InvalidRenderField::Viewport,
        });
    }
    enforce(
        TextRenderResource::TextBytes,
        frame.shaped().text().len(),
        limits.max_text_bytes,
    )?;
    validate_coordinate(frame.origin().x(), limits, InvalidRenderField::Origin)?;
    validate_coordinate(frame.origin().y(), limits, InvalidRenderField::Origin)
}

fn validate_metrics(shaped: &ShapedText, limits: TextRenderLimits) -> Result<(), TextRenderError> {
    let metrics = shaped.metrics();
    for value in [
        metrics.width(),
        metrics.full_width(),
        metrics.height(),
        metrics.first_baseline(),
        metrics.ascent(),
        metrics.descent(),
        metrics.leading(),
        metrics.line_height(),
    ] {
        validate_coordinate(value, limits, InvalidRenderField::TextMetric)?;
    }
    Ok(())
}

fn validate_run(
    shaped: &ShapedText,
    run: &ShapedRun,
    origin: crate::TextOrigin,
    limits: TextRenderLimits,
) -> Result<(), TextRenderError> {
    validate_positive(run.font_size_px(), limits, InvalidRenderField::FontSize)?;
    validate_coordinate(run.advance(), limits, InvalidRenderField::RunAdvance)?;
    let run_range = run.text_range();
    validate_text_range(shaped.text(), &run_range, InvalidRenderField::RunRange)?;
    if run.face().bytes().is_empty() {
        return Err(TextRenderError::InvalidValue {
            field: InvalidRenderField::FontData,
        });
    }
    if !run.normalized_variation_coordinates().is_empty() {
        return Err(TextRenderError::UnsupportedNormalizedVariations {
            coordinate_count: run.normalized_variation_coordinates().len(),
        });
    }
    if let Some(skew) = run.synthesis().skew_degrees()
        && (!skew.is_finite() || !(-89.0..=89.0).contains(&skew))
    {
        return Err(TextRenderError::InvalidValue {
            field: InvalidRenderField::SyntheticSkew,
        });
    }
    let metrics = run.metrics();
    for value in [
        metrics.ascent,
        metrics.descent,
        metrics.leading,
        metrics.underline_offset,
        metrics.underline_size,
        metrics.strikethrough_offset,
        metrics.strikethrough_size,
        metrics.line_height,
    ] {
        validate_coordinate(value, limits, InvalidRenderField::RunMetric)?;
    }
    for optional in [metrics.x_height, metrics.cap_height].into_iter().flatten() {
        validate_coordinate(optional, limits, InvalidRenderField::RunMetric)?;
    }

    let mut next_glyph_index = 0;
    for (cluster_index, cluster) in run.clusters().iter().enumerate() {
        validate_cluster(
            shaped.text(),
            &run_range,
            run.glyphs().len(),
            cluster,
            limits,
        )?;
        let glyph_range = cluster.glyph_range();
        if glyph_range.start != next_glyph_index {
            return Err(TextRenderError::InvalidValue {
                field: InvalidRenderField::GlyphRange,
            });
        }
        next_glyph_index = glyph_range.end;
        for glyph_index in glyph_range {
            let glyph = &run.glyphs()[glyph_index];
            if glyph.cluster_index() as usize != cluster_index {
                return Err(TextRenderError::InvalidValue {
                    field: InvalidRenderField::GlyphRange,
                });
            }
        }
    }
    if next_glyph_index != run.glyphs().len() {
        return Err(TextRenderError::InvalidValue {
            field: InvalidRenderField::GlyphRange,
        });
    }
    for glyph in run.glyphs() {
        if glyph.cluster_index() as usize >= run.clusters().len() {
            return Err(TextRenderError::InvalidValue {
                field: InvalidRenderField::GlyphRange,
            });
        }
        validate_coordinate(glyph.x(), limits, InvalidRenderField::GlyphPosition)?;
        validate_coordinate(glyph.y(), limits, InvalidRenderField::GlyphPosition)?;
        validate_coordinate(
            origin.x() + glyph.x(),
            limits,
            InvalidRenderField::GlyphPosition,
        )?;
        validate_coordinate(
            origin.y() + glyph.y(),
            limits,
            InvalidRenderField::GlyphPosition,
        )?;
        validate_coordinate(glyph.advance(), limits, InvalidRenderField::GlyphAdvance)?;
    }
    Ok(())
}

fn validate_cluster(
    text: &str,
    run_range: &Range<usize>,
    glyph_count: usize,
    cluster: &GlyphCluster,
    limits: TextRenderLimits,
) -> Result<(), TextRenderError> {
    let text_range = cluster.text_range();
    validate_text_range(text, &text_range, InvalidRenderField::ClusterRange)?;
    if text_range.start < run_range.start || text_range.end > run_range.end {
        return Err(TextRenderError::InvalidValue {
            field: InvalidRenderField::ClusterRange,
        });
    }
    let glyph_range = cluster.glyph_range();
    if glyph_range.start > glyph_range.end || glyph_range.end > glyph_count {
        return Err(TextRenderError::InvalidValue {
            field: InvalidRenderField::GlyphRange,
        });
    }
    validate_coordinate(cluster.x(), limits, InvalidRenderField::GlyphPosition)?;
    validate_coordinate(cluster.advance(), limits, InvalidRenderField::GlyphAdvance)
}

fn validate_text_range(
    text: &str,
    range: &Range<usize>,
    field: InvalidRenderField,
) -> Result<(), TextRenderError> {
    if range.start <= range.end
        && range.end <= text.len()
        && text.is_char_boundary(range.start)
        && text.is_char_boundary(range.end)
    {
        Ok(())
    } else {
        Err(TextRenderError::InvalidValue { field })
    }
}

fn validate_positive(
    value: f32,
    limits: TextRenderLimits,
    field: InvalidRenderField,
) -> Result<(), TextRenderError> {
    if value > 0.0 {
        validate_coordinate(value, limits, field)
    } else {
        Err(TextRenderError::InvalidValue { field })
    }
}

fn validate_coordinate(
    value: f32,
    limits: TextRenderLimits,
    field: InvalidRenderField,
) -> Result<(), TextRenderError> {
    if value.is_finite() && f64::from(value.abs()) <= f64::from(limits.max_abs_coordinate_px) {
        Ok(())
    } else {
        Err(TextRenderError::InvalidValue { field })
    }
}

fn find_exact_face<'a>(
    faces: &'a [FontFace],
    face: &FontFace,
) -> Result<Option<&'a FontFace>, TextRenderError> {
    match faces.iter().find(|known| known.id() == face.id()) {
        Some(known) if known.exactly_matches(face) => Ok(Some(known)),
        Some(_) => Err(TextRenderError::FontIdentityCollision { id: face.id() }),
        None => Ok(None),
    }
}

fn checked_accumulate(
    resource: TextRenderResource,
    current: usize,
    addition: usize,
    limit: usize,
) -> Result<usize, TextRenderError> {
    let observed = current
        .checked_add(addition)
        .ok_or(TextRenderError::ResourceLimitExceeded {
            resource,
            observed: usize::MAX,
            limit,
        })?;
    enforce(resource, observed, limit)?;
    Ok(observed)
}

fn preflight_display_list_bytes(
    run_count: usize,
    glyph_count: usize,
    limits: TextRenderLimits,
) -> Result<(), TextRenderError> {
    const CONSERVATIVE_RUN_BYTES: usize = 512;
    const FIXED_BYTES: usize = 4_096;
    let observed = glyph_count
        .checked_mul(size_of::<GlyphInstance>())
        .and_then(|glyph_bytes| {
            run_count
                .checked_mul(CONSERVATIVE_RUN_BYTES)
                .and_then(|run_bytes| glyph_bytes.checked_add(run_bytes))
        })
        .and_then(|bytes| bytes.checked_add(FIXED_BYTES))
        .ok_or(TextRenderError::ResourceLimitExceeded {
            resource: TextRenderResource::DisplayListBytes,
            observed: usize::MAX,
            limit: limits.max_display_list_bytes,
        })?;
    enforce(
        TextRenderResource::DisplayListBytes,
        observed,
        limits.max_display_list_bytes,
    )
}

#[allow(clippy::cast_precision_loss)]
fn build_display_list(
    frame: &ShapedTextFrame,
    viewport: TextViewport,
    pipeline_id: webrender_api::PipelineId,
    runs: &[PreparedRun],
    existing_instances: &[InstanceEntry],
    staged_instances: &[StagedInstance],
) -> Result<webrender_api::BuiltDisplayList, TextRenderError> {
    catch_unwind(AssertUnwindSafe(|| {
        let bounds = LayoutRect::from_size(LayoutSize::new(
            viewport.width() as f32,
            viewport.height() as f32,
        ));
        let root = SpaceAndClipInfo::root_scroll(pipeline_id);
        let mut builder = DisplayListBuilder::new(pipeline_id);
        builder.begin();
        let clip_id = builder.define_clip_rect(root.spatial_id, bounds);
        let clip_chain_id = builder.define_clip_chain(None, [clip_id]);
        let common = CommonItemProperties::new(
            bounds,
            SpaceAndClipInfo {
                spatial_id: root.spatial_id,
                clip_chain_id,
            },
        );
        for run in runs {
            let key = existing_instances
                .iter()
                .find(|entry| entry.descriptor == run.descriptor)
                .map(|entry| entry.key)
                .or_else(|| {
                    staged_instances
                        .iter()
                        .find(|entry| entry.descriptor == run.descriptor)
                        .map(|entry| entry.key)
                })
                .ok_or(TextRenderError::MissingFontInstance {
                    id: run.descriptor.face_id,
                })?;
            builder.push_text(
                &common,
                bounds,
                &run.glyphs,
                key,
                frame.color().as_webrender(),
                None,
            );
        }
        let (_, display_list) = builder.end();
        Ok(display_list)
    }))
    .map_err(|_| TextRenderError::DisplayListBuildFailed)?
}

const fn allocation(resource: TextRenderResource, requested: usize) -> TextRenderError {
    TextRenderError::AllocationFailed {
        resource,
        requested,
    }
}

#[cfg(test)]
mod tests {
    use webrender_api::{FontInstanceKey, FontKey, IdNamespace};
    use wild_buzzard_text::{TextLimits, TextRequest, TextSystem};

    use super::{FontEntry, InstanceEntry, TextFontRegistry, build_display_list};
    use crate::{
        InvalidRenderField, ShapedTextFrame, TextOrigin, TextPipelineKey, TextRenderError,
        TextRenderLimits, TextRenderResource, TextViewport,
    };

    const NAMESPACE: IdNamespace = IdNamespace(17);
    const VIEWPORT: TextViewport = TextViewport::new(160, 64);

    fn shaped_frame() -> ShapedTextFrame {
        let mut system = TextSystem::new_deterministic(TextLimits::default()).unwrap();
        let shaped = system.shape(&TextRequest::new("Rust ->", 24.0)).unwrap();
        ShapedTextFrame::new(1, TextPipelineKey::new(8, 3), shaped)
            .with_origin(TextOrigin::new(8.0, 4.0))
    }

    fn registry(limits: TextRenderLimits) -> TextFontRegistry {
        TextFontRegistry {
            namespace: NAMESPACE,
            limits,
            fonts: Vec::new(),
            instances: Vec::new(),
            font_bytes: 0,
        }
    }

    #[test]
    fn plan_revalidates_bounds_before_creating_renderer_resources() {
        let frame = shaped_frame();
        let glyph_limit = frame.shaped().glyph_count() - 1;
        let error = registry(TextRenderLimits::default().with_max_glyphs(glyph_limit))
            .plan_frame(&frame, VIEWPORT)
            .err()
            .expect("renderer boundary must enforce its own glyph limit");
        assert!(matches!(
            error,
            TextRenderError::ResourceLimitExceeded {
                resource: TextRenderResource::Glyphs,
                observed,
                limit,
            } if observed == frame.shaped().glyph_count() && limit == glyph_limit
        ));

        let invalid_pipeline = ShapedTextFrame::new(
            1,
            TextPipelineKey::new(u32::MAX, u32::MAX),
            frame.shaped().clone(),
        );
        assert!(matches!(
            registry(TextRenderLimits::default()).plan_frame(&invalid_pipeline, VIEWPORT),
            Err(TextRenderError::InvalidValue {
                field: InvalidRenderField::Pipeline,
            })
        ));

        let invalid_origin =
            ShapedTextFrame::new(1, TextPipelineKey::new(8, 3), frame.shaped().clone())
                .with_origin(TextOrigin::new(f32::NAN, 0.0));
        assert!(matches!(
            registry(TextRenderLimits::default()).plan_frame(&invalid_origin, VIEWPORT),
            Err(TextRenderError::InvalidValue {
                field: InvalidRenderField::Origin,
            })
        ));
    }

    #[test]
    fn exact_face_and_instance_are_reused_and_emit_a_real_text_item() {
        let frame = shaped_frame();
        let mut registry = registry(TextRenderLimits::default());
        let first = registry.plan_frame(&frame, VIEWPORT).unwrap();
        assert_eq!(first.new_faces.len(), 1);
        assert_eq!(first.new_instances.len(), 1);
        assert_eq!(first.runs.len(), frame.shaped().runs().len());

        registry.font_bytes = first.registered_font_bytes_after;
        registry.fonts.push(FontEntry {
            face: first.new_faces[0].clone(),
            key: FontKey(NAMESPACE, 1),
        });
        registry.instances.push(InstanceEntry {
            descriptor: first.new_instances[0].clone(),
            key: FontInstanceKey(NAMESPACE, 2),
        });
        let reused = registry.plan_frame(&frame, VIEWPORT).unwrap();
        assert!(reused.new_faces.is_empty());
        assert!(reused.new_instances.is_empty());
        let display_list = build_display_list(
            &frame,
            VIEWPORT,
            frame.pipeline().as_webrender(),
            &reused.runs,
            &registry.instances,
            &[],
        )
        .unwrap();
        assert!(display_list.size_in_bytes() > 0);
    }

    #[test]
    fn missing_prepared_instance_is_a_typed_error() {
        let frame = shaped_frame();
        let registry = registry(TextRenderLimits::default());
        let plan = registry.plan_frame(&frame, VIEWPORT).unwrap();
        assert!(matches!(
            build_display_list(
                &frame,
                VIEWPORT,
                frame.pipeline().as_webrender(),
                &plan.runs,
                &[],
                &[],
            ),
            Err(TextRenderError::MissingFontInstance { id })
                if id == frame.shaped().runs()[0].face().id()
        ));
    }
}
