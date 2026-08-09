use wild_buzzard_dom::DocumentVersion;
use wild_buzzard_html::parse_document;
use wild_buzzard_layout::{
    Au, Color, ComputedStyle, Edges, InitialStyleResolver, MonospaceTextMeasurer, StyleInput,
    StyleResolver, Viewport, layout_document,
};
use wild_buzzard_renderer::{CompileRequest, PipelineKey, SceneCompiler};

use wild_buzzard_headless::{
    ContextStep, FrameRequest, FrameSize, HeadlessError, HeadlessLimits, HeadlessRenderer,
    ResourceKind,
};

const WIDTH: u32 = 64;
const HEIGHT: u32 = 48;
const REVISION_PIPELINE: PipelineKey = PipelineKey::new(41, 7);
const BACKGROUND: [u8; 4] = [220, 20, 30, 255];
const BORDER: [u8; 4] = [5, 10, 200, 255];
const CLEAR: [u8; 4] = [255, 255, 255, 255];

struct ScreenshotStyles;

impl StyleResolver for ScreenshotStyles {
    fn resolve(&self, input: StyleInput<'_>) -> ComputedStyle {
        let frame = input.element.html_attribute("data-frame").is_some();
        let root = input.element.html_attribute("data-root").is_some();
        let mut style = InitialStyleResolver.resolve(input);
        if root {
            style.margin = Edges::default();
        }
        if frame {
            style.margin = Edges::default();
            style.border = Edges::all(Au::from_px(4));
            style.padding = Edges::all(Au::from_px(8));
            style.background_color = color(BACKGROUND);
            style.color = color(BORDER);
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

fn layout(width: u32, height: u32) -> wild_buzzard_layout::LayoutOutput {
    layout_source(
        width,
        height,
        "<html data-root><body data-frame></body></html>",
    )
}

fn layout_source(width: u32, height: u32, source: &str) -> wild_buzzard_layout::LayoutOutput {
    let document = parse_document(source)
        .expect("screenshot fixture must parse")
        .document;
    layout_document(
        &document
            .snapshot()
            .expect("screenshot snapshot must succeed"),
        Viewport::from_css_pixels(width.cast_signed(), height.cast_signed()),
        &ScreenshotStyles,
        &MonospaceTextMeasurer,
    )
    .expect("screenshot fixture must lay out")
}

fn compile(output: &wild_buzzard_layout::LayoutOutput) -> wild_buzzard_renderer::CompiledScene {
    compile_with_pipeline(output, REVISION_PIPELINE)
}

fn compile_with_pipeline(
    output: &wild_buzzard_layout::LayoutOutput,
    pipeline: PipelineKey,
) -> wild_buzzard_renderer::CompiledScene {
    SceneCompiler::default()
        .compile(
            output,
            CompileRequest::new(output.document_version, pipeline),
        )
        .expect("screenshot fixture must compile")
}

fn assert_pre_submission_rejections(
    renderer: &mut HeadlessRenderer,
    output: &wild_buzzard_layout::LayoutOutput,
) {
    let version = output.document_version;
    let wrong_version = DocumentVersion::new(version.document_id(), version.revision() + 1);
    let stale = renderer
        .render(compile(output), FrameRequest::new(wrong_version, 1))
        .unwrap_err();
    assert!(matches!(
        stale,
        HeadlessError::DocumentVersionMismatch {
            expected,
            actual
        } if expected == wrong_version && actual == version
    ));

    let invalid_epoch = renderer
        .render(compile(output), FrameRequest::new(version, u32::MAX))
        .unwrap_err();
    assert!(matches!(
        invalid_epoch,
        HeadlessError::InvalidEpoch { epoch: u32::MAX }
    ));

    let wrong_viewport = layout(WIDTH / 2, HEIGHT);
    let mismatch = renderer
        .render(
            compile(&wrong_viewport),
            FrameRequest::new(wrong_viewport.document_version, 1),
        )
        .unwrap_err();
    assert!(matches!(
        mismatch,
        HeadlessError::ViewportMismatch {
            scene_width: 32,
            scene_height: HEIGHT,
            frame_width: WIDTH,
            frame_height: HEIGHT
        }
    ));
}

fn assert_decoration_frames(
    renderer: &mut HeadlessRenderer,
    output: &wild_buzzard_layout::LayoutOutput,
    size: FrameSize,
) {
    let version = output.document_version;
    let first = renderer
        .render(compile(output), FrameRequest::new(version, 1))
        .expect("first real WebRender frame must render");
    assert_eq!(first.size(), size);
    assert_eq!(first.stride(), WIDTH as usize * 4);
    assert_eq!(first.pixels().len(), WIDTH as usize * HEIGHT as usize * 4);
    assert_eq!(first.document_version(), version);
    assert_eq!(first.epoch(), 1);
    assert_eq!(first.pending_text_runs(), 0);
    assert_eq!(first.pixel(1, 1), Some(BORDER));
    assert_eq!(first.pixel(8, 8), Some(BACKGROUND));
    assert_eq!(first.pixel(8, 40), Some(CLEAR));
    assert_eq!(first.pixel(WIDTH, 0), None);

    let stale_epoch = renderer
        .render(compile(output), FrameRequest::new(version, 1))
        .unwrap_err();
    assert!(matches!(
        stale_epoch,
        HeadlessError::StaleEpoch {
            previous: 1,
            actual: 1
        }
    ));

    let second = renderer
        .render(compile(output), FrameRequest::new(version, 2))
        .expect("second real WebRender frame must render");
    assert_eq!(first.pixels(), second.pixels());

    let alternate = renderer
        .render(
            compile_with_pipeline(output, PipelineKey::new(41, 8)),
            FrameRequest::new(version, 3),
        )
        .expect("switching root pipelines must remove the superseded pipeline");
    assert_eq!(first.pixels(), alternate.pixels());
    let restored = renderer
        .render(compile(output), FrameRequest::new(version, 4))
        .expect("switching back must remain deterministic");
    assert_eq!(first.pixels(), restored.pixels());
}

fn assert_pending_text_frame(renderer: &mut HeadlessRenderer) {
    let text_output = layout_source(
        WIDTH,
        HEIGHT,
        "<html data-root><body data-frame>pending text</body></html>",
    );
    let text_scene = compile(&text_output);
    let expected_pending_text = text_scene.scene().pending_text().len();
    assert!(expected_pending_text > 0);
    let text_frame = renderer
        .render(
            text_scene,
            FrameRequest::new(text_output.document_version, 5),
        )
        .expect("a frame with explicitly pending text must still render decorations");
    assert_eq!(text_frame.pending_text_runs(), expected_pending_text);
    assert_eq!(text_frame.pixel(8, 8), Some(BACKGROUND));
}

fn assert_resource_rejection(size: FrameSize, output: &wild_buzzard_layout::LayoutOutput) {
    let restrictive = HeadlessLimits::default()
        .with_max_width(WIDTH)
        .with_max_height(HEIGHT)
        .with_max_scene_items(1);
    let mut renderer = HeadlessRenderer::new(size, restrictive)
        .expect("second EGL pbuffer context must initialize");
    let resource_error = renderer
        .render(
            compile(output),
            FrameRequest::new(output.document_version, 1),
        )
        .unwrap_err();
    assert!(matches!(
        resource_error,
        HeadlessError::ResourceLimitExceeded {
            resource: ResourceKind::SceneItems,
            observed: 2,
            limit: 1
        }
    ));
    let report = renderer
        .shutdown()
        .expect("rejected-scene renderer must tear down");
    assert!(report.backend_acknowledged());
    assert!(report.context_released());
}

#[test]
fn real_webrender_frame_is_deterministic_bounded_and_cleanly_torn_down() {
    let size = FrameSize::new(WIDTH, HEIGHT).unwrap();
    let limits = HeadlessLimits::default()
        .with_max_width(WIDTH)
        .with_max_height(HEIGHT)
        .with_max_pixel_bytes(WIDTH as usize * HEIGHT as usize * 4);
    let output = layout(WIDTH, HEIGHT);
    let mut renderer = HeadlessRenderer::new(size, limits)
        .expect("host must provide a usable Linux EGL pbuffer context");
    let info = renderer.gl_info().unwrap();
    eprintln!(
        "headless GL backend={:?} EGL={} GL={} renderer={}",
        info.backend(),
        info.egl_version(),
        info.version(),
        info.renderer()
    );
    assert!(!info.egl_version().is_empty());
    assert!(!info.version().is_empty());
    assert!(!info.renderer().is_empty());
    assert_pre_submission_rejections(&mut renderer, &output);
    assert_decoration_frames(&mut renderer, &output, size);
    assert_pending_text_frame(&mut renderer);
    let report = renderer.shutdown().expect("backend and EGL must tear down");
    assert!(report.backend_acknowledged());
    assert!(report.context_released());
    assert!(report.wake_notifications() > 0);
    assert!(report.frame_ready_notifications() >= 5);
    assert_resource_rejection(size, &output);
}

#[test]
fn disabled_context_sources_fail_with_bounded_diagnostics() {
    let size = FrameSize::new(8, 8).unwrap();
    let error = HeadlessRenderer::new(
        size,
        HeadlessLimits::default()
            .with_device_contexts(false)
            .with_x11_fallback(false),
    )
    .err()
    .expect("disabled context sources must fail");
    let HeadlessError::ContextUnavailable { attempts } = error else {
        panic!("unexpected context failure: {error}");
    };
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].step(), ContextStep::EnumerateDevices);
    assert!(attempts[0].detail().contains("disabled by policy"));
    assert!(attempts[0].detail().len() <= 1_024);
}

#[test]
fn two_live_renderers_restore_their_own_contexts() {
    let size = FrameSize::new(WIDTH, HEIGHT).unwrap();
    let output = layout(WIDTH, HEIGHT);
    let mut first = HeadlessRenderer::new(size, HeadlessLimits::default())
        .expect("first EGL renderer must initialize");
    let mut second = HeadlessRenderer::new(size, HeadlessLimits::default())
        .expect("second EGL renderer must initialize and supersede the current context");

    let first_frame = first
        .render(
            compile(&output),
            FrameRequest::new(output.document_version, 1),
        )
        .expect("first renderer must restore its context");
    let second_frame = second
        .render(
            compile(&output),
            FrameRequest::new(output.document_version, 1),
        )
        .expect("second renderer must restore its context");
    assert_eq!(first_frame.pixels(), second_frame.pixels());

    first
        .shutdown()
        .expect("first renderer must reactivate and tear down its own context");
    let second_again = second
        .render(
            compile(&output),
            FrameRequest::new(output.document_version, 2),
        )
        .expect("remaining renderer must reactivate after its peer shuts down");
    assert_eq!(second_frame.pixels(), second_again.pixels());
    second
        .shutdown()
        .expect("second renderer must tear down cleanly");
}
