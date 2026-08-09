use std::borrow::Cow;
use std::collections::VecDeque;
use std::mem::size_of;
use std::sync::Arc;

use fontique::{Collection, CollectionOptions, SourceCache};
use icu_properties::props::Script;
use icu_properties::{CodePointMapData, PropertyNamesShort};
use linebender_resource_handle::Blob;
use parley::layout::PositionedLayoutItem;
use parley::setting::Tag as ParleyTag;
use parley::{Alignment, AlignmentOptions, FontContext, LayoutContext, StyleProperty};
use parley::{
    FontFamily as ParleyFontFamily, FontFamilyName as ParleyFontFamilyName,
    FontFeature as ParleyFontFeature, FontFeatures as ParleyFontFeatures,
    FontStyle as ParleyFontStyle, FontVariation as ParleyFontVariation,
    FontVariations as ParleyFontVariations, FontWeight as ParleyFontWeight,
    FontWidth as ParleyFontWidth, GenericFamily as ParleyGenericFamily, Language as ParleyLanguage,
    LineHeight as ParleyLineHeight,
};

use crate::contract::{
    CacheStatistics, FontFace, FontFamily, FontFeature, FontStyle, FontSynthesis, FontVariation,
    GenericFamily, GlyphCluster, LineHeight, PositionedGlyph, RunDirection, RunMetrics, ScriptTag,
    ShapedRun, ShapedText, TextDirection, TextMetrics, TextRequest,
};
use crate::error::{InvalidTextField, TextError, TextResource};
use crate::limits::TextLimits;

const EMBEDDED_FAMILY: &str = "Fira Code";
const EMBEDDED_FIRA_CODE: &[u8] = include_bytes!("../res/FiraCode-Regular.ttf");
const COLLECTION_REVISION: u64 = 1;

/// Which font sources are visible to a text system.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FontSourcePolicy {
    /// Only the pinned Fira Code font is visible. Intended for deterministic tests.
    EmbeddedOnly,
    /// Linux Fontconfig fonts are visible, with pinned Fira Code as final fallback.
    LinuxSystemWithEmbeddedFallback,
}

/// Counts released system-owned cache state during explicit shutdown.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TextShutdownReport {
    cached_shapes_released: usize,
    accounted_cache_bytes_released: usize,
}

impl TextShutdownReport {
    #[must_use]
    pub const fn cached_shapes_released(self) -> usize {
        self.cached_shapes_released
    }

    #[must_use]
    pub const fn accounted_cache_bytes_released(self) -> usize {
        self.accounted_cache_bytes_released
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RequestKey {
    text: String,
    families: Vec<FontFamily>,
    font_size_bits: u32,
    line_height: LineHeightKey,
    weight_bits: u32,
    stretch_bits: u32,
    style: FontStyleKey,
    language: Option<String>,
    direction: TextDirection,
    features: Vec<FontFeature>,
    variations: Vec<FontVariationKey>,
    letter_spacing_bits: u32,
    word_spacing_bits: u32,
    collection_revision: u64,
}

impl RequestKey {
    fn new(request: &TextRequest) -> Result<Self, TextError> {
        let text = try_clone_str(TextResource::TextBytes, request.text())?;
        let mut families = Vec::new();
        families
            .try_reserve_exact(request.families().len())
            .map_err(|_| allocation(TextResource::FontFamilies, request.families().len()))?;
        for family in request.families() {
            families.push(match family {
                FontFamily::Named(name) => {
                    FontFamily::Named(try_clone_str(TextResource::FamilyNameBytes, name)?)
                }
                FontFamily::Generic(generic) => FontFamily::Generic(*generic),
            });
        }
        let language = request
            .language()
            .map(|language| try_clone_str(TextResource::LanguageBytes, language))
            .transpose()?;
        let mut features = Vec::new();
        features
            .try_reserve_exact(request.features().len())
            .map_err(|_| allocation(TextResource::FeatureSettings, request.features().len()))?;
        features.extend_from_slice(request.features());
        let mut variations = Vec::new();
        variations
            .try_reserve_exact(request.variations().len())
            .map_err(|_| allocation(TextResource::VariationSettings, request.variations().len()))?;
        variations.extend(
            request
                .variations()
                .iter()
                .copied()
                .map(FontVariationKey::from),
        );

        Ok(Self {
            text,
            families,
            font_size_bits: request.font_size_px().to_bits(),
            line_height: request.line_height().into(),
            weight_bits: request.weight().value().to_bits(),
            stretch_bits: request.stretch().ratio().to_bits(),
            style: request.style().into(),
            language,
            direction: request.direction(),
            features,
            variations,
            letter_spacing_bits: request.letter_spacing_px().to_bits(),
            word_spacing_bits: request.word_spacing_px().to_bits(),
            collection_revision: COLLECTION_REVISION,
        })
    }

    fn owned_heap_bytes(&self) -> usize {
        let family_names = self.families.iter().fold(0_usize, |total, family| {
            total.saturating_add(match family {
                FontFamily::Named(name) => owned_buffer_bytes(name.capacity()),
                FontFamily::Generic(_) => 0,
            })
        });
        owned_buffer_bytes(self.text.capacity())
            .saturating_add(owned_vec_bytes::<FontFamily>(self.families.capacity()))
            .saturating_add(family_names)
            .saturating_add(
                self.language
                    .as_ref()
                    .map_or(0, |language| owned_buffer_bytes(language.capacity())),
            )
            .saturating_add(owned_vec_bytes::<FontFeature>(self.features.capacity()))
            .saturating_add(owned_vec_bytes::<FontVariationKey>(
                self.variations.capacity(),
            ))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LineHeightKey {
    Normal,
    Used {
        px_bits: u32,
        provenance: crate::LineHeightProvenance,
    },
}

impl From<LineHeight> for LineHeightKey {
    fn from(value: LineHeight) -> Self {
        match value {
            LineHeight::Normal => Self::Normal,
            LineHeight::Used { px, provenance } => Self::Used {
                px_bits: px.to_bits(),
                provenance,
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FontStyleKey {
    Normal,
    Italic,
    Oblique(u32),
}

impl From<FontStyle> for FontStyleKey {
    fn from(value: FontStyle) -> Self {
        match value {
            FontStyle::Normal => Self::Normal,
            FontStyle::Italic => Self::Italic,
            FontStyle::Oblique { degrees } => Self::Oblique(degrees.to_bits()),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FontVariationKey {
    tag: [u8; 4],
    value_bits: u32,
}

impl From<FontVariation> for FontVariationKey {
    fn from(value: FontVariation) -> Self {
        Self {
            tag: value.tag(),
            value_bits: value.value().to_bits(),
        }
    }
}

struct CacheEntry {
    key: RequestKey,
    shaped: Arc<ShapedText>,
    accounted_bytes: usize,
}

/// Stateful font selection and shaping context.
///
/// `TextSystem` owns Parley's scratch contexts and a bounded full-key cache, so
/// shaping takes `&mut self`. Returned `Arc<ShapedText>` values own their font
/// blob handles and remain valid after this system is dropped.
pub struct TextSystem {
    font_context: FontContext,
    layout_context: LayoutContext<()>,
    limits: TextLimits,
    source_policy: FontSourcePolicy,
    cache: VecDeque<CacheEntry>,
    cache_bytes: usize,
    cache_hits: u64,
    cache_misses: u64,
}

impl TextSystem {
    /// Creates a deterministic context containing only pinned Fira Code.
    ///
    /// # Errors
    ///
    /// Returns a structured error if configured limits reject the font or the
    /// pinned font can no longer be parsed as its expected family.
    pub fn new_deterministic(limits: TextLimits) -> Result<Self, TextError> {
        Self::new_with_font(
            FontSourcePolicy::EmbeddedOnly,
            limits,
            EMBEDDED_FIRA_CODE,
            EMBEDDED_FAMILY,
        )
    }

    /// Creates the Linux production selection context.
    ///
    /// Fontconfig is loaded through Fontique's `fontconfig-dlopen` feature. If
    /// the host library is absent, Fontique contributes no system families and
    /// the pinned embedded font remains available.
    ///
    /// # Errors
    ///
    /// Returns a structured error if configured limits reject the embedded
    /// fallback or the font cannot be registered.
    pub fn new_linux(limits: TextLimits) -> Result<Self, TextError> {
        Self::new_with_font(
            FontSourcePolicy::LinuxSystemWithEmbeddedFallback,
            limits,
            EMBEDDED_FIRA_CODE,
            EMBEDDED_FAMILY,
        )
    }

    fn new_with_font(
        source_policy: FontSourcePolicy,
        limits: TextLimits,
        fallback_font: &'static [u8],
        expected_family: &str,
    ) -> Result<Self, TextError> {
        enforce_limit(
            TextResource::FontBytes,
            fallback_font.len(),
            limits.max_font_bytes,
        )?;
        enforce_limit(
            TextResource::TotalFontBytes,
            fallback_font.len(),
            limits.max_total_font_bytes,
        )?;

        let mut collection = Collection::new(CollectionOptions {
            shared: false,
            system_fonts: matches!(
                source_policy,
                FontSourcePolicy::LinuxSystemWithEmbeddedFallback
            ),
        });
        let blob = Blob::new(Arc::new(fallback_font));
        if collection.register_fonts(blob, None).is_empty() {
            return Err(TextError::EmbeddedFontRejected);
        }
        if collection.family_id(expected_family).is_none() {
            return Err(TextError::EmbeddedFontFamilyMissing);
        }

        Ok(Self {
            font_context: FontContext {
                collection,
                source_cache: SourceCache::default(),
            },
            layout_context: LayoutContext::new(),
            limits,
            source_policy,
            cache: VecDeque::new(),
            cache_bytes: 0,
            cache_hits: 0,
            cache_misses: 0,
        })
    }

    #[must_use]
    pub const fn limits(&self) -> TextLimits {
        self.limits
    }

    #[must_use]
    pub const fn source_policy(&self) -> FontSourcePolicy {
        self.source_policy
    }

    #[must_use]
    pub const fn collection_revision(&self) -> u64 {
        COLLECTION_REVISION
    }

    #[must_use]
    pub fn cache_statistics(&self) -> CacheStatistics {
        CacheStatistics {
            entries: self.cache.len(),
            accounted_bytes: self.cache_bytes,
            hits: self.cache_hits,
            misses: self.cache_misses,
        }
    }

    /// Removes all system-owned shaped-result cache entries. Existing result
    /// handles and their selected font blobs remain valid in their callers.
    pub fn clear_shape_cache(&mut self) {
        self.cache.clear();
        self.cache_bytes = 0;
    }

    /// Shapes one validated, unwrapped UTF-8 run.
    ///
    /// # Errors
    ///
    /// Returns a structured error for invalid values, unsupported forced base
    /// direction or multiline input, resource exhaustion, missing fonts, or an
    /// upstream invariant violation. No partial result is returned.
    pub fn shape(&mut self, request: &TextRequest) -> Result<Arc<ShapedText>, TextError> {
        let validated = validate_request(request, self.limits)?;
        let key = RequestKey::new(request)?;
        if let Some(index) = self.cache.iter().position(|entry| entry.key == key) {
            let entry = self
                .cache
                .remove(index)
                .ok_or(TextError::BackendInvariant {
                    detail: "cache index disappeared",
                })?;
            let shaped = Arc::clone(&entry.shaped);
            self.cache.push_back(entry);
            self.cache_hits = self.cache_hits.saturating_add(1);
            return Ok(shaped);
        }

        self.cache_misses = self.cache_misses.saturating_add(1);
        let shaped = Arc::new(self.shape_uncached(request, validated)?);
        self.insert_cache(key, Arc::clone(&shaped));
        Ok(shaped)
    }

    /// Explicitly releases this system's cache and font-selection contexts.
    /// Results retained by callers intentionally continue to own their blobs.
    #[must_use]
    pub fn shutdown(self) -> TextShutdownReport {
        TextShutdownReport {
            cached_shapes_released: self.cache.len(),
            accounted_cache_bytes_released: self.cache_bytes,
        }
    }

    fn shape_uncached(
        &mut self,
        request: &TextRequest,
        validated: ValidatedRequest,
    ) -> Result<ShapedText, TextError> {
        let family_names = parley_families(request.families())?;
        let mut features = Vec::new();
        features
            .try_reserve_exact(request.features().len())
            .map_err(|_| allocation(TextResource::FeatureSettings, request.features().len()))?;
        features.extend(request.features().iter().map(|feature| {
            ParleyFontFeature::new(ParleyTag::from_bytes(feature.tag()), feature.value())
        }));
        let mut variations = Vec::new();
        variations
            .try_reserve_exact(request.variations().len())
            .map_err(|_| allocation(TextResource::VariationSettings, request.variations().len()))?;
        variations.extend(request.variations().iter().map(|variation| {
            ParleyFontVariation::new(ParleyTag::from_bytes(variation.tag()), variation.value())
        }));

        let mut builder =
            self.layout_context
                .ranged_builder(&mut self.font_context, request.text(), 1.0, false);
        builder.push_default(StyleProperty::FontFamily(ParleyFontFamily::List(
            Cow::Borrowed(&family_names),
        )));
        builder.push_default(StyleProperty::FontSize(request.font_size_px()));
        builder.push_default(StyleProperty::FontWeight(ParleyFontWeight::new(
            request.weight().value(),
        )));
        builder.push_default(StyleProperty::FontWidth(ParleyFontWidth::from_ratio(
            request.stretch().ratio(),
        )));
        builder.push_default(StyleProperty::FontStyle(to_parley_style(request.style())));
        builder.push_default(StyleProperty::LineHeight(to_parley_line_height(
            request.line_height(),
        )));
        builder.push_default(StyleProperty::LetterSpacing(request.letter_spacing_px()));
        builder.push_default(StyleProperty::WordSpacing(request.word_spacing_px()));
        builder.push_default(StyleProperty::FontFeatures(ParleyFontFeatures::List(
            Cow::Borrowed(&features),
        )));
        builder.push_default(StyleProperty::FontVariations(ParleyFontVariations::List(
            Cow::Borrowed(&variations),
        )));
        if let Some(language) = validated.language {
            builder.push_default(StyleProperty::Locale(Some(language)));
        }

        let mut layout = builder.build(request.text());
        layout.break_all_lines(None);
        layout.align(Alignment::Start, AlignmentOptions::default());
        if layout.len() > 1 {
            return Err(TextError::BackendInvariant {
                detail: "single-run input produced multiple lines",
            });
        }

        let base_direction = if layout.is_rtl() {
            RunDirection::RightToLeft
        } else {
            RunDirection::LeftToRight
        };
        let (metrics, runs) = extract_layout(request.text(), &layout, self.limits)?;
        if !request.text().is_empty()
            && runs.is_empty()
            && request
                .text()
                .chars()
                .any(|character| !is_invisible_control(character))
        {
            return Err(TextError::NoUsableFont);
        }

        Ok(ShapedText::new(
            Arc::from(request.text()),
            COLLECTION_REVISION,
            base_direction,
            request.line_height(),
            metrics,
            runs.into_boxed_slice(),
        ))
    }

    fn insert_cache(&mut self, key: RequestKey, shaped: Arc<ShapedText>) {
        if self.limits.max_cache_entries == 0 || self.limits.max_cache_bytes == 0 {
            return;
        }
        let accounted_bytes = cache_weight(&key, &shaped);
        if accounted_bytes > self.limits.max_cache_bytes {
            return;
        }
        while self.cache.len() >= self.limits.max_cache_entries
            || self.cache_bytes.saturating_add(accounted_bytes) > self.limits.max_cache_bytes
        {
            let Some(evicted) = self.cache.pop_front() else {
                break;
            };
            self.cache_bytes = self.cache_bytes.saturating_sub(evicted.accounted_bytes);
        }
        if self.cache.try_reserve(1).is_err() {
            return;
        }
        self.cache_bytes = self.cache_bytes.saturating_add(accounted_bytes);
        self.cache.push_back(CacheEntry {
            key,
            shaped,
            accounted_bytes,
        });
    }
}

#[derive(Clone, Copy)]
struct ValidatedRequest {
    language: Option<ParleyLanguage>,
}

fn validate_request(
    request: &TextRequest,
    limits: TextLimits,
) -> Result<ValidatedRequest, TextError> {
    validate_request_resources(request, limits)?;
    validate_request_scalars(request, limits)?;
    validate_request_capabilities(request, limits)?;

    let language = if let Some(language) = request.language() {
        match ParleyLanguage::parse(language) {
            Ok(parsed) => Some(parsed),
            Err(_) => {
                return Err(TextError::InvalidLanguageTag {
                    language: try_clone_str(TextResource::LanguageBytes, language)?,
                });
            }
        }
    } else {
        None
    };
    Ok(ValidatedRequest { language })
}

fn validate_request_resources(request: &TextRequest, limits: TextLimits) -> Result<(), TextError> {
    enforce_limit(
        TextResource::TextBytes,
        request.text().len(),
        limits.max_text_bytes,
    )?;
    enforce_limit(
        TextResource::FontFamilies,
        request.families().len(),
        limits.max_families,
    )?;
    enforce_limit(
        TextResource::FeatureSettings,
        request.features().len(),
        limits.max_features,
    )?;
    enforce_limit(
        TextResource::VariationSettings,
        request.variations().len(),
        limits.max_variations,
    )?;
    if let Some(language) = request.language() {
        enforce_limit(
            TextResource::LanguageBytes,
            language.len(),
            limits.max_language_bytes,
        )?;
    }

    let family_bytes = request.families().iter().try_fold(
        0_usize,
        |total, family| -> Result<usize, TextError> {
            let bytes = match family {
                FontFamily::Named(name) => {
                    if name.trim().is_empty() {
                        return Err(TextError::EmptyFontFamily);
                    }
                    name.len()
                }
                FontFamily::Generic(_) => 0,
            };
            total
                .checked_add(bytes)
                .ok_or(TextError::ResourceLimitExceeded {
                    resource: TextResource::FamilyNameBytes,
                    observed: usize::MAX,
                    limit: limits.max_family_name_bytes,
                })
        },
    )?;
    enforce_limit(
        TextResource::FamilyNameBytes,
        family_bytes,
        limits.max_family_name_bytes,
    )
}

fn validate_request_scalars(request: &TextRequest, limits: TextLimits) -> Result<(), TextError> {
    validate_positive_bounded(
        request.font_size_px(),
        f64::from(limits.max_font_size_px),
        InvalidTextField::FontSize,
    )?;
    validate_range(
        request.weight().value(),
        1.0,
        1_000.0,
        InvalidTextField::FontWeight,
    )?;
    validate_range(
        request.stretch().ratio(),
        0.01,
        10.0,
        InvalidTextField::FontStretch,
    )?;
    if let FontStyle::Oblique { degrees } = request.style() {
        validate_range(degrees, -90.0, 90.0, InvalidTextField::ObliqueAngle)?;
    }
    if let LineHeight::Used { px, .. } = request.line_height() {
        validate_positive_bounded(
            px,
            f64::from(limits.max_abs_coordinate_px),
            InvalidTextField::LineHeight,
        )?;
    }
    validate_signed_bounded(
        request.letter_spacing_px(),
        f64::from(limits.max_abs_coordinate_px),
        InvalidTextField::LetterSpacing,
    )?;
    validate_signed_bounded(
        request.word_spacing_px(),
        f64::from(limits.max_abs_coordinate_px),
        InvalidTextField::WordSpacing,
    )
}

fn validate_request_capabilities(
    request: &TextRequest,
    limits: TextLimits,
) -> Result<(), TextError> {
    if request.direction() != TextDirection::Auto {
        return Err(TextError::UnsupportedDirection {
            direction: request.direction(),
        });
    }
    if request.text().chars().any(is_line_separator) {
        return Err(TextError::UnsupportedMultilineText);
    }
    for feature in request.features() {
        validate_tag(feature.tag())?;
    }
    for variation in request.variations() {
        validate_tag(variation.tag())?;
        validate_signed_bounded(
            variation.value(),
            f64::from(limits.max_abs_coordinate_px),
            InvalidTextField::VariationValue,
        )?;
    }
    Ok(())
}

fn parley_families(families: &[FontFamily]) -> Result<Vec<ParleyFontFamilyName<'_>>, TextError> {
    let requested = families.len().saturating_add(1);
    let mut result = Vec::new();
    result
        .try_reserve_exact(requested)
        .map_err(|_| allocation(TextResource::FontFamilies, requested))?;
    result.extend(families.iter().map(|family| match family {
        FontFamily::Named(name) => ParleyFontFamilyName::Named(Cow::Borrowed(name.as_str())),
        FontFamily::Generic(generic) => ParleyFontFamilyName::Generic(to_parley_generic(*generic)),
    }));
    let includes_embedded = families.iter().any(
        |family| matches!(family, FontFamily::Named(name) if name.eq_ignore_ascii_case(EMBEDDED_FAMILY)),
    );
    if !includes_embedded {
        result.push(ParleyFontFamilyName::Named(Cow::Borrowed(EMBEDDED_FAMILY)));
    }
    Ok(result)
}

const fn to_parley_generic(value: GenericFamily) -> ParleyGenericFamily {
    match value {
        GenericFamily::Serif => ParleyGenericFamily::Serif,
        GenericFamily::SansSerif => ParleyGenericFamily::SansSerif,
        GenericFamily::Monospace => ParleyGenericFamily::Monospace,
        GenericFamily::Cursive => ParleyGenericFamily::Cursive,
        GenericFamily::Fantasy => ParleyGenericFamily::Fantasy,
        GenericFamily::SystemUi => ParleyGenericFamily::SystemUi,
        GenericFamily::UiSerif => ParleyGenericFamily::UiSerif,
        GenericFamily::UiSansSerif => ParleyGenericFamily::UiSansSerif,
        GenericFamily::UiMonospace => ParleyGenericFamily::UiMonospace,
        GenericFamily::UiRounded => ParleyGenericFamily::UiRounded,
        GenericFamily::Emoji => ParleyGenericFamily::Emoji,
        GenericFamily::Math => ParleyGenericFamily::Math,
        GenericFamily::FangSong => ParleyGenericFamily::FangSong,
    }
}

const fn to_parley_style(value: FontStyle) -> ParleyFontStyle {
    match value {
        FontStyle::Normal => ParleyFontStyle::Normal,
        FontStyle::Italic => ParleyFontStyle::Italic,
        FontStyle::Oblique { degrees } => ParleyFontStyle::Oblique(Some(degrees)),
    }
}

const fn to_parley_line_height(value: LineHeight) -> ParleyLineHeight {
    match value {
        LineHeight::Normal => ParleyLineHeight::MetricsRelative(1.0),
        LineHeight::Used { px, .. } => ParleyLineHeight::Absolute(px),
    }
}

fn extract_layout(
    text: &str,
    layout: &parley::Layout<()>,
    limits: TextLimits,
) -> Result<(TextMetrics, Vec<ShapedRun>), TextError> {
    let Some(line) = layout.get(0) else {
        let metrics = TextMetrics::new(
            layout.width(),
            layout.full_width(),
            layout.height(),
            0.0,
            0.0,
            0.0,
            0.0,
            0.0,
        );
        validate_text_metrics(metrics, limits)?;
        return Ok((metrics, Vec::new()));
    };
    let line_metrics = line.metrics();
    let metrics = TextMetrics::new(
        layout.width(),
        layout.full_width(),
        layout.height(),
        line_metrics.baseline,
        line_metrics.ascent,
        line_metrics.descent,
        line_metrics.leading,
        line_metrics.line_height,
    );
    validate_text_metrics(metrics, limits)?;

    let mut runs = Vec::new();
    let run_capacity = line.items().count().min(limits.max_runs);
    runs.try_reserve_exact(run_capacity)
        .map_err(|_| allocation(TextResource::Runs, run_capacity))?;
    let mut totals = ExtractionTotals::default();

    for item in line.items() {
        let PositionedLayoutItem::GlyphRun(glyph_run) = item else {
            return Err(TextError::BackendInvariant {
                detail: "text-only request produced an inline box",
            });
        };
        enforce_increment(TextResource::Runs, runs.len(), 1, limits.max_runs)?;
        runs.push(extract_run(text, &glyph_run, limits, &mut totals)?);
    }

    Ok((metrics, runs))
}

#[derive(Default)]
struct ExtractionTotals {
    clusters: usize,
    glyphs: usize,
    faces: Vec<FontFace>,
    font_bytes: usize,
}

fn extract_run(
    text: &str,
    glyph_run: &parley::GlyphRun<'_, ()>,
    limits: TextLimits,
    totals: &mut ExtractionTotals,
) -> Result<ShapedRun, TextError> {
    let run = glyph_run.run();
    enforce_limit(
        TextResource::NormalizedCoordinates,
        run.normalized_coords().len(),
        limits.max_normalized_coordinates,
    )?;
    validate_output_value(glyph_run.offset(), limits)?;
    validate_output_value(glyph_run.baseline(), limits)?;
    validate_output_value(glyph_run.advance(), limits)?;
    let face = validate_face(run.font(), limits, totals)?;
    let (glyphs, clusters) = extract_clusters(
        text,
        run,
        glyph_run.baseline(),
        glyph_run.offset(),
        limits,
        totals,
    )?;
    if glyph_run.glyphs().count() != glyphs.len() {
        return Err(TextError::BackendInvariant {
            detail: "a positioned glyph run covered only part of its shaping run",
        });
    }
    let run_metrics = copy_run_metrics(run.metrics());
    validate_run_metrics(run_metrics, limits)?;
    let text_range = validate_run_range(text, run.text_range())?;
    let script = script_for_range(text, text_range.clone());
    let synthesis = run.synthesis();
    let mut normalized_coordinates = Vec::new();
    normalized_coordinates
        .try_reserve_exact(run.normalized_coords().len())
        .map_err(|_| {
            allocation(
                TextResource::NormalizedCoordinates,
                run.normalized_coords().len(),
            )
        })?;
    normalized_coordinates.extend_from_slice(run.normalized_coords());

    Ok(ShapedRun::new(
        face,
        run.font_size(),
        normalized_coordinates.into_boxed_slice(),
        FontSynthesis::new(synthesis.embolden(), synthesis.skew()),
        if run.is_rtl() {
            RunDirection::RightToLeft
        } else {
            RunDirection::LeftToRight
        },
        script,
        text_range,
        glyph_run.advance(),
        run_metrics,
        glyphs.into_boxed_slice(),
        clusters.into_boxed_slice(),
    ))
}

fn validate_face(
    font: &parley::FontData,
    limits: TextLimits,
    totals: &mut ExtractionTotals,
) -> Result<FontFace, TextError> {
    let face = FontFace::new(font.data.clone(), font.index);
    enforce_limit(
        TextResource::FontBytes,
        face.bytes().len(),
        limits.max_font_bytes,
    )?;
    match totals.faces.iter().find(|known| known.id() == face.id()) {
        Some(known) if !known.exactly_matches(&face) => Err(TextError::BackendInvariant {
            detail: "font blob identity referred to different bytes or face index",
        }),
        Some(_) => Ok(face),
        None => {
            enforce_increment(TextResource::Fonts, totals.faces.len(), 1, limits.max_fonts)?;
            totals.font_bytes = checked_resource_add(
                TextResource::TotalFontBytes,
                totals.font_bytes,
                face.bytes().len(),
                limits.max_total_font_bytes,
            )?;
            totals
                .faces
                .try_reserve(1)
                .map_err(|_| allocation(TextResource::Fonts, 1))?;
            totals.faces.push(face.clone());
            Ok(face)
        }
    }
}

fn extract_clusters(
    text: &str,
    run: &parley::Run<'_, ()>,
    baseline: f32,
    run_origin: f32,
    limits: TextLimits,
    totals: &mut ExtractionTotals,
) -> Result<(Vec<PositionedGlyph>, Vec<GlyphCluster>), TextError> {
    let cluster_iterator = run.visual_clusters();
    let cluster_count = cluster_iterator.clone().count();
    let glyph_count = cluster_iterator
        .clone()
        .try_fold(0_usize, |count, cluster| {
            checked_resource_add(
                TextResource::Glyphs,
                count,
                cluster.glyphs().count(),
                limits.max_glyphs,
            )
        })?;
    let total_clusters = checked_resource_add(
        TextResource::Clusters,
        totals.clusters,
        cluster_count,
        limits.max_clusters,
    )?;
    let total_glyphs = checked_resource_add(
        TextResource::Glyphs,
        totals.glyphs,
        glyph_count,
        limits.max_glyphs,
    )?;
    let mut glyphs = Vec::new();
    glyphs
        .try_reserve_exact(glyph_count)
        .map_err(|_| allocation(TextResource::Glyphs, glyph_count))?;
    let mut clusters = Vec::new();
    clusters
        .try_reserve_exact(cluster_count)
        .map_err(|_| allocation(TextResource::Clusters, cluster_count))?;
    let mut glyph_pen = run_origin;
    let mut cluster_pen = run_origin;
    for cluster in cluster_iterator {
        let cluster_index =
            u32::try_from(clusters.len()).map_err(|_| TextError::ResourceLimitExceeded {
                resource: TextResource::Clusters,
                observed: clusters.len(),
                limit: u32::MAX as usize,
            })?;
        let text_range = validate_cluster_range(text, cluster.text_range())?;
        let glyph_start = glyphs.len();
        let cluster_x = cluster_pen;
        for glyph in cluster.glyphs() {
            let x = glyph_pen + glyph.x;
            let y = baseline + glyph.y;
            validate_output_value(x, limits)?;
            validate_output_value(y, limits)?;
            validate_output_value(glyph.advance, limits)?;
            glyphs.push(PositionedGlyph::new(
                glyph.id,
                x,
                y,
                glyph.advance,
                cluster_index,
            ));
            glyph_pen += glyph.advance;
            validate_output_value(glyph_pen, limits)?;
        }
        validate_output_value(cluster_x, limits)?;
        validate_output_value(cluster.advance(), limits)?;
        clusters.push(GlyphCluster::new(
            text_range,
            glyph_start..glyphs.len(),
            cluster_x,
            cluster.advance(),
            cluster.is_ligature_start(),
            cluster.is_ligature_continuation(),
        ));
        cluster_pen += cluster.advance();
        validate_output_value(cluster_pen, limits)?;
    }
    totals.clusters = total_clusters;
    totals.glyphs = total_glyphs;
    Ok((glyphs, clusters))
}

fn validate_run_range(
    text: &str,
    range: std::ops::Range<usize>,
) -> Result<std::ops::Range<usize>, TextError> {
    if range.start <= range.end
        && range.end <= text.len()
        && text.is_char_boundary(range.start)
        && text.is_char_boundary(range.end)
    {
        Ok(range)
    } else {
        Err(TextError::BackendInvariant {
            detail: "shaper returned a run outside UTF-8 text boundaries",
        })
    }
}

fn validate_cluster_range(
    text: &str,
    range: std::ops::Range<usize>,
) -> Result<std::ops::Range<usize>, TextError> {
    if range.start <= range.end
        && range.end <= text.len()
        && text.is_char_boundary(range.start)
        && text.is_char_boundary(range.end)
    {
        Ok(range)
    } else {
        Err(TextError::InvalidValue {
            field: InvalidTextField::ClusterRange,
        })
    }
}

const fn copy_run_metrics(source: &parley::RunMetrics) -> RunMetrics {
    RunMetrics {
        ascent: source.ascent,
        descent: source.descent,
        leading: source.leading,
        underline_offset: source.underline_offset,
        underline_size: source.underline_size,
        strikethrough_offset: source.strikethrough_offset,
        strikethrough_size: source.strikethrough_size,
        line_height: source.line_height,
        x_height: source.x_height,
        cap_height: source.cap_height,
    }
}

fn script_for_range(text: &str, range: std::ops::Range<usize>) -> ScriptTag {
    let scripts = CodePointMapData::<Script>::new();
    let names = PropertyNamesShort::<Script>::new();
    let mut first = Script::Unknown;
    for character in text[range].chars() {
        let script = scripts.get(character);
        if first == Script::Unknown {
            first = script;
        }
        if script != Script::Common && script != Script::Inherited && script != Script::Unknown {
            return script_tag(names.get(script));
        }
    }
    script_tag(names.get(first))
}

fn script_tag(name: Option<&str>) -> ScriptTag {
    let bytes = name.unwrap_or("Zzzz").as_bytes();
    if let Ok(tag) = <[u8; 4]>::try_from(bytes) {
        ScriptTag::from_bytes(tag)
    } else {
        ScriptTag::UNKNOWN
    }
}

fn validate_text_metrics(metrics: TextMetrics, limits: TextLimits) -> Result<(), TextError> {
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
        validate_metric_value(value, limits)?;
    }
    Ok(())
}

fn validate_run_metrics(metrics: RunMetrics, limits: TextLimits) -> Result<(), TextError> {
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
        validate_metric_value(value, limits)?;
    }
    for value in [metrics.x_height, metrics.cap_height].into_iter().flatten() {
        validate_metric_value(value, limits)?;
    }
    Ok(())
}

fn validate_metric_value(value: f32, limits: TextLimits) -> Result<(), TextError> {
    if value.is_finite() && f64::from(value.abs()) <= f64::from(limits.max_abs_coordinate_px) {
        Ok(())
    } else {
        Err(TextError::InvalidValue {
            field: InvalidTextField::OutputMetric,
        })
    }
}

fn validate_output_value(value: f32, limits: TextLimits) -> Result<(), TextError> {
    if value.is_finite() && f64::from(value.abs()) <= f64::from(limits.max_abs_coordinate_px) {
        Ok(())
    } else {
        Err(TextError::InvalidValue {
            field: InvalidTextField::OutputCoordinate,
        })
    }
}

fn validate_positive_bounded(
    value: f32,
    maximum: f64,
    field: InvalidTextField,
) -> Result<(), TextError> {
    if value.is_finite() && value > 0.0 && f64::from(value) <= maximum {
        Ok(())
    } else {
        Err(TextError::InvalidValue { field })
    }
}

fn validate_signed_bounded(
    value: f32,
    maximum: f64,
    field: InvalidTextField,
) -> Result<(), TextError> {
    if value.is_finite() && f64::from(value.abs()) <= maximum {
        Ok(())
    } else {
        Err(TextError::InvalidValue { field })
    }
}

fn validate_range(
    value: f32,
    minimum: f32,
    maximum: f32,
    field: InvalidTextField,
) -> Result<(), TextError> {
    if value.is_finite() && value >= minimum && value <= maximum {
        Ok(())
    } else {
        Err(TextError::InvalidValue { field })
    }
}

fn validate_tag(tag: [u8; 4]) -> Result<(), TextError> {
    if tag
        .iter()
        .all(|byte| byte.is_ascii_graphic() || *byte == b' ')
    {
        Ok(())
    } else {
        Err(TextError::InvalidValue {
            field: InvalidTextField::OpenTypeTag,
        })
    }
}

const fn is_line_separator(character: char) -> bool {
    matches!(
        character,
        '\n' | '\r' | '\u{0085}' | '\u{2028}' | '\u{2029}'
    )
}

fn is_invisible_control(character: char) -> bool {
    character.is_control()
        || matches!(
            character,
            '\u{061C}'
                | '\u{200B}'..='\u{200F}'
                | '\u{202A}'..='\u{202E}'
                | '\u{2060}'..='\u{206F}'
                | '\u{FEFF}'
        )
}

fn enforce_increment(
    resource: TextResource,
    current: usize,
    additional: usize,
    limit: usize,
) -> Result<(), TextError> {
    checked_resource_add(resource, current, additional, limit).map(|_| ())
}

fn checked_resource_add(
    resource: TextResource,
    current: usize,
    additional: usize,
    limit: usize,
) -> Result<usize, TextError> {
    let observed = current
        .checked_add(additional)
        .ok_or(TextError::ResourceLimitExceeded {
            resource,
            observed: usize::MAX,
            limit,
        })?;
    enforce_limit(resource, observed, limit)?;
    Ok(observed)
}

fn enforce_limit(resource: TextResource, observed: usize, limit: usize) -> Result<(), TextError> {
    if observed <= limit {
        Ok(())
    } else {
        Err(TextError::ResourceLimitExceeded {
            resource,
            observed,
            limit,
        })
    }
}

fn cache_weight(key: &RequestKey, shaped: &ShapedText) -> usize {
    // Cache accounting is deliberately conservative. A cached shape can keep a
    // selected font blob alive after Fontique prunes its source cache, so every
    // unique blob's complete bytes are charged even when another system owner
    // currently shares that allocation.
    let mut bytes = size_of::<CacheEntry>()
        .saturating_mul(2)
        .saturating_add(key.owned_heap_bytes())
        .saturating_add(size_of::<ShapedText>())
        .saturating_add(ARC_ALLOCATION_OVERHEAD)
        .saturating_add(owned_buffer_bytes(shaped.text().len()))
        .saturating_add(owned_vec_bytes::<ShapedRun>(shaped.runs().len()));
    for (index, run) in shaped.runs().iter().enumerate() {
        bytes = bytes
            .saturating_add(owned_vec_bytes::<i16>(
                run.normalized_variation_coordinates().len(),
            ))
            .saturating_add(owned_vec_bytes::<PositionedGlyph>(run.glyphs().len()))
            .saturating_add(owned_vec_bytes::<GlyphCluster>(run.clusters().len()));
        if !shaped.runs()[..index]
            .iter()
            .any(|known| known.face().id().blob_id() == run.face().id().blob_id())
        {
            bytes = bytes
                .saturating_add(ARC_ALLOCATION_OVERHEAD)
                .saturating_add(run.face().bytes().len());
        }
    }
    bytes
}

const ALLOCATION_OVERHEAD: usize = size_of::<usize>() * 2;
const ARC_ALLOCATION_OVERHEAD: usize = size_of::<usize>() * 3;

const fn owned_buffer_bytes(capacity: usize) -> usize {
    if capacity == 0 {
        0
    } else {
        capacity.saturating_add(ALLOCATION_OVERHEAD)
    }
}

const fn owned_vec_bytes<T>(capacity: usize) -> usize {
    if capacity == 0 {
        0
    } else {
        capacity
            .saturating_mul(size_of::<T>())
            .saturating_add(ALLOCATION_OVERHEAD)
    }
}

fn try_clone_str(resource: TextResource, value: &str) -> Result<String, TextError> {
    let mut cloned = String::new();
    cloned
        .try_reserve_exact(value.len())
        .map_err(|_| allocation(resource, value.len()))?;
    cloned.push_str(value);
    Ok(cloned)
}

const fn allocation(resource: TextResource, requested: usize) -> TextError {
    TextError::AllocationFailed {
        resource,
        requested,
    }
}

#[cfg(test)]
mod tests {
    use super::{FontSourcePolicy, TextSystem};
    use crate::{TextError, TextLimits, TextResource};

    #[test]
    fn malformed_fallback_font_is_rejected_without_entering_shaping() {
        let error = TextSystem::new_with_font(
            FontSourcePolicy::EmbeddedOnly,
            TextLimits::default(),
            b"not an OpenType font",
            "missing",
        )
        .err()
        .expect("malformed font must fail");
        assert_eq!(error, TextError::EmbeddedFontRejected);
    }

    #[test]
    fn embedded_font_is_checked_against_limits_before_registration() {
        let error = TextSystem::new_deterministic(TextLimits::default().with_max_font_bytes(16))
            .err()
            .expect("small font limit must fail");
        assert!(matches!(
            error,
            TextError::ResourceLimitExceeded {
                resource: TextResource::FontBytes,
                observed,
                limit: 16,
            } if observed > 16
        ));
    }
}
