/// Resource limits for one text system and one shaped request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(clippy::struct_field_names)]
pub struct TextLimits {
    pub(crate) max_text_bytes: usize,
    pub(crate) max_families: usize,
    pub(crate) max_family_name_bytes: usize,
    pub(crate) max_language_bytes: usize,
    pub(crate) max_features: usize,
    pub(crate) max_variations: usize,
    pub(crate) max_runs: usize,
    pub(crate) max_clusters: usize,
    pub(crate) max_glyphs: usize,
    pub(crate) max_fonts: usize,
    pub(crate) max_font_bytes: usize,
    pub(crate) max_total_font_bytes: usize,
    pub(crate) max_normalized_coordinates: usize,
    pub(crate) max_cache_entries: usize,
    pub(crate) max_cache_bytes: usize,
    pub(crate) max_font_size_px: u32,
    pub(crate) max_abs_coordinate_px: u32,
}

impl Default for TextLimits {
    fn default() -> Self {
        Self {
            max_text_bytes: 1 << 20,
            max_families: 32,
            max_family_name_bytes: 8 << 10,
            max_language_bytes: 256,
            max_features: 128,
            max_variations: 64,
            max_runs: 65_536,
            max_clusters: 1_000_000,
            max_glyphs: 1_000_000,
            max_fonts: 64,
            max_font_bytes: 64 << 20,
            max_total_font_bytes: 128 << 20,
            max_normalized_coordinates: 256,
            max_cache_entries: 128,
            max_cache_bytes: 64 << 20,
            max_font_size_px: 4_096,
            max_abs_coordinate_px: 1_000_000,
        }
    }
}

macro_rules! limit_accessors {
    ($(($getter:ident, $setter:ident, $field:ident)),* $(,)?) => {
        impl TextLimits {
            $(
                #[must_use]
                pub const fn $getter(self) -> usize {
                    self.$field
                }

                #[must_use]
                pub const fn $setter(mut self, limit: usize) -> Self {
                    self.$field = limit;
                    self
                }
            )*
        }
    };
}

limit_accessors!(
    (max_text_bytes, with_max_text_bytes, max_text_bytes),
    (max_families, with_max_families, max_families),
    (
        max_family_name_bytes,
        with_max_family_name_bytes,
        max_family_name_bytes
    ),
    (
        max_language_bytes,
        with_max_language_bytes,
        max_language_bytes
    ),
    (max_features, with_max_features, max_features),
    (max_variations, with_max_variations, max_variations),
    (max_runs, with_max_runs, max_runs),
    (max_clusters, with_max_clusters, max_clusters),
    (max_glyphs, with_max_glyphs, max_glyphs),
    (max_fonts, with_max_fonts, max_fonts),
    (max_font_bytes, with_max_font_bytes, max_font_bytes),
    (
        max_total_font_bytes,
        with_max_total_font_bytes,
        max_total_font_bytes
    ),
    (
        max_normalized_coordinates,
        with_max_normalized_coordinates,
        max_normalized_coordinates
    ),
    (max_cache_entries, with_max_cache_entries, max_cache_entries),
    (max_cache_bytes, with_max_cache_bytes, max_cache_bytes),
);

impl TextLimits {
    #[must_use]
    pub const fn max_font_size_px(self) -> u32 {
        self.max_font_size_px
    }

    #[must_use]
    pub const fn with_max_font_size_px(mut self, limit: u32) -> Self {
        self.max_font_size_px = limit;
        self
    }

    #[must_use]
    pub const fn max_abs_coordinate_px(self) -> u32 {
        self.max_abs_coordinate_px
    }

    #[must_use]
    pub const fn with_max_abs_coordinate_px(mut self, limit: u32) -> Self {
        self.max_abs_coordinate_px = limit;
        self
    }
}
