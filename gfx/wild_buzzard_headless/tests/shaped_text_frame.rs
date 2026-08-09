use std::collections::BTreeSet;
use std::sync::Arc;

use wild_buzzard_headless::{
    FrameRequest, FrameSize, HeadlessError, HeadlessLimits, HeadlessRenderer, RgbaFrame,
    ShapedTextFrame, TextColor, TextOrigin, TextPipelineKey, TextRegistryStatistics,
};
use wild_buzzard_text::{ShapedText, TextLimits, TextRequest, TextSystem};

const WIDTH: u32 = 160;
const HEIGHT: u32 = 64;
const CLEAR: [u8; 4] = [255, 255, 255, 255];
const PIPELINE: TextPipelineKey = TextPipelineKey::new(83, 4);
const ALTERNATE_PIPELINE: TextPipelineKey = TextPipelineKey::new(83, 5);

fn renderer() -> HeadlessRenderer {
    let size = FrameSize::new(WIDTH, HEIGHT).unwrap();
    let limits = HeadlessLimits::default()
        .with_max_width(WIDTH)
        .with_max_height(HEIGHT)
        .with_max_pixel_bytes(WIDTH as usize * HEIGHT as usize * 4);
    HeadlessRenderer::new(size, limits).expect("host must provide a Linux EGL pbuffer")
}

fn changed_pixels(frame: &wild_buzzard_headless::RgbaFrame) -> usize {
    frame
        .pixels()
        .chunks_exact(4)
        .filter(|pixel| *pixel != CLEAR)
        .count()
}

fn changed_coordinates(frame: &wild_buzzard_headless::RgbaFrame) -> BTreeSet<(u32, u32)> {
    frame
        .pixels()
        .chunks_exact(4)
        .enumerate()
        .filter(|(_, pixel)| *pixel != CLEAR)
        .map(|(index, _)| {
            let index = u32::try_from(index).unwrap();
            (index % WIDTH, index / WIDTH)
        })
        .collect()
}

fn render_initial(
    renderer: &mut HeadlessRenderer,
    shaped_24: &Arc<ShapedText>,
) -> (RgbaFrame, TextRegistryStatistics) {
    let frame_24 =
        ShapedTextFrame::new(1, PIPELINE, shaped_24.clone()).with_origin(TextOrigin::new(8.0, 4.0));
    assert_eq!(
        renderer.text_registry_statistics().unwrap(),
        TextRegistryStatistics::default()
    );
    let invalid_epoch = renderer
        .render_shaped_text(&frame_24, FrameRequest::new(1, u32::MAX))
        .unwrap_err();
    assert!(matches!(
        invalid_epoch,
        HeadlessError::InvalidEpoch { epoch: u32::MAX }
    ));

    let first = renderer
        .render_shaped_text(&frame_24, FrameRequest::new(1, 1))
        .expect("exact shaped glyphs must render");
    assert_eq!(first.pending_text_runs(), 0);
    assert!(changed_pixels(&first) > 20, "glyphs must alter real pixels");
    assert_eq!(first.pixel(0, 0), Some(CLEAR));
    let first_stats = renderer.text_registry_statistics().unwrap();
    assert_eq!(first_stats.font_templates(), 1);
    assert_eq!(first_stats.font_instances(), 1);
    assert_eq!(
        first_stats.font_bytes(),
        shaped_24.runs()[0].face().bytes().len()
    );
    (first, first_stats)
}

fn assert_placement_color_clipping_and_pipeline_replacement(
    renderer: &mut HeadlessRenderer,
    shaped_24: &Arc<ShapedText>,
    first: &RgbaFrame,
    first_stats: TextRegistryStatistics,
) {
    let shifted_frame = ShapedTextFrame::new(1, ALTERNATE_PIPELINE, shaped_24.clone())
        .with_origin(TextOrigin::new(13.0, 7.0));
    let shifted = renderer
        .render_shaped_text(&shifted_frame, FrameRequest::new(1, 2))
        .expect("integer placement and a replacement pipeline must render");
    let expected_shifted: BTreeSet<_> = changed_coordinates(first)
        .into_iter()
        .map(|(x, y)| (x + 5, y + 3))
        .collect();
    assert_eq!(changed_coordinates(&shifted), expected_shifted);

    let repeated_frame =
        ShapedTextFrame::new(1, PIPELINE, shaped_24.clone()).with_origin(TextOrigin::new(8.0, 4.0));
    let repeated = renderer
        .render_shaped_text(&repeated_frame, FrameRequest::new(1, 3))
        .expect("switching back must reuse exact resources and pixels");
    assert_eq!(first.pixels(), repeated.pixels());
    assert_eq!(renderer.text_registry_statistics().unwrap(), first_stats);

    let red_frame = ShapedTextFrame::new(1, PIPELINE, shaped_24.clone())
        .with_origin(TextOrigin::new(8.0, 4.0))
        .with_color(TextColor::rgba(255, 0, 0, 255));
    let red = renderer
        .render_shaped_text(&red_frame, FrameRequest::new(1, 4))
        .expect("non-premultiplied red text must render");
    assert_eq!(changed_coordinates(&red), changed_coordinates(first));
    for pixel in red.pixels().chunks_exact(4).filter(|pixel| *pixel != CLEAR) {
        assert_eq!(pixel[0], 255, "red coverage blended onto white stays red");
        assert_eq!(pixel[1], pixel[2], "green and blue coverage must match");
        assert_eq!(pixel[3], 255, "the opaque target remains opaque");
    }

    let transparent_frame = ShapedTextFrame::new(1, PIPELINE, shaped_24.clone())
        .with_origin(TextOrigin::new(8.0, 4.0))
        .with_color(TextColor::rgba(255, 0, 0, 0));
    let transparent = renderer
        .render_shaped_text(&transparent_frame, FrameRequest::new(1, 5))
        .expect("transparent text must be a deterministic no-op");
    assert_eq!(changed_pixels(&transparent), 0);

    let clipped_frame = ShapedTextFrame::new(1, PIPELINE, shaped_24.clone())
        .with_origin(TextOrigin::new(168.0, 4.0));
    let clipped = renderer
        .render_shaped_text(&clipped_frame, FrameRequest::new(1, 6))
        .expect("fully offscreen glyphs must be clipped");
    assert_eq!(changed_pixels(&clipped), 0);
    assert_eq!(
        renderer.text_registry_statistics().unwrap(),
        first_stats,
        "placement and color changes must not create font resources"
    );
}

fn assert_second_instance_and_shutdown(
    mut renderer: HeadlessRenderer,
    text: &mut TextSystem,
    first: &RgbaFrame,
    first_stats: TextRegistryStatistics,
) {
    let shaped_30 = text
        .shape(&TextRequest::new("Rust ->", 30.0))
        .expect("second size must shape");
    let larger =
        ShapedTextFrame::new(2, PIPELINE, shaped_30).with_origin(TextOrigin::new(8.0, 2.0));
    let larger_frame = renderer
        .render_shaped_text(&larger, FrameRequest::new(2, 7))
        .expect("a second font instance must render");
    assert!(changed_pixels(&larger_frame) > changed_pixels(first));
    let larger_stats = renderer.text_registry_statistics().unwrap();
    assert_eq!(larger_stats.font_templates(), 1);
    assert_eq!(larger_stats.font_instances(), 2);

    let first_shutdown = renderer
        .shutdown()
        .expect("font deletion, backend shutdown, and EGL release must complete");
    assert_eq!(first_shutdown.text_font_templates_released(), 1);
    assert_eq!(first_shutdown.text_font_instances_released(), 2);
    assert_eq!(
        first_shutdown.text_font_bytes_released(),
        first_stats.font_bytes()
    );
}

#[test]
fn exact_shaped_glyphs_reach_real_webrender_pixels_and_resources_are_scoped() {
    let mut text = TextSystem::new_deterministic(TextLimits::default())
        .expect("pinned Fira Code must initialize");
    let shaped_24 = text
        .shape(&TextRequest::new("Rust ->", 24.0))
        .expect("Latin fixture must shape through HarfRust");
    let mut first_renderer = renderer();
    let (first, first_stats) = render_initial(&mut first_renderer, &shaped_24);
    assert_placement_color_clipping_and_pipeline_replacement(
        &mut first_renderer,
        &shaped_24,
        &first,
        first_stats,
    );
    assert_second_instance_and_shutdown(first_renderer, &mut text, &first, first_stats);

    let mut second_renderer = renderer();
    assert_eq!(
        second_renderer.text_registry_statistics().unwrap(),
        TextRegistryStatistics::default(),
        "a new renderer must start in a fresh WebRender namespace"
    );
    let new_namespace_frame =
        ShapedTextFrame::new(1, PIPELINE, shaped_24).with_origin(TextOrigin::new(8.0, 4.0));
    let second = second_renderer
        .render_shaped_text(&new_namespace_frame, FrameRequest::new(1, 1))
        .expect("the exact blob must be uploaded into the new namespace");
    assert_eq!(first.pixels(), second.pixels());
    assert_eq!(
        second_renderer
            .text_registry_statistics()
            .unwrap()
            .font_templates(),
        1
    );
    let second_shutdown = second_renderer.shutdown().unwrap();
    assert_eq!(second_shutdown.text_font_templates_released(), 1);
    assert_eq!(second_shutdown.text_font_instances_released(), 1);
}
