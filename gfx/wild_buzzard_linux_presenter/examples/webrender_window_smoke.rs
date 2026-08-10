use std::error::Error;
use std::fmt::Write as _;
use std::io;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use wild_buzzard_dom::Document;
use wild_buzzard_layout::{
    Au, Color as LayoutColor, ComputedStyle, Edges, InitialStyleResolver, MonospaceTextMeasurer,
    StyleInput, StyleResolver, Viewport, layout_document,
};
use wild_buzzard_linux_presenter::{
    BrowserAddressSelection, BrowserChromeFocus, BrowserChromeGeometry, BrowserChromeRevision,
    BrowserChromeScene, BrowserChromeState, BrowserChromeTab, BrowserFrameReceipt,
    BrowserFrameRequest, BrowserHitTarget, BrowserNavigationIdentity, BrowserPageScene,
    BrowserPageSceneRevision, BrowserPageSnapshot, BrowserPageUpdate, BrowserTabIdentity,
    LinuxPresentationBackend, WebRenderPresentedWindow, WebRenderTeardownEvidence,
    WebRenderWindowResizeRequest, WebRenderWindowState, prepare_and_attach,
};
use wild_buzzard_platform::{
    PhysicalPoint, PhysicalSize, PixelFormat, ScaleFactor, SurfaceDescriptor, SurfaceIdAllocator,
    SurfaceNamespace, SurfaceRole,
};
use wild_buzzard_renderer::{CompileRequest, PipelineKey, SceneCompiler};
use wild_buzzard_text::{TextLimits, TextRequest, TextSystem};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize as WinitLogicalSize;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::platform::wayland::{
    ActiveEventLoopExtWayland, EventLoopBuilderExtWayland, WindowAttributesExtWayland,
};
use winit::platform::x11::{ActiveEventLoopExtX11, EventLoopBuilderExtX11, WindowAttributesExtX11};
use winit::window::{Window, WindowId};

const ENABLE_ENV: &str = "WILDBUZZARD_REAL_WEBRENDER_WINDOW_TEST";
const BACKEND_ENV: &str = "WILDBUZZARD_DISPLAY_BACKEND";
const CHILD_ENV: &str = "WILDBUZZARD_REAL_WEBRENDER_WINDOW_CHILD";
const HARD_DEADLINE: Duration = Duration::from_secs(25);
const PRESENT_LINGER: Duration = Duration::from_millis(750);
const PAGE_PIPELINE: PipelineKey = PipelineKey::new(94, 1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RequestedBackend {
    Wayland,
    X11,
}

impl RequestedBackend {
    fn from_environment() -> Result<Self, io::Error> {
        match std::env::var(BACKEND_ENV).as_deref() {
            Ok("wayland") => Ok(Self::Wayland),
            Ok("x11") => Ok(Self::X11),
            Ok(value) => Err(io::Error::other(format!(
                "{BACKEND_ENV} must be wayland or x11, not {value:?}"
            ))),
            Err(_) => Err(io::Error::other(format!(
                "{BACKEND_ENV} must be set to exactly wayland or x11"
            ))),
        }
    }

    const fn presentation(self) -> LinuxPresentationBackend {
        match self {
            Self::Wayland => LinuxPresentationBackend::Wayland,
            Self::X11 => LinuxPresentationBackend::X11,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Wayland => "wayland",
            Self::X11 => "x11",
        }
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    if std::env::var(ENABLE_ENV).as_deref() != Ok("1") {
        return Err(
            io::Error::other(format!("refusing to open a display without {ENABLE_ENV}=1")).into(),
        );
    }
    let backend = RequestedBackend::from_environment()?;
    if std::env::var(CHILD_ENV).as_deref() == Ok("1") {
        run_event_loop_child(backend)
    } else {
        run_bounded_parent()
    }
}

fn run_bounded_parent() -> Result<(), Box<dyn Error>> {
    let executable = std::env::current_exe()?;
    let mut child = Command::new(executable)
        .env(CHILD_ENV, "1")
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()?;
    let deadline = Instant::now()
        .checked_add(HARD_DEADLINE)
        .ok_or_else(|| io::Error::other("smoke hard deadline overflowed"))?;
    loop {
        if let Some(status) = child.try_wait()? {
            if status.success() {
                return Ok(());
            }
            return Err(io::Error::other(format!(
                "real WebRender window smoke child failed with {status}"
            ))
            .into());
        }
        if Instant::now() >= deadline {
            child.kill()?;
            let status = child.wait()?;
            return Err(io::Error::other(format!(
                "real WebRender window smoke exceeded {HARD_DEADLINE:?}; killed child with {status}"
            ))
            .into());
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn run_event_loop_child(backend: RequestedBackend) -> Result<(), Box<dyn Error>> {
    let mut builder = EventLoop::<()>::builder();
    match backend {
        RequestedBackend::Wayland => {
            EventLoopBuilderExtWayland::with_wayland(&mut builder);
        }
        RequestedBackend::X11 => {
            EventLoopBuilderExtX11::with_x11(&mut builder);
        }
    }
    let event_loop = builder.build()?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let mut app = SmokeApplication::new(backend)?;
    event_loop.run_app(&mut app)?;
    if let Some(failure) = app.failure {
        return Err(io::Error::other(failure).into());
    }
    if !app.completed {
        return Err(io::Error::other("event loop exited without completed smoke evidence").into());
    }
    Ok(())
}

struct SmokeStyles;

impl StyleResolver for SmokeStyles {
    fn resolve(&self, input: StyleInput<'_>) -> ComputedStyle {
        let is_page = input.element.html_attribute("data-smoke-page").is_some();
        let mut style = InitialStyleResolver.resolve(input);
        if is_page {
            style.background_color = LayoutColor {
                red: 22,
                green: 86,
                blue: 118,
                alpha: 255,
            };
            style.padding = Edges::all(Au::from_px(72));
        }
        style
    }
}

fn smoke_document() -> Result<Document, io::Error> {
    let mut document = Document::new();
    let html = document
        .create_html_element("html")
        .map_err(|error| io::Error::other(error.to_string()))?;
    let body = document
        .create_html_element("body")
        .map_err(|error| io::Error::other(error.to_string()))?;
    document
        .set_html_attribute(body, "data-smoke-page", "")
        .map_err(|error| io::Error::other(error.to_string()))?;
    document
        .append_child(html, body)
        .map_err(|error| io::Error::other(error.to_string()))?;
    document
        .append_child(document.document_node(), html)
        .map_err(|error| io::Error::other(error.to_string()))?;
    Ok(document)
}

struct SmokeApplication {
    backend: RequestedBackend,
    surface_allocator: SurfaceIdAllocator,
    document: Document,
    text: TextSystem,
    owner: Option<WebRenderPresentedWindow>,
    receipt: Option<BrowserFrameReceipt>,
    resize_target: Option<PhysicalSize>,
    resize_observed: bool,
    finish_at: Option<Instant>,
    explicitly_suspended: bool,
    completed: bool,
    failure: Option<String>,
}

impl SmokeApplication {
    fn new(backend: RequestedBackend) -> Result<Self, io::Error> {
        let namespace = SurfaceNamespace::new(94_001)
            .ok_or_else(|| io::Error::other("smoke surface namespace must be nonzero"))?;
        Ok(Self {
            backend,
            surface_allocator: SurfaceIdAllocator::new(namespace),
            document: smoke_document()?,
            text: TextSystem::new_linux(TextLimits::default())
                .map_err(|error| io::Error::other(error.to_string()))?,
            owner: None,
            receipt: None,
            resize_target: None,
            resize_observed: false,
            finish_at: None,
            explicitly_suspended: false,
            completed: false,
            failure: None,
        })
    }

    fn create_owner(
        &mut self,
        event_loop: &ActiveEventLoop,
    ) -> Result<WebRenderPresentedWindow, String> {
        let backend_matches = match self.backend {
            RequestedBackend::Wayland => ActiveEventLoopExtWayland::is_wayland(event_loop),
            RequestedBackend::X11 => ActiveEventLoopExtX11::is_x11(event_loop),
        };
        if !backend_matches {
            return Err(format!(
                "requested {} but winit created a different backend",
                self.backend.label()
            ));
        }
        let surface = self
            .surface_allocator
            .allocate()
            .map_err(|error| format!("surface identity allocation failed: {error}"))?;
        let requested_backend = self.backend;
        let presenter = prepare_and_attach(
            event_loop,
            requested_backend.presentation(),
            move |preparation| -> Result<_, io::Error> {
                let application_id = "org.wildbuzzard.webrender-window-smoke".to_owned();
                let mut attributes = Window::default_attributes()
                    .with_title("Wild Buzzard WebRender Window Smoke")
                    .with_inner_size(WinitLogicalSize::new(640.0, 480.0));
                attributes = match requested_backend {
                    RequestedBackend::Wayland => WindowAttributesExtWayland::with_name(
                        attributes,
                        application_id.clone(),
                        application_id,
                    ),
                    RequestedBackend::X11 => {
                        let visual = preparation.x11_visual_id().ok_or_else(|| {
                            io::Error::other("presenter omitted its required X11 visual")
                        })?;
                        WindowAttributesExtX11::with_x11_visual(
                            WindowAttributesExtX11::with_name(
                                attributes,
                                application_id.clone(),
                                application_id,
                            ),
                            visual,
                        )
                    }
                };
                let window = event_loop
                    .create_window(attributes)
                    .map_err(|error| io::Error::other(error.to_string()))?;
                let native_size = window.inner_size();
                let size = PhysicalSize::new(native_size.width, native_size.height)
                    .map_err(|error| io::Error::other(error.to_string()))?;
                let scale = ScaleFactor::new(window.scale_factor())
                    .map_err(|error| io::Error::other(error.to_string()))?;
                let descriptor = SurfaceDescriptor {
                    id: surface,
                    size,
                    scale,
                    format: PixelFormat::Rgba8Srgb,
                    role: SurfaceRole::Window,
                };
                Ok((window, descriptor))
            },
        )
        .map_err(|error| error.to_string())?;
        presenter
            .into_browser_compositor()
            .map_err(|error| error.to_string())
    }

    #[allow(clippy::too_many_lines)]
    fn submit_frame(&mut self, event_loop: &ActiveEventLoop) -> Result<(), String> {
        let owner = self
            .owner
            .as_ref()
            .ok_or_else(|| "redraw arrived without a WebRender owner".to_owned())?;
        if owner.state() != WebRenderWindowState::Active || !self.resize_observed {
            return Err(format!(
                "redraw arrived before active resized compositor state: {:?}/{}",
                owner.state(),
                self.resize_observed
            ));
        }
        let snapshot = owner.surface_snapshot();
        let geometry = BrowserChromeGeometry::for_surface(snapshot)
            .map_err(|error| format!("browser geometry failed: {error}"))?;
        let content = geometry
            .content()
            .size()
            .ok_or_else(|| "resized smoke surface has no page content extent".to_owned())?;
        let width = i32::try_from(content.width)
            .map_err(|_| "page width does not fit layout geometry".to_owned())?;
        let height = i32::try_from(content.height)
            .map_err(|_| "page height does not fit layout geometry".to_owned())?;
        let layout = layout_document(
            &self
                .document
                .snapshot()
                .map_err(|error| format!("page snapshot failed: {error}"))?,
            Viewport::from_css_pixels(width, height),
            &SmokeStyles,
            &MonospaceTextMeasurer,
        )
        .map_err(|error| format!("page layout failed: {error}"))?;
        let scene = SceneCompiler::default()
            .compile(
                &layout,
                CompileRequest::new(self.document.version(), PAGE_PIPELINE),
            )
            .map_err(|error| format!("page scene compilation failed: {error}"))?;
        if scene.scene().items().is_empty() || !scene.scene().pending_text().is_empty() {
            return Err("smoke page must contain paint but no unresolved page text".to_owned());
        }
        let page = BrowserPageScene::new(
            BrowserNavigationIdentity::new(1).expect("nonzero smoke navigation"),
            BrowserPageSceneRevision::new(1).expect("nonzero smoke page revision"),
            scene,
            Box::new([]),
        )
        .map_err(|error| format!("page publication failed: {error}"))?;
        let page_snapshot = BrowserPageSnapshot::Scene(page.identity());

        let tab_identity = BrowserTabIdentity::new(1).expect("nonzero smoke tab");
        let tab_title = self
            .text
            .shape(&TextRequest::new("Wild Buzzard", 14.0))
            .map_err(|error| format!("tab shaping failed: {error}"))?;
        let address = self
            .text
            .shape(&TextRequest::new("about:wildbuzzard", 14.0))
            .map_err(|error| format!("address shaping failed: {error}"))?;
        let status = self
            .text
            .shape(&TextRequest::new("Rust page + browser chrome", 12.0))
            .map_err(|error| format!("status shaping failed: {error}"))?;
        let address_end = address.text().len();
        let chrome_state = BrowserChromeState::new(
            vec![BrowserChromeTab::new(tab_identity, tab_title)].into_boxed_slice(),
            Some(tab_identity),
            address,
        )
        .with_address_selection(BrowserAddressSelection::new(address_end, address_end))
        .with_status(Some(status))
        .with_focus(BrowserChromeFocus::AddressBar);
        let chrome = BrowserChromeScene::new(
            BrowserChromeRevision::new(1).expect("nonzero smoke chrome revision"),
            snapshot,
            chrome_state,
        )
        .map_err(|error| format!("chrome scene failed: {error}"))?;
        let chrome_revision = chrome.revision();
        let request = BrowserFrameRequest::new(snapshot, page_snapshot, chrome_revision, 1, 1);
        let owner = self
            .owner
            .as_mut()
            .ok_or_else(|| "WebRender owner disappeared before submission".to_owned())?;
        let receipt = owner
            .submit_browser_frame(BrowserPageUpdate::Install(page), Some(chrome), request)
            .map_err(|error| format!("browser composition failed: {error}"))?;
        let expected_bytes = u64::from(snapshot.size().width)
            .checked_mul(u64::from(snapshot.size().height))
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or_else(|| "RGBA8-equivalent smoke byte count overflowed".to_owned())?;
        if receipt.request() != request
            || receipt.backend_publish_id() == 0
            || receipt.rgba8_byte_equivalent() != expected_bytes
            || receipt.page_epoch() != Some(1)
            || receipt.chrome_epoch() != 1
            || receipt.page_display_list_bytes() == 0
            || receipt.chrome_display_list_bytes() == 0
            || receipt.root_display_list_bytes() == 0
            || receipt.chrome_primitives() == 0
            || !receipt.renderer_frame_submitted()
            || !receipt.egl_swap_submitted()
            || receipt.desktop_compositor_acknowledged()
        {
            return Err(format!("invalid browser composition receipt: {receipt:?}"));
        }
        let address_point = PhysicalPoint {
            x: i32::try_from(geometry.address_field().x() + 1)
                .map_err(|_| "address hit x overflowed".to_owned())?,
            y: i32::try_from(geometry.address_field().y() + 1)
                .map_err(|_| "address hit y overflowed".to_owned())?,
        };
        let address_hit = owner
            .hit_test_browser(address_point, snapshot)
            .map_err(|error| format!("address hit test failed: {error}"))?
            .ok_or_else(|| "address hit test returned no target".to_owned())?;
        let page_point = PhysicalPoint {
            x: i32::try_from(geometry.content().x() + 12)
                .map_err(|_| "page hit x overflowed".to_owned())?,
            y: i32::try_from(geometry.content().y() + 12)
                .map_err(|_| "page hit y overflowed".to_owned())?,
        };
        let page_hit = owner
            .hit_test_browser(page_point, snapshot)
            .map_err(|error| format!("page hit test failed: {error}"))?
            .ok_or_else(|| "page hit test returned no target".to_owned())?;
        if address_hit.receipt() != receipt
            || address_hit.target() != BrowserHitTarget::AddressBar
            || page_hit.receipt() != receipt
            || !matches!(
                page_hit.target(),
                BrowserHitTarget::Page {
                    point: PhysicalPoint { x: 12, y: 12 },
                    ..
                }
            )
        {
            return Err(format!(
                "browser hit authority mismatch: address={address_hit:?} page={page_hit:?}"
            ));
        }
        self.receipt = Some(receipt);
        let finish_at = Instant::now()
            .checked_add(PRESENT_LINGER)
            .ok_or_else(|| "presentation linger deadline overflowed".to_owned())?;
        self.finish_at = Some(finish_at);
        event_loop.set_control_flow(ControlFlow::WaitUntil(finish_at));
        Ok(())
    }

    fn finish_success(&mut self, event_loop: &ActiveEventLoop) {
        let Some(receipt) = self.receipt else {
            self.fail("finish requested without a frame receipt", event_loop);
            return;
        };
        let Some(owner) = self.owner.take() else {
            self.fail("finish requested without a WebRender owner", event_loop);
            return;
        };
        let report = match owner.shutdown() {
            Ok(report) => report,
            Err(error) => {
                self.failure = Some(format!("ordered WebRender shutdown failed: {error}"));
                event_loop.exit();
                return;
            }
        };
        let native = report.presentation();
        if report.backend_shutdown() != WebRenderTeardownEvidence::Confirmed
            || report.renderer_deinitialization() != WebRenderTeardownEvidence::Confirmed
            || report.text_font_templates_released() == 0
            || report.text_font_instances_released() == 0
            || report.text_font_bytes_released() == 0
            || native.surface() != receipt.request().surface().surface()
            || native.submitted_frames() != 1
            || native.last_sequence() != Some(1)
            || !self.resize_observed
        {
            self.failure = Some(format!("invalid ordered shutdown evidence: {report:?}"));
            event_loop.exit();
            return;
        }
        println!(
            "W6-A4R {} page+chrome publish={} page_epoch={:?} chrome_epoch={} resize=observed EGL_swap=accepted compositor_ack=false",
            self.backend.label(),
            receipt.backend_publish_id(),
            receipt.page_epoch(),
            receipt.chrome_epoch(),
        );
        self.completed = true;
        event_loop.exit();
    }

    fn fail(&mut self, message: impl Into<String>, event_loop: &ActiveEventLoop) {
        let mut detail = message.into();
        if let Some(owner) = self.owner.take()
            && let Err(error) = owner.shutdown()
        {
            let _ = write!(detail, "; ordered cleanup also failed: {error}");
        }
        self.failure = Some(detail);
        event_loop.exit();
    }
}

impl ApplicationHandler for SmokeApplication {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(owner) = self.owner.as_mut() {
            if self.explicitly_suspended {
                let snapshot = owner.surface_snapshot();
                match owner.resume(snapshot) {
                    Ok(_) => self.explicitly_suspended = false,
                    Err(error) => {
                        self.fail(format!("resume failed: {error}"), event_loop);
                        return;
                    }
                }
            }
            if self.resize_observed {
                owner.request_redraw();
            } else if let Some(target) = self.resize_target {
                let _ = owner.request_inner_size(target);
            }
            return;
        }
        match self.create_owner(event_loop) {
            Ok(owner) => {
                let current = owner.surface_snapshot().size();
                let target = if (current.width, current.height) == (720, 540) {
                    PhysicalSize::new(704, 528).expect("bounded alternate smoke size")
                } else {
                    PhysicalSize::new(720, 540).expect("bounded smoke size")
                };
                let _ = owner.request_inner_size(target);
                self.resize_target = Some(target);
                self.owner = Some(owner);
            }
            Err(error) => self.fail(
                format!("window/WebRender startup failed: {error}"),
                event_loop,
            ),
        }
    }

    fn suspended(&mut self, event_loop: &ActiveEventLoop) {
        let Some(owner) = self.owner.as_mut() else {
            return;
        };
        let snapshot = owner.surface_snapshot();
        match owner.suspend(snapshot) {
            Ok(_) => self.explicitly_suspended = true,
            Err(error) => self.fail(format!("suspend failed: {error}"), event_loop),
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        let Some(owner) = self.owner.as_mut() else {
            return;
        };
        if !owner.matches_window_id(window_id) {
            self.fail("event named a foreign window", event_loop);
            return;
        }
        match event {
            WindowEvent::Resized(size) => {
                let size = match PhysicalSize::new(size.width, size.height) {
                    Ok(size) => size,
                    Err(error) => {
                        self.fail(format!("invalid native resize: {error}"), event_loop);
                        return;
                    }
                };
                if self.resize_observed || self.resize_target != Some(size) {
                    // X11 can deliver the initial ConfigureNotify after the
                    // server has already applied our later explicit request.
                    // Only the target notification may drive this smoke's
                    // exact EGL/WebRender resize transition.
                    return;
                }
                let request = WebRenderWindowResizeRequest::new(owner.surface_snapshot(), size);
                match owner.resize(request) {
                    Ok(snapshot) if snapshot.size().width != 0 && snapshot.size().height != 0 => {
                        self.explicitly_suspended = false;
                        if self.resize_target == Some(snapshot.size()) {
                            self.resize_observed = true;
                            owner.request_redraw();
                        }
                    }
                    Ok(_) => {}
                    Err(error) => self.fail(format!("resize failed: {error}"), event_loop),
                }
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                let scale = match ScaleFactor::new(scale_factor) {
                    Ok(scale) => scale,
                    Err(error) => {
                        self.fail(format!("invalid native scale: {error}"), event_loop);
                        return;
                    }
                };
                let snapshot = owner.surface_snapshot();
                if let Err(error) = owner.update_scale(snapshot, scale) {
                    self.fail(format!("scale update failed: {error}"), event_loop);
                }
            }
            WindowEvent::RedrawRequested if self.receipt.is_none() && self.resize_observed => {
                if let Err(error) = self.submit_frame(event_loop) {
                    self.fail(error, event_loop);
                }
            }
            WindowEvent::CloseRequested => {
                self.fail("native close arrived before smoke completion", event_loop);
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self
            .finish_at
            .is_some_and(|finish_at| Instant::now() >= finish_at)
        {
            self.finish_success(event_loop);
        } else if self.receipt.is_none()
            && self.resize_observed
            && let Some(owner) = self.owner.as_ref()
            && owner.state() == WebRenderWindowState::Active
        {
            owner.request_redraw();
        }
    }
}
