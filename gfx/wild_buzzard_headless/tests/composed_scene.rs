use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use webrender_api::units::LayoutPoint;
use webrender_api::{FontInstanceKey, GlyphInstance, IdNamespace};
use wild_buzzard_dom::Document;
use wild_buzzard_headless::{
    FrameRequest, FrameSize, FrameStage, HeadlessError, HeadlessLimits, HeadlessRenderer,
    ShapedSceneText, TextRegistryStatistics,
};
use wild_buzzard_html::parse_document;
use wild_buzzard_layout::{
    Au, Color, ComputedStyle, Edges, InitialStyleResolver, StyleInput, StyleResolver, TextMeasurer,
    TextMetrics, Viewport, layout_document,
};
use wild_buzzard_renderer::{
    AppUnitRect, CompileRequest, CompiledScene, GeometryField, PipelineKey, SceneBuildError,
    SceneCompiler, SceneItem, SceneTextDescriptor, SceneTextMetrics,
};
use wild_buzzard_text::{
    LineHeight, LineHeightProvenance, ShapedText, TextLimits, TextRequest, TextSystem,
};

const WIDTH: u32 = 192;
const HEIGHT: u32 = 80;
const PIPELINE: PipelineKey = PipelineKey::new(109, 7);
const BACKGROUND: [u8; 4] = [210, 230, 250, 255];
const TEXT: [u8; 4] = [15, 25, 35, 255];

struct ComposedStyles;

impl StyleResolver for ComposedStyles {
    fn resolve(&self, input: StyleInput<'_>) -> ComputedStyle {
        let root = input.element.html_attribute("data-root").is_some();
        let frame = input.element.html_attribute("data-frame").is_some();
        let mut style = InitialStyleResolver.resolve(input);
        if root {
            style.margin = Edges::default();
        }
        if frame {
            style.margin = Edges::default();
            style.border = Edges::all(Au::from_px(2));
            style.padding = Edges::all(Au::from_px(8));
            style.background_color = color(BACKGROUND);
            style.color = color(TEXT);
            style.font_size = Au::from_px(20);
            // Explicit leading makes first_baseline observably differ from the
            // raw font ascent used by the superseded projection.
            style.line_height = Au::from_px(32);
        }
        style
    }
}

const fn color(channels: [u8; 4]) -> Color {
    Color {
        red: channels[0],
        green: channels[1],
        blue: channels[2],
        alpha: channels[3],
    }
}

struct ExactShapingMeasurer {
    system: Mutex<TextSystem>,
}

impl ExactShapingMeasurer {
    fn new() -> Self {
        Self {
            system: Mutex::new(
                TextSystem::new_deterministic(TextLimits::default())
                    .expect("pinned deterministic font must initialize"),
            ),
        }
    }

    fn shape_request(&self, request: &TextRequest) -> Arc<ShapedText> {
        lock_unpoisoned(&self.system)
            .shape(request)
            .expect("fixture text must shape")
    }

    fn request(text: &str, font_size: i32, line_height: i32) -> TextRequest {
        TextRequest::new(text, app_units_to_px(font_size)).with_line_height(LineHeight::Used {
            px: app_units_to_px(line_height),
            provenance: LineHeightProvenance::Explicit,
        })
    }
}

impl TextMeasurer for ExactShapingMeasurer {
    fn measure(&self, text: &str, style: &ComputedStyle) -> TextMetrics {
        let shaped = self.shape_request(&Self::request(
            text,
            style.font_size.raw(),
            style.line_height.raw(),
        ));
        let metrics = shaped.metrics();
        TextMetrics {
            advance: px_to_app_units(metrics.full_width()),
            // Positioned glyph Y already contains Parley's first baseline.
            // Layout must retain that exact split, not substitute font ascent.
            ascent: px_to_app_units(metrics.first_baseline()),
            descent: px_to_app_units(metrics.height() - metrics.first_baseline()),
        }
    }
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[allow(clippy::cast_precision_loss)]
fn app_units_to_px(value: i32) -> f32 {
    value as f32 / Au::PER_CSS_PX as f32
}

fn px_to_app_units(value: f32) -> Au {
    assert!(value.is_finite() && value >= 0.0);
    let scaled = (f64::from(value) * f64::from(Au::PER_CSS_PX)).round();
    assert!(scaled <= f64::from(i32::MAX));
    #[allow(clippy::cast_possible_truncation)]
    Au::from_raw(scaled as i32)
}

fn fixture() -> (wild_buzzard_layout::LayoutOutput, Vec<ShapedSceneText>) {
    let parsed = parse_document("<html data-root><body data-frame>Wild Buzzard</body></html>")
        .expect("composed fixture must parse");
    let measurer = ExactShapingMeasurer::new();
    let output = layout_document(
        &parsed.document.snapshot().unwrap(),
        Viewport::from_css_pixels(WIDTH.cast_signed(), HEIGHT.cast_signed()),
        &ComposedStyles,
        &measurer,
    )
    .expect("composed fixture must lay out");
    let preliminary = compile(&output);
    assert!(
        preliminary.scene().pending_text().len() >= 2,
        "the composed fixture must exercise multiple canonical text entries"
    );
    let texts = preliminary
        .scene()
        .pending_text()
        .iter()
        .map(|pending| {
            let shaped = measurer.shape_request(&ExactShapingMeasurer::request(
                pending.text(),
                pending.font_size(),
                pending.line_height(),
            ));
            assert_ne!(
                shaped.metrics().first_baseline().to_bits(),
                shaped.metrics().ascent().to_bits()
            );
            ShapedSceneText::new(output.document_version, pending.id().index(), shaped)
        })
        .collect();
    (output, texts)
}

fn compile(output: &wild_buzzard_layout::LayoutOutput) -> wild_buzzard_renderer::CompiledScene {
    SceneCompiler::default()
        .compile(
            output,
            CompileRequest::new(output.document_version, PIPELINE),
        )
        .expect("fixture scene must compile")
}

fn resolve_fixture_scene(
    output: &wild_buzzard_layout::LayoutOutput,
    texts: &[ShapedSceneText],
    renderer_namespace: IdNamespace,
) -> CompiledScene {
    let compiled = compile(output);
    let descriptors = texts
        .iter()
        .map(|text| {
            let metrics = text.shaped().metrics();
            SceneTextDescriptor::new(
                text.document_version(),
                text.pending_index(),
                text.shaped().text(),
                SceneTextMetrics::new(
                    metrics.full_width(),
                    metrics.height(),
                    metrics.first_baseline(),
                    text.font_size_px().unwrap_or(0.0),
                    metrics.line_height(),
                ),
            )
        })
        .collect::<Vec<_>>();
    let map = compiled
        .validate_text_map(&descriptors)
        .expect("real shaped fixture must map exactly");
    let mut resolution = map
        .begin_resolution(renderer_namespace)
        .expect("fixture resolution storage must allocate");
    for text in texts {
        let runs = text
            .shaped()
            .runs()
            .iter()
            .map(|run| {
                let glyphs = run
                    .glyphs()
                    .iter()
                    .map(|glyph| GlyphInstance {
                        index: glyph.id(),
                        point: LayoutPoint::new(glyph.x(), glyph.y()),
                    })
                    .collect::<Vec<_>>();
                (FontInstanceKey(renderer_namespace, 1), glyphs)
            })
            .collect::<Vec<_>>();
        resolution
            .resolve_next(
                text.document_version(),
                text.pending_index(),
                runs.iter().map(|(key, glyphs)| (*key, glyphs.as_slice())),
            )
            .expect("real shaped glyphs must resolve in canonical order");
    }
    compiled
        .compose_text(resolution.finish().expect("every entry was resolved"))
        .expect("resolved fixture must compose")
}

fn renderer() -> HeadlessRenderer {
    HeadlessRenderer::new(
        FrameSize::new(WIDTH, HEIGHT).unwrap(),
        HeadlessLimits::default()
            .with_max_width(WIDTH)
            .with_max_height(HEIGHT)
            .with_max_pixel_bytes(WIDTH as usize * HEIGHT as usize * 4),
    )
    .expect("host must provide a usable Linux EGL pbuffer")
}

fn differing_coordinates(
    left: &wild_buzzard_headless::RgbaFrame,
    right: &wild_buzzard_headless::RgbaFrame,
) -> Vec<(u32, u32)> {
    left.pixels()
        .chunks_exact(4)
        .zip(right.pixels().chunks_exact(4))
        .enumerate()
        .filter(|(_, (left, right))| left != right)
        .map(|(index, _)| {
            let index = u32::try_from(index).unwrap();
            (index % WIDTH, index / WIDTH)
        })
        .collect()
}

fn pixel_intersects_rect(x: u32, y: u32, rect: AppUnitRect) -> bool {
    let pixel_left = i64::from(x) * i64::from(Au::PER_CSS_PX);
    let pixel_top = i64::from(y) * i64::from(Au::PER_CSS_PX);
    let pixel_right = pixel_left + i64::from(Au::PER_CSS_PX);
    let pixel_bottom = pixel_top + i64::from(Au::PER_CSS_PX);
    let rect_left = i64::from(rect.x());
    let rect_top = i64::from(rect.y());
    let rect_right = rect_left + i64::from(rect.width());
    let rect_bottom = rect_top + i64::from(rect.height());
    pixel_right > rect_left
        && pixel_left < rect_right
        && pixel_bottom > rect_top
        && pixel_top < rect_bottom
}

#[test]
fn real_shaped_glyph_y_adds_only_the_fragment_top() {
    let (output, texts) = fixture();
    let pending_scene = compile(&output);
    let pending = &pending_scene.scene().pending_text()[0];
    let fragment_top = app_units_to_px(pending.rect().y());
    let shaped_glyph = texts[0].shaped().runs()[0].glyphs()[0];
    let shaped_metrics = texts[0].shaped().metrics();
    let composed = resolve_fixture_scene(&output, &texts, IdNamespace(701));
    let resolved = composed
        .scene()
        .items()
        .iter()
        .find_map(|item| match item {
            SceneItem::Text(text) if text.pending_text().index() == 0 => Some(text),
            _ => None,
        })
        .expect("the first real shaped entry must resolve");
    let final_y = resolved.glyph_runs()[0].glyphs()[0].y();
    let expected = fragment_top + shaped_glyph.y();
    let ascent_double_added = expected + shaped_metrics.ascent();

    assert_eq!(
        final_y.to_bits(),
        expected.to_bits(),
        "final glyph Y must be fragment top plus Parley's already-baselined glyph Y"
    );
    assert_ne!(
        final_y.to_bits(),
        ascent_double_added.to_bits(),
        "font ascent must not be added to an already-baselined glyph"
    );
}

#[test]
fn decorations_and_all_positioned_text_share_one_deterministic_real_frame() {
    let (output, texts) = fixture();
    let version = output.document_version;
    let pending_rects = compile(&output)
        .scene()
        .pending_text()
        .iter()
        .map(wild_buzzard_renderer::PendingTextRun::rect)
        .collect::<Vec<_>>();
    assert_eq!(pending_rects.len(), texts.len());
    assert!(pending_rects.len() >= 2);
    let mut renderer = renderer();
    let decorations = renderer
        .render(compile(&output), FrameRequest::new(version, 1))
        .expect("decoration reference must render");
    assert_eq!(decorations.pending_text_runs(), texts.len());
    assert_eq!(
        renderer.text_registry_statistics().unwrap(),
        TextRegistryStatistics::default()
    );

    let composed = renderer
        .render_composed(compile(&output), &texts, FrameRequest::new(version, 2))
        .expect("one composed decoration and text transaction must render");
    assert_eq!(composed.pending_text_runs(), 0);
    // The provisional renderer maps border color from currentColor.
    assert_eq!(composed.pixel(1, 1), Some(TEXT));
    assert_eq!(composed.pixel(8, 8), Some(BACKGROUND));
    let differences = differing_coordinates(&decorations, &composed);
    assert!(
        differences.len() > 40,
        "real glyphs must alter the page frame"
    );
    assert!(differences.iter().all(|(_, y)| *y < HEIGHT));
    for (pending_index, rect) in pending_rects.into_iter().enumerate() {
        assert!(
            differences
                .iter()
                .any(|(x, y)| pixel_intersects_rect(*x, *y, rect)),
            "pending text {pending_index} must contribute glyph pixels inside its fragment"
        );
    }
    let stats = renderer.text_registry_statistics().unwrap();
    assert_eq!(stats.font_templates(), 1);
    assert_eq!(stats.font_instances(), 1);

    let repeated = renderer
        .render_composed(compile(&output), &texts, FrameRequest::new(version, 3))
        .expect("the exact composed frame must remain deterministic");
    assert_eq!(composed.pixels(), repeated.pixels());
    assert_eq!(renderer.text_registry_statistics().unwrap(), stats);

    let report = renderer.shutdown().unwrap();
    assert!(report.backend_acknowledged());
    assert!(report.context_released());
    assert_eq!(report.text_font_templates_released(), 1);
    assert_eq!(report.text_font_instances_released(), 1);
}

#[test]
fn mapping_failures_leave_the_transactional_registry_unchanged() {
    let (output, texts) = fixture();
    let version = output.document_version;
    let mut renderer = renderer();

    let missing = renderer
        .render_composed(compile(&output), &[], FrameRequest::new(version, 1))
        .unwrap_err();
    assert!(matches!(
        missing,
        HeadlessError::SceneComposition(SceneBuildError::MissingTextResolution {
            pending_index: 0
        })
    ));

    let duplicate = vec![texts[0].clone(), texts[0].clone()];
    assert!(matches!(
        renderer.render_composed(compile(&output), &duplicate, FrameRequest::new(version, 1)),
        Err(HeadlessError::SceneComposition(
            SceneBuildError::DuplicateTextResolution { pending_index: 0 }
        ))
    ));
    let unknown_index = u32::try_from(texts.len()).unwrap();
    let unknown = vec![ShapedSceneText::new(
        version,
        unknown_index,
        texts[0].shaped().clone(),
    )];
    assert!(matches!(
        renderer.render_composed(compile(&output), &unknown, FrameRequest::new(version, 1)),
        Err(HeadlessError::SceneComposition(
            SceneBuildError::UnknownTextResolution {
                pending_index,
                available
            }
        )) if pending_index == unknown_index && available == texts.len()
    ));
    let other = Document::new();
    let wrong_version = vec![ShapedSceneText::new(
        other.version(),
        0,
        texts[0].shaped().clone(),
    )];
    assert!(matches!(
        renderer.render_composed(
            compile(&output),
            &wrong_version,
            FrameRequest::new(version, 1)
        ),
        Err(HeadlessError::SceneComposition(
            SceneBuildError::DocumentVersionMismatch { expected, actual }
        )) if expected == version && actual == other.version()
    ));

    let mut wrong_size_system =
        TextSystem::new_deterministic(TextLimits::default()).expect("font must initialize");
    let wrong_size = wrong_size_system
        .shape(
            &TextRequest::new(texts[0].shaped().text(), 18.0).with_line_height(LineHeight::Used {
                px: 32.0,
                provenance: LineHeightProvenance::Explicit,
            }),
        )
        .unwrap();
    let mut metric_mismatch = texts.clone();
    metric_mismatch[0] = ShapedSceneText::new(version, 0, wrong_size);
    assert!(matches!(
        renderer.render_composed(
            compile(&output),
            &metric_mismatch,
            FrameRequest::new(version, 1)
        ),
        Err(HeadlessError::SceneComposition(
            SceneBuildError::TextMetricMismatch {
                field: GeometryField::Width,
                ..
            }
        ))
    ));
    let foreign_scene = resolve_fixture_scene(&output, &texts, IdNamespace::DEBUGGER);
    assert_eq!(
        foreign_scene.scene().renderer_namespace(),
        Some(IdNamespace::DEBUGGER)
    );
    assert!(matches!(
        renderer.render(foreign_scene, FrameRequest::new(version, 1)),
        Err(HeadlessError::SceneComposition(
            SceneBuildError::FontInstanceNamespaceMismatch {
                actual: IdNamespace::DEBUGGER,
                ..
            }
        ))
    ));
    assert_eq!(
        renderer.text_registry_statistics().unwrap(),
        TextRegistryStatistics::default(),
        "no mapping failure may commit a staged font or instance"
    );

    let valid = renderer
        .render_composed(compile(&output), &texts, FrameRequest::new(version, 1))
        .expect("the same epoch must remain available after pre-send failures");
    assert_eq!(valid.pending_text_runs(), 0);
    renderer.shutdown().unwrap();
}

#[test]
fn post_send_timeout_keeps_committed_resources_for_teardown_and_poisons_renderer() {
    let (output, texts) = fixture();
    let version = output.document_version;
    let mut renderer = HeadlessRenderer::new(
        FrameSize::new(WIDTH, HEIGHT).unwrap(),
        HeadlessLimits::default()
            .with_max_width(WIDTH)
            .with_max_height(HEIGHT)
            .with_max_pixel_bytes(WIDTH as usize * HEIGHT as usize * 4)
            .with_frame_timeout(Duration::from_nanos(1)),
    )
    .expect("timeout fixture still needs a real Linux EGL pbuffer");

    let failure = renderer
        .render_composed(compile(&output), &texts, FrameRequest::new(version, 1))
        .unwrap_err();
    assert!(matches!(
        failure,
        HeadlessError::FrameTimeout {
            stage: FrameStage::FrameBuilt,
            ..
        }
    ));
    let committed = renderer.text_registry_statistics().unwrap();
    assert_eq!(committed.font_templates(), 1);
    assert_eq!(committed.font_instances(), 1);
    assert!(matches!(
        renderer.render_composed(compile(&output), &texts, FrameRequest::new(version, 2)),
        Err(HeadlessError::RendererUnusable)
    ));

    let report = renderer
        .shutdown()
        .expect("committed resources must remain available for deterministic teardown");
    assert_eq!(report.text_font_templates_released(), 1);
    assert_eq!(report.text_font_instances_released(), 1);
    assert!(report.backend_acknowledged());
    assert!(report.context_released());
}
