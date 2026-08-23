use std::cell::RefCell;
use std::error::Error;
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::Path;
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::thread;
use std::time::{Duration, Instant};

use wild_buzzard_dom::Document;
use wild_buzzard_layout::{
    Au, Color as LayoutColor, ComputedStyle, Edges, InitialStyleResolver, MonospaceTextMeasurer,
    StyleInput, StyleResolver, Viewport, layout_document,
};
use wild_buzzard_linux_presenter::{
    BrowserAddressSelection, BrowserChromeDirection, BrowserChromeElementIdentity,
    BrowserChromeFocus, BrowserChromeGeometry, BrowserChromeRevision, BrowserChromeScene,
    BrowserChromeState, BrowserChromeTab, BrowserElementAvailability, BrowserFrameReceipt,
    BrowserFrameRequest, BrowserHitTarget, BrowserNavigationIdentity, BrowserPageScene,
    BrowserPageSceneRevision, BrowserPageSnapshot, BrowserPageUpdate, BrowserPhysicalRect,
    BrowserPrimaryActionKind, BrowserPrimaryChromeState, BrowserPrimaryControl,
    BrowserPrimaryControlKind, BrowserPrimaryLayoutPreview, BrowserPrimaryPopup,
    BrowserPrimaryPopupKind, BrowserPrimaryPopupRow, BrowserReloadStopMode,
    BrowserSiteIdentityKind, BrowserTabIdentity, LinuxPresentationBackend,
    LinuxPresentationCapabilities, LinuxPresentationPolicy, LinuxPresentedWindow,
    MAX_LINUX_PRESENTATION_PROFILE_ATTEMPTS, WebRenderPresentedWindow, WebRenderTeardownEvidence,
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
const CAPTURE_RAW_ENV: &str = "WILDBUZZARD_INTERNAL_CAPTURE_BGRA_PATH";
const HARD_DEADLINE: Duration = Duration::from_secs(25);
const PRESENT_LINGER: Duration = Duration::from_millis(750);
const PAGE_PIPELINE: PipelineKey = PipelineKey::new(94, 1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SmokeWindowEventIdentity {
    Selected,
    RetiredProfile,
    Foreign,
}

fn classify_smoke_window_event<T: Eq>(
    selected: &T,
    retired_profiles: &[T],
    event: &T,
) -> SmokeWindowEventIdentity {
    if event == selected {
        SmokeWindowEventIdentity::Selected
    } else if retired_profiles.contains(event) {
        SmokeWindowEventIdentity::RetiredProfile
    } else {
        SmokeWindowEventIdentity::Foreign
    }
}

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

struct CreatedSmokeOwner {
    owner: WebRenderPresentedWindow,
    selected_window_id: WindowId,
    retired_profile_window_ids: Box<[WindowId]>,
    profile_windows_created: usize,
    egl_profile_attempts: Box<[LinuxPresentationCapabilities]>,
}

fn finish_profile_selection(
    presenter: LinuxPresentedWindow,
    created_profile_windows: &[(WindowId, LinuxPresentationCapabilities)],
) -> Result<CreatedSmokeOwner, String> {
    if created_profile_windows.is_empty()
        || created_profile_windows.len() > MAX_LINUX_PRESENTATION_PROFILE_ATTEMPTS
    {
        return Err(format!(
            "profile ladder created {} windows; expected 1..={MAX_LINUX_PRESENTATION_PROFILE_ATTEMPTS}",
            created_profile_windows.len()
        ));
    }
    for (index, (id, _)) in created_profile_windows.iter().enumerate() {
        if created_profile_windows[..index]
            .iter()
            .any(|(previous, _)| previous == id)
        {
            return Err("profile ladder reused a native window identity".to_owned());
        }
    }
    let selected = created_profile_windows
        .iter()
        .map(|(id, _)| *id)
        .filter(|id| presenter.matches_window_id(*id))
        .collect::<Vec<_>>();
    let [selected_window_id] = selected.as_slice() else {
        return Err(format!(
            "selected presenter matched {} of {} profile window identities",
            selected.len(),
            created_profile_windows.len()
        ));
    };
    let retired_profile_window_ids = created_profile_windows
        .iter()
        .map(|(id, _)| *id)
        .filter(|id| id != selected_window_id)
        .collect::<Vec<_>>()
        .into_boxed_slice();
    let egl_profile_attempts = created_profile_windows
        .iter()
        .map(|(_, capabilities)| *capabilities)
        .collect::<Vec<_>>()
        .into_boxed_slice();
    let owner = presenter
        .into_browser_compositor()
        .map_err(|error| error.to_string())?;
    Ok(CreatedSmokeOwner {
        owner,
        selected_window_id: *selected_window_id,
        retired_profile_window_ids,
        profile_windows_created: created_profile_windows.len(),
        egl_profile_attempts,
    })
}

struct SmokeApplication {
    backend: RequestedBackend,
    surface_allocator: SurfaceIdAllocator,
    document: Document,
    text: TextSystem,
    owner: Option<WebRenderPresentedWindow>,
    selected_window_id: Option<WindowId>,
    retired_profile_window_ids: Box<[WindowId]>,
    profile_windows_created: usize,
    egl_profile_attempts: Box<[LinuxPresentationCapabilities]>,
    retired_profile_events: u64,
    receipt: Option<BrowserFrameReceipt>,
    capture_summary: Option<(PhysicalSize, usize, BrowserPhysicalRect, usize)>,
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
            selected_window_id: None,
            retired_profile_window_ids: Box::new([]),
            profile_windows_created: 0,
            egl_profile_attempts: Box::new([]),
            retired_profile_events: 0,
            receipt: None,
            capture_summary: None,
            resize_target: None,
            resize_observed: false,
            finish_at: None,
            explicitly_suspended: false,
            completed: false,
            failure: None,
        })
    }

    fn create_owner(&mut self, event_loop: &ActiveEventLoop) -> Result<CreatedSmokeOwner, String> {
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
        let created_profile_windows = Rc::new(RefCell::new(Vec::with_capacity(
            MAX_LINUX_PRESENTATION_PROFILE_ATTEMPTS,
        )));
        let closure_profile_windows = Rc::clone(&created_profile_windows);
        let presenter = prepare_and_attach(
            event_loop,
            requested_backend.presentation(),
            LinuxPresentationPolicy::AutomaticCompatible,
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
                closure_profile_windows
                    .borrow_mut()
                    .push((window.id(), preparation.capabilities()));
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
        finish_profile_selection(presenter, &created_profile_windows.borrow())
    }

    fn primary_controls(
        &mut self,
        preview: &BrowserPrimaryLayoutPreview,
    ) -> Result<Box<[BrowserPrimaryControl]>, String> {
        BrowserPrimaryControlKind::ALL
            .into_iter()
            .enumerate()
            .map(|(index, kind)| {
                let availability = match kind {
                    BrowserPrimaryControlKind::Back | BrowserPrimaryControlKind::Forward => {
                        BrowserElementAvailability::Disabled
                    }
                    BrowserPrimaryControlKind::Overflow
                        if preview.hidden_controls().is_empty()
                            || preview.popup_row_capacity() == 0 =>
                    {
                        BrowserElementAvailability::Disabled
                    }
                    BrowserPrimaryControlKind::SiteIdentity
                    | BrowserPrimaryControlKind::AllTabs
                    | BrowserPrimaryControlKind::ApplicationMenu
                        if preview.popup_row_capacity() == 0 =>
                    {
                        BrowserElementAvailability::Disabled
                    }
                    _ => BrowserElementAvailability::Enabled,
                };
                let label = self
                    .text
                    .shape(&TextRequest::new(format!("{kind:?}"), 14.0))
                    .map_err(|error| format!("primary control label shaping failed: {error}"))?;
                Ok(BrowserPrimaryControl::new(
                    BrowserChromeElementIdentity::new(
                        100 + u64::try_from(index).expect("fixed primary index"),
                    )
                    .expect("nonzero primary element"),
                    kind,
                    label,
                    availability,
                ))
            })
            .collect::<Result<Vec<_>, String>>()
            .map(Vec::into_boxed_slice)
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
        let primary_preview = BrowserPrimaryLayoutPreview::for_surface(
            snapshot,
            BrowserChromeDirection::LeftToRight,
            1,
        )
        .map_err(|error| format!("primary layout preview failed: {error}"))?;
        let primary_controls = self.primary_controls(&primary_preview)?;
        let primary = BrowserPrimaryChromeState::new(
            BrowserChromeDirection::LeftToRight,
            primary_controls,
            BrowserReloadStopMode::Reload,
            BrowserSiteIdentityKind::Internal,
        );
        let address_end = address.text().len();
        let chrome_state = BrowserChromeState::new(
            vec![BrowserChromeTab::new(tab_identity, tab_title)].into_boxed_slice(),
            Some(tab_identity),
            address,
        )
        .with_address_selection(BrowserAddressSelection::new(address_end, address_end))
        .with_status(Some(status))
        .with_focus(BrowserChromeFocus::AddressBar)
        .with_primary_chrome(Some(primary));
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
            || receipt.request().surface().capabilities() != owner.capabilities()
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
            x: i32::try_from(primary_preview.address_field().x() + 1)
                .map_err(|_| "address hit x overflowed".to_owned())?,
            y: i32::try_from(primary_preview.address_field().y() + 1)
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
        let app_controls = self.primary_controls(&primary_preview)?;
        let application = app_controls
            .iter()
            .find(|control| control.kind() == BrowserPrimaryControlKind::ApplicationMenu)
            .map(BrowserPrimaryControl::element)
            .ok_or_else(|| "fixed primary inventory omitted ApplicationMenu".to_owned())?;
        let action_rows = [
            (
                BrowserPrimaryActionKind::NewTab,
                "New tab",
                BrowserElementAvailability::Enabled,
            ),
            (
                BrowserPrimaryActionKind::CloseTab,
                "Close tab",
                BrowserElementAvailability::Enabled,
            ),
            (
                BrowserPrimaryActionKind::Back,
                "Back",
                BrowserElementAvailability::Disabled,
            ),
            (
                BrowserPrimaryActionKind::Forward,
                "Forward",
                BrowserElementAvailability::Disabled,
            ),
            (
                BrowserPrimaryActionKind::ReloadStop,
                "Reload",
                BrowserElementAvailability::Enabled,
            ),
        ];
        let app_rows = action_rows
            .into_iter()
            .enumerate()
            .map(|(index, (action, label, availability))| {
                let label = self
                    .text
                    .shape(&TextRequest::new(label, 14.0))
                    .map_err(|error| format!("application row shaping failed: {error}"))?;
                Ok(BrowserPrimaryPopupRow::action(
                    BrowserChromeElementIdentity::new(
                        200 + u64::try_from(index).expect("fixed application row index"),
                    )
                    .expect("nonzero application row"),
                    action,
                    label,
                    availability,
                ))
            })
            .collect::<Result<Vec<_>, String>>()?;
        let first_app_row = app_rows[0].element();
        let app_popup = BrowserPrimaryPopup::new(
            BrowserPrimaryPopupKind::ApplicationMenu,
            application,
            app_rows.into_boxed_slice(),
        );
        let open_primary = BrowserPrimaryChromeState::new(
            BrowserChromeDirection::LeftToRight,
            app_controls,
            BrowserReloadStopMode::Reload,
            BrowserSiteIdentityKind::Internal,
        )
        .with_popup(Some(app_popup));
        let open_tab_title = self
            .text
            .shape(&TextRequest::new("Wild Buzzard", 14.0))
            .map_err(|error| format!("second tab shaping failed: {error}"))?;
        let open_address = self
            .text
            .shape(&TextRequest::new("about:wildbuzzard", 14.0))
            .map_err(|error| format!("second address shaping failed: {error}"))?;
        let open_status = self
            .text
            .shape(&TextRequest::new("Rust primary application panel", 12.0))
            .map_err(|error| format!("second status shaping failed: {error}"))?;
        let open_chrome_state = BrowserChromeState::new(
            vec![BrowserChromeTab::new(tab_identity, open_tab_title)].into_boxed_slice(),
            Some(tab_identity),
            open_address,
        )
        .with_status(Some(open_status))
        .with_focus(BrowserChromeFocus::PopupRow(first_app_row))
        .with_primary_chrome(Some(open_primary));
        let open_chrome = BrowserChromeScene::new(
            BrowserChromeRevision::new(2).expect("nonzero second chrome revision"),
            snapshot,
            open_chrome_state,
        )
        .map_err(|error| format!("open primary chrome scene failed: {error}"))?;
        let open_popup = open_chrome
            .primary_layout()
            .and_then(|layout| layout.popup())
            .ok_or_else(|| "second chrome omitted its exact open popup".to_owned())?;
        let first_row_rect = open_popup.rows()[0]
            .rect()
            .ok_or_else(|| "first application row is outside popup capacity".to_owned())?;
        let capture_geometry = open_chrome.geometry();
        let second_request =
            BrowserFrameRequest::new(snapshot, page_snapshot, open_chrome.revision(), 2, 2);
        let owner = self
            .owner
            .as_mut()
            .ok_or_else(|| "WebRender owner disappeared before popup submission".to_owned())?;
        let capture = owner
            .submit_browser_frame_with_capture(
                BrowserPageUpdate::Retain,
                Some(open_chrome),
                second_request,
            )
            .map_err(|error| format!("open popup browser composition failed: {error}"))?;
        let second_receipt = capture.receipt();
        if second_receipt.request() != second_request
            || second_receipt.page_epoch() != Some(1)
            || second_receipt.chrome_epoch() != 2
            || second_receipt.page_display_list_bytes() != receipt.page_display_list_bytes()
            || second_receipt.chrome_display_list_bytes() == 0
            || second_receipt.root_display_list_bytes() == 0
            || second_receipt.backend_publish_id() <= receipt.backend_publish_id()
            || capture.size() != snapshot.size()
            || capture.stride()
                != usize::try_from(snapshot.size().width)
                    .map_err(|_| "capture width did not fit usize".to_owned())?
                    .checked_mul(4)
                    .ok_or_else(|| "capture stride overflowed".to_owned())?
            || capture.pixels().len()
                != capture
                    .stride()
                    .checked_mul(
                        usize::try_from(snapshot.size().height)
                            .map_err(|_| "capture height did not fit usize".to_owned())?,
                    )
                    .ok_or_else(|| "capture byte length overflowed".to_owned())?
            || capture.content_rect() != capture_geometry.content()
            || capture.content().row(0).is_none()
            || capture.desktop_compositor_acknowledged()
        {
            return Err(format!(
                "invalid receipt-bound open-popup capture: {capture:?}"
            ));
        }
        if let Ok(path) = std::env::var(CAPTURE_RAW_ENV) {
            let path = Path::new(&path);
            if !path.is_absolute() {
                return Err(format!("{CAPTURE_RAW_ENV} must be an absolute path"));
            }
            fs::write(path, capture.pixels())
                .map_err(|error| format!("writing internal BGRA evidence failed: {error}"))?;
        }
        self.capture_summary = Some((
            capture.size(),
            capture.stride(),
            capture.content_rect(),
            capture.pixels().len(),
        ));
        let popup_row_hit = owner
            .hit_test_browser(
                PhysicalPoint {
                    x: i32::try_from(first_row_rect.x() + 1)
                        .map_err(|_| "popup row x overflowed".to_owned())?,
                    y: i32::try_from(first_row_rect.y() + 1)
                        .map_err(|_| "popup row y overflowed".to_owned())?,
                },
                snapshot,
            )
            .map_err(|error| format!("popup row hit test failed: {error}"))?
            .ok_or_else(|| "popup row hit returned no target".to_owned())?;
        let popup_dismiss_hit = owner
            .hit_test_browser(page_point, snapshot)
            .map_err(|error| format!("popup dismiss hit test failed: {error}"))?
            .ok_or_else(|| "popup dismiss hit returned no target".to_owned())?;
        if popup_row_hit.receipt() != second_receipt
            || !matches!(
                popup_row_hit.target(),
                BrowserHitTarget::PrimaryPopupRow { element, .. } if element == first_app_row
            )
            || popup_dismiss_hit.receipt() != second_receipt
            || !matches!(
                popup_dismiss_hit.target(),
                BrowserHitTarget::PrimaryPopupDismiss {
                    kind: BrowserPrimaryPopupKind::ApplicationMenu,
                    anchor,
                } if anchor == application
            )
        {
            return Err(format!(
                "primary popup hit authority mismatch: row={popup_row_hit:?} dismiss={popup_dismiss_hit:?}"
            ));
        }
        self.receipt = Some(second_receipt);
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
        let Some((capture_size, capture_stride, capture_content, capture_bytes)) =
            self.capture_summary
        else {
            self.fail(
                "finish requested without internal capture evidence",
                event_loop,
            );
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
            || native.submitted_frames() != 2
            || native.last_sequence() != Some(2)
            || native.capabilities() != receipt.request().surface().capabilities()
            || report.capabilities() != native.capabilities()
            || self.profile_windows_created != self.retired_profile_window_ids.len() + 1
            || self.egl_profile_attempts.len() != self.profile_windows_created
            || !self.resize_observed
        {
            self.failure = Some(format!("invalid ordered shutdown evidence: {report:?}"));
            event_loop.exit();
            return;
        }
        println!(
            "W9-A4X {} renderer_bound_capabilities={:?} egl_profile_attempts={:?} profile_windows_created={} retired_profile_events={} selected_resize_redraw_frames=confirmed no_capture_first_frame=true one_shot_capture_second_frame=true capture_size={capture_size:?} capture_stride={capture_stride} capture_content={capture_content:?} capture_bytes={capture_bytes} primary-toolbar+application-popup publish={} page_epoch={:?} chrome_epoch={} resize=observed EGL_swap=accepted compositor_ack=false",
            self.backend.label(),
            native.capabilities(),
            self.egl_profile_attempts,
            self.profile_windows_created,
            self.retired_profile_events,
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
            Ok(created) => {
                let CreatedSmokeOwner {
                    owner,
                    selected_window_id,
                    retired_profile_window_ids,
                    profile_windows_created,
                    egl_profile_attempts,
                } = created;
                let current = owner.surface_snapshot().size();
                let target = if (current.width, current.height) == (720, 540) {
                    PhysicalSize::new(704, 528).expect("bounded alternate smoke size")
                } else {
                    PhysicalSize::new(720, 540).expect("bounded smoke size")
                };
                let _ = owner.request_inner_size(target);
                self.resize_target = Some(target);
                self.selected_window_id = Some(selected_window_id);
                self.retired_profile_window_ids = retired_profile_window_ids;
                self.profile_windows_created = profile_windows_created;
                self.egl_profile_attempts = egl_profile_attempts;
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
        let Some(selected_window_id) = self.selected_window_id else {
            return;
        };
        let identity = classify_smoke_window_event(
            &selected_window_id,
            &self.retired_profile_window_ids,
            &window_id,
        );
        let owner_matches = self
            .owner
            .as_ref()
            .is_some_and(|owner| owner.matches_window_id(window_id));
        if owner_matches != (identity == SmokeWindowEventIdentity::Selected) {
            self.fail(
                "presenter/window identity disagreed with smoke profile inventory",
                event_loop,
            );
            return;
        }
        match identity {
            SmokeWindowEventIdentity::Selected => {}
            SmokeWindowEventIdentity::RetiredProfile => {
                let Some(count) = self.retired_profile_events.checked_add(1) else {
                    self.fail("retired profile event count overflowed", event_loop);
                    return;
                };
                self.retired_profile_events = count;
                return;
            }
            SmokeWindowEventIdentity::Foreign => {
                self.fail("event named an unknown foreign window", event_loop);
                return;
            }
        }
        let Some(owner) = self.owner.as_mut() else {
            return;
        };
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

#[cfg(test)]
mod tests {
    use super::{SmokeWindowEventIdentity, classify_smoke_window_event};

    #[test]
    fn selected_retired_profile_and_unknown_window_events_remain_distinct() {
        let selected = 7_u8;
        let retired = [3_u8, 5_u8];
        assert_eq!(
            classify_smoke_window_event(&selected, &retired, &selected),
            SmokeWindowEventIdentity::Selected
        );
        assert_eq!(
            classify_smoke_window_event(&selected, &retired, &3),
            SmokeWindowEventIdentity::RetiredProfile
        );
        assert_eq!(
            classify_smoke_window_event(&selected, &retired, &11),
            SmokeWindowEventIdentity::Foreign
        );
    }
}
