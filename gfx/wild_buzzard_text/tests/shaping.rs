use std::sync::Arc;

use wild_buzzard_text::{
    FontFamily, FontFeature, FontSourcePolicy, FontStyle, FontVariation, GenericFamily,
    InvalidTextField, LineHeight, LineHeightProvenance, RunDirection, ScriptTag, TextDirection,
    TextError, TextLimits, TextRequest, TextResource, TextSystem,
};

fn deterministic_system() -> TextSystem {
    TextSystem::new_deterministic(TextLimits::default())
        .expect("the pinned Fira Code font must initialize")
}

#[test]
fn latin_text_has_real_fonts_glyphs_clusters_and_metrics() {
    let mut system = deterministic_system();
    let shaped = system
        .shape(&TextRequest::new("Wild Buzzard", 18.0))
        .expect("bounded Latin text must shape");

    assert_eq!(shaped.text(), "Wild Buzzard");
    assert_eq!(shaped.collection_revision(), system.collection_revision());
    assert_eq!(shaped.base_direction(), RunDirection::LeftToRight);
    assert!(!shaped.runs().is_empty());
    assert!(shaped.glyph_count() > 0);
    assert!(shaped.cluster_count() > 0);
    assert!(shaped.metrics().width() > 0.0);
    assert!(shaped.metrics().height() > 0.0);
    assert!(shaped.metrics().first_baseline() > 0.0);

    for run in shaped.runs() {
        assert!(!run.face().bytes().is_empty());
        assert_eq!(run.face().collection_index(), 0);
        assert_eq!(run.direction(), RunDirection::LeftToRight);
        assert_eq!(run.script(), ScriptTag::from_bytes(*b"Latn"));
        assert!(run.font_size_px() > 0.0);
        for glyph in run.glyphs() {
            assert!(glyph.id() > 0, "trusted Latin must not use .notdef");
            assert!(glyph.x().is_finite());
            assert!(glyph.y().is_finite());
            assert!(glyph.advance().is_finite());
            assert!((glyph.cluster_index() as usize) < run.clusters().len());
        }
        for cluster in run.clusters() {
            let range = cluster.text_range();
            assert!(shaped.text().is_char_boundary(range.start));
            assert!(shaped.text().is_char_boundary(range.end));
            assert!(range.start <= range.end);
            assert!(range.end <= shaped.text().len());
        }
    }
}

#[test]
fn fira_code_contextual_ligature_and_combining_clusters_are_preserved() {
    let mut system = deterministic_system();
    let ligature = system
        .shape(&TextRequest::new("->", 20.0).with_features(vec![FontFeature::new(*b"calt", 1)]))
        .expect("Fira Code arrow must shape");
    let clusters: Vec<_> = ligature
        .runs()
        .iter()
        .flat_map(wild_buzzard_text::ShapedRun::clusters)
        .collect();
    assert_eq!(clusters.len(), 2);

    let no_contextual_alternates = system
        .shape(&TextRequest::new("->", 20.0).with_features(vec![FontFeature::new(*b"calt", 0)]))
        .expect("feature-disabled arrow must shape");
    let ligature_ids: Vec<_> = ligature
        .runs()
        .iter()
        .flat_map(wild_buzzard_text::ShapedRun::glyphs)
        .map(|glyph| glyph.id())
        .collect();
    let plain_ids: Vec<_> = no_contextual_alternates
        .runs()
        .iter()
        .flat_map(wild_buzzard_text::ShapedRun::glyphs)
        .map(|glyph| glyph.id())
        .collect();
    assert_ne!(ligature_ids, plain_ids);

    let combining = system
        .shape(&TextRequest::new("e\u{301}", 20.0))
        .expect("combining sequence must shape");
    let combining_clusters: Vec<_> = combining
        .runs()
        .iter()
        .flat_map(wild_buzzard_text::ShapedRun::clusters)
        .collect();
    assert_eq!(combining_clusters.len(), 2);
    assert_eq!(combining_clusters[0].text_range(), 0..1);
    assert_eq!(combining_clusters[1].text_range(), 1.."e\u{301}".len());
    assert!(combining_clusters[0].is_ligature_start());
    assert!(combining_clusters[1].is_ligature_continuation());
    assert!(combining_clusters[0].x() < combining_clusters[1].x());
    assert!(
        combining
            .runs()
            .iter()
            .flat_map(wild_buzzard_text::ShapedRun::glyphs)
            .all(|glyph| glyph.x().is_finite() && glyph.y().is_finite())
    );
}

#[test]
fn unicode_bidi_visual_runs_retain_logical_utf8_ranges() {
    let mut system = deterministic_system();
    let source = "abc \u{202E}DEF\u{202C} ghi";
    let shaped = system
        .shape(&TextRequest::new(source, 18.0))
        .expect("Latin text under Unicode bidi controls must shape");

    assert!(
        shaped
            .runs()
            .iter()
            .any(|run| run.direction() == RunDirection::RightToLeft)
    );
    for cluster in shaped
        .runs()
        .iter()
        .flat_map(wild_buzzard_text::ShapedRun::clusters)
    {
        let range = cluster.text_range();
        assert!(source.is_char_boundary(range.start));
        assert!(source.is_char_boundary(range.end));
    }
}

#[test]
fn named_family_miss_falls_through_to_the_embedded_font() {
    let mut system = deterministic_system();
    let missing = system
        .shape(
            &TextRequest::new("fallback", 16.0)
                .with_families(vec![FontFamily::named("Definitely Not Installed")]),
        )
        .expect("embedded fallback must be explicit in every query");
    let explicit = system
        .shape(
            &TextRequest::new("fallback", 16.0).with_families(vec![FontFamily::named("Fira Code")]),
        )
        .expect("embedded family must be selectable by name");

    assert!(
        missing.runs()[0]
            .face()
            .exactly_matches(explicit.runs()[0].face())
    );
}

#[test]
fn full_request_cache_reuses_only_exact_results_and_is_bounded() {
    let limits = TextLimits::default().with_max_cache_entries(1);
    let mut system = TextSystem::new_deterministic(limits).unwrap();
    let first = system.shape(&TextRequest::new("one", 16.0)).unwrap();
    let first_again = system.shape(&TextRequest::new("one", 16.0)).unwrap();
    assert!(Arc::ptr_eq(&first, &first_again));
    assert_eq!(system.cache_statistics().hits(), 1);
    assert_eq!(system.cache_statistics().misses(), 1);

    let second = system.shape(&TextRequest::new("one", 17.0)).unwrap();
    assert!(!Arc::ptr_eq(&first, &second));
    assert_eq!(system.cache_statistics().entries(), 1);
    assert!(system.cache_statistics().accounted_bytes() > 0);

    let first_after_eviction = system.shape(&TextRequest::new("one", 16.0)).unwrap();
    assert!(!Arc::ptr_eq(&first, &first_after_eviction));
}

#[test]
fn cache_budget_counts_language_settings_result_storage_and_retained_font_blob() {
    let base_request = TextRequest::new("->", 16.0);
    let decorated_request = TextRequest::new("->", 16.0)
        .with_language(Some("en-US".to_owned()))
        .with_features(vec![FontFeature::new(*b"calt", 1)])
        .with_variations(vec![FontVariation::new(*b"wght", 400.0)]);

    let mut base_probe = deterministic_system();
    base_probe.shape(&base_request).unwrap();
    let base_weight = base_probe.cache_statistics().accounted_bytes();
    let mut decorated_probe = deterministic_system();
    decorated_probe.shape(&decorated_request).unwrap();
    let decorated_weight = decorated_probe.cache_statistics().accounted_bytes();
    assert!(decorated_weight > base_weight);
    assert!(
        decorated_weight > 188_500,
        "the retained Fira Code blob is charged"
    );

    let mut tight = TextSystem::new_deterministic(
        TextLimits::default().with_max_cache_bytes(decorated_weight - 1),
    )
    .unwrap();
    tight.shape(&base_request).unwrap();
    assert_eq!(tight.cache_statistics().entries(), 1);
    tight.clear_shape_cache();
    tight.shape(&decorated_request).unwrap();
    assert_eq!(tight.cache_statistics().entries(), 0);
    assert_eq!(tight.cache_statistics().accounted_bytes(), 0);
}

#[test]
fn request_validation_fails_closed_before_shaping() {
    let mut system = deterministic_system();

    let oversized = system
        .shape(&TextRequest::new("too long", 16.0))
        .expect("ordinary request must establish a usable baseline");
    assert!(oversized.glyph_count() > 0);

    let mut restricted =
        TextSystem::new_deterministic(TextLimits::default().with_max_text_bytes(3)).unwrap();
    assert!(matches!(
        restricted.shape(&TextRequest::new("four", 16.0)),
        Err(TextError::ResourceLimitExceeded {
            resource: TextResource::TextBytes,
            observed: 4,
            limit: 3,
        })
    ));
    let mut language_restricted =
        TextSystem::new_deterministic(TextLimits::default().with_max_language_bytes(4)).unwrap();
    assert!(matches!(
        language_restricted
            .shape(&TextRequest::new("lang", 16.0).with_language(Some("en-US".to_owned()))),
        Err(TextError::ResourceLimitExceeded {
            resource: TextResource::LanguageBytes,
            observed: 5,
            limit: 4,
        })
    ));
    assert!(matches!(
        system.shape(&TextRequest::new("bad", f32::NAN)),
        Err(TextError::InvalidValue {
            field: InvalidTextField::FontSize,
        })
    ));
    assert!(matches!(
        system
            .shape(&TextRequest::new("direction", 16.0).with_direction(TextDirection::RightToLeft)),
        Err(TextError::UnsupportedDirection {
            direction: TextDirection::RightToLeft,
        })
    ));
    assert_eq!(
        system.shape(&TextRequest::new("line one\nline two", 16.0)),
        Err(TextError::UnsupportedMultilineText)
    );
    assert!(matches!(
        system.shape(&TextRequest::new("language", 16.0).with_language(Some("x".to_owned()))),
        Err(TextError::InvalidLanguageTag { .. })
    ));
    assert!(matches!(
        system.shape(
            &TextRequest::new("tag", 16.0)
                .with_features(vec![FontFeature::new([0, b'a', b'l', b't'], 1)])
        ),
        Err(TextError::InvalidValue {
            field: InvalidTextField::OpenTypeTag,
        })
    ));
    assert!(matches!(
        system.shape(
            &TextRequest::new("style", 16.0).with_style(FontStyle::Oblique { degrees: 100.0 })
        ),
        Err(TextError::InvalidValue {
            field: InvalidTextField::ObliqueAngle,
        })
    ));
}

#[test]
fn line_height_provenance_and_explicit_shutdown_are_observable() {
    let mut system = deterministic_system();
    let request = TextRequest::new("line metrics", 20.0).with_line_height(LineHeight::Used {
        px: 24.0,
        provenance: LineHeightProvenance::ProvisionalNormal120Percent,
    });
    let shaped = system.shape(&request).unwrap();
    assert_eq!(shaped.line_height_request(), request.line_height());
    assert!((shaped.metrics().line_height() - 24.0).abs() < f32::EPSILON);
    assert_eq!(system.source_policy(), FontSourcePolicy::EmbeddedOnly);

    let report = system.shutdown();
    assert_eq!(report.cached_shapes_released(), 1);
    assert!(report.accounted_cache_bytes_released() > 0);
}

#[test]
fn linux_constructor_keeps_embedded_fallback_independent_of_system_fonts() {
    let mut system = TextSystem::new_linux(TextLimits::default())
        .expect("fontconfig availability must not affect embedded registration");
    assert_eq!(
        system.source_policy(),
        FontSourcePolicy::LinuxSystemWithEmbeddedFallback
    );
    let shaped = system
        .shape(
            &TextRequest::new("portable fallback", 16.0).with_families(vec![
                FontFamily::named("A Family That Cannot Exist"),
                FontFamily::Generic(GenericFamily::Emoji),
            ]),
        )
        .expect("the appended embedded family must remain usable");
    assert!(shaped.glyph_count() > 0);
}
