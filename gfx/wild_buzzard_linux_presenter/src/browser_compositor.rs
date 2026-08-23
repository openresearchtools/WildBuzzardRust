#![forbid(unsafe_code)]

use std::error::Error;
use std::fmt;
use std::num::NonZeroU64;
use std::ops::Range;
use std::sync::Arc;

use webrender_api::units::{DeviceIntSize, LayoutPoint, LayoutRect, LayoutSize};
use webrender_api::{
    BuiltDisplayList, ColorF, CommonItemProperties, DisplayListBuilder, GlyphInstance, PipelineId,
    PrimitiveFlags, SpaceAndClipInfo,
};
use wild_buzzard_dom::DocumentVersion;
use wild_buzzard_platform::{PhysicalPoint, PhysicalSize};
use wild_buzzard_renderer::{CompiledScene, PipelineKey};
use wild_buzzard_text::{RunDirection, ShapedText};
use wild_buzzard_text_webrender::{PreparedSceneTextEntry, ShapedSceneText};

use crate::contract::{
    MAX_PRESENTATION_DIMENSION, MAX_PRESENTATION_PIXEL_BYTES, MAX_PRESENTATION_PIXELS,
};
use crate::primary_chrome::{
    BrowserChromeElementIdentity, BrowserElementAvailability, BrowserElementExpansion,
    BrowserElementInteraction, BrowserElementSelection, BrowserPrimaryChromeLayout,
    BrowserPrimaryChromeState, BrowserPrimaryControl, BrowserPrimaryControlKind,
    BrowserPrimaryPopupKind, BrowserPrimaryPopupRowKind, BrowserReloadStopMode,
    BrowserResolvedPrimaryControl, BrowserResolvedPrimaryPopup, BrowserResolvedPrimaryPopupRow,
    BrowserSiteIdentityKind, MAX_BROWSER_PRIMARY_CONTROLS, MAX_BROWSER_PRIMARY_POPUP_ROWS,
};
use crate::{
    WebRenderSurfaceSnapshot, WebRenderWindowError, WebRenderWindowErrorKind,
    WebRenderWindowFailureStage,
};

/// Maximum tabs retained by one compositor-authored chrome scene.
pub const MAX_BROWSER_CHROME_TABS: usize = 64;
/// Maximum shaped chrome text allocations in one scene.
pub const MAX_BROWSER_CHROME_TEXTS: usize =
    MAX_BROWSER_CHROME_TABS + 2 + MAX_BROWSER_PRIMARY_CONTROLS + MAX_BROWSER_PRIMARY_POPUP_ROWS;
/// Maximum aggregate source UTF-8 bytes retained by one chrome scene.
pub const MAX_BROWSER_CHROME_TEXT_BYTES: usize = 1 << 20;
/// Maximum shaped glyphs retained by one chrome scene.
pub const MAX_BROWSER_CHROME_GLYPHS: usize = 100_000;
/// Maximum shaped runs retained by one chrome scene.
pub const MAX_BROWSER_CHROME_RUNS: usize = 16_384;
/// Maximum serialized bytes in the compositor-authored chrome display list.
pub const MAX_BROWSER_CHROME_DISPLAY_LIST_BYTES: usize = 16 << 20;
/// Maximum serialized bytes in the compositor-authored root display list.
pub const MAX_BROWSER_ROOT_DISPLAY_LIST_BYTES: usize = 1 << 20;
/// Minimum nonempty framebuffer axis admitted by explicit browser capture.
pub const MIN_BROWSER_CAPTURE_DIMENSION: u32 = 2;
/// Maximum framebuffer axis admitted by explicit browser capture.
pub const MAX_BROWSER_CAPTURE_DIMENSION: u32 = MAX_PRESENTATION_DIMENSION;
/// Maximum pixels admitted by one explicit browser capture.
pub const MAX_BROWSER_CAPTURE_PIXELS: u64 = MAX_PRESENTATION_PIXELS;
/// Maximum bytes admitted by one tightly packed BGRA8 browser capture.
pub const MAX_BROWSER_CAPTURE_BYTES: u64 = MAX_PRESENTATION_PIXEL_BYTES;

const BROWSER_CAPTURE_BYTES_PER_PIXEL: usize = 4;

const TAB_STRIP_HEIGHT_CSS_PX: f64 = 36.0;
const ADDRESS_STRIP_HEIGHT_CSS_PX: f64 = 44.0;
const STATUS_HEIGHT_CSS_PX: f64 = 24.0;
const TAB_MAX_WIDTH_CSS_PX: f64 = 240.0;
const TAB_CLOSE_WIDTH_CSS_PX: f64 = 28.0;

/// Capability-neutral identity supplied by a browser shell for one navigation.
///
/// Graphics treats this as an opaque nonzero value. It grants no engine,
/// document, network, or renderer authority.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct BrowserNavigationIdentity(NonZeroU64);

impl BrowserNavigationIdentity {
    /// Creates an opaque identity. Zero is reserved.
    #[must_use]
    pub const fn new(value: u64) -> Option<Self> {
        match NonZeroU64::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    /// Numeric value for checked transport and shell-side revalidation.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// Capability-neutral identity supplied by a browser shell for one tab.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct BrowserTabIdentity(NonZeroU64);

impl BrowserTabIdentity {
    /// Creates an opaque identity. Zero is reserved.
    #[must_use]
    pub const fn new(value: u64) -> Option<Self> {
        match NonZeroU64::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    /// Numeric value for checked transport and shell-side revalidation.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// Never-reused browser-shell revision for an immutable page scene.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct BrowserPageSceneRevision(NonZeroU64);

impl BrowserPageSceneRevision {
    /// Creates a revision. Zero is reserved for the absence of a page.
    #[must_use]
    pub const fn new(value: u64) -> Option<Self> {
        match NonZeroU64::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    /// Numeric revision for checked transport.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// Never-reused browser-shell revision for one immutable chrome scene.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct BrowserChromeRevision(NonZeroU64);

impl BrowserChromeRevision {
    /// Creates a revision. Zero is reserved for an uninitialized compositor.
    #[must_use]
    pub const fn new(value: u64) -> Option<Self> {
        match NonZeroU64::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    /// Numeric revision for checked transport.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// Bounded physical rectangle in the native framebuffer coordinate space.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BrowserPhysicalRect {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

impl BrowserPhysicalRect {
    pub(crate) const fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// Horizontal origin in physical pixels.
    #[must_use]
    pub const fn x(self) -> u32 {
        self.x
    }

    /// Vertical origin in physical pixels.
    #[must_use]
    pub const fn y(self) -> u32 {
        self.y
    }

    /// Width in physical pixels.
    #[must_use]
    pub const fn width(self) -> u32 {
        self.width
    }

    /// Height in physical pixels.
    #[must_use]
    pub const fn height(self) -> u32 {
        self.height
    }

    /// Exact extent when both axes are nonzero.
    ///
    /// Clipped chrome rectangles may legitimately be empty on a tiny surface;
    /// those return `None` instead of fabricating a drawable extent.
    #[must_use]
    pub fn size(self) -> Option<PhysicalSize> {
        if self.width == 0 || self.height == 0 {
            return None;
        }
        PhysicalSize::new(self.width, self.height).ok()
    }

    pub(crate) fn contains(self, point: PhysicalPoint) -> bool {
        let Ok(x) = u32::try_from(point.x) else {
            return false;
        };
        let Ok(y) = u32::try_from(point.y) else {
            return false;
        };
        x >= self.x
            && y >= self.y
            && x < self.x.saturating_add(self.width)
            && y < self.y.saturating_add(self.height)
    }

    pub(crate) fn local_point(self, point: PhysicalPoint) -> PhysicalPoint {
        let x = i64::from(point.x) - i64::from(self.x);
        let y = i64::from(point.y) - i64::from(self.y);
        PhysicalPoint {
            x: i32::try_from(x).expect("bounded local x fits i32"),
            y: i32::try_from(y).expect("bounded local y fits i32"),
        }
    }
}

/// Deterministic physical chrome and page geometry for one exact surface.
///
/// The current compositor uses physical `WebRender` coordinates. A scale change
/// therefore creates a new surface revision and new geometry; it never relabels
/// the old pixels as if they had been rendered at the new scale.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BrowserChromeGeometry {
    surface: WebRenderSurfaceSnapshot,
    tab_strip: BrowserPhysicalRect,
    address_strip: BrowserPhysicalRect,
    address_field: BrowserPhysicalRect,
    content: BrowserPhysicalRect,
    status: BrowserPhysicalRect,
    tab_max_width: u32,
    tab_close_width: u32,
}

impl BrowserChromeGeometry {
    /// Computes fixed Rust-authored chrome geometry for an exact surface.
    ///
    /// # Errors
    ///
    /// Rejects a suspended/zero-sized surface or an unrepresentable scale.
    pub fn for_surface(surface: WebRenderSurfaceSnapshot) -> Result<Self, WebRenderWindowError> {
        let size = surface.size();
        if size.width == 0 || size.height == 0 {
            return Err(WebRenderWindowError::new(
                WebRenderWindowFailureStage::ValidateRequest,
                WebRenderWindowErrorKind::Suspended,
                "cannot build browser chrome for a zero-sized surface",
            ));
        }
        let scale = surface.descriptor().scale.get();
        let tab_height = scaled_axis(TAB_STRIP_HEIGHT_CSS_PX, scale)?;
        let address_height = scaled_axis(ADDRESS_STRIP_HEIGHT_CSS_PX, scale)?;
        let chrome_height = tab_height.saturating_add(address_height).min(size.height);
        let tab_height = tab_height.min(chrome_height);
        let address_height = chrome_height.saturating_sub(tab_height);
        let inset = scaled_axis(6.0, scale)?.min(size.width / 2);
        let address_field = BrowserPhysicalRect::new(
            inset,
            tab_height.saturating_add(inset.min(address_height / 2)),
            size.width.saturating_sub(inset.saturating_mul(2)),
            address_height.saturating_sub(inset.min(address_height / 2).saturating_mul(2)),
        );
        let status_height = scaled_axis(STATUS_HEIGHT_CSS_PX, scale)?
            .min(size.height.saturating_sub(chrome_height));
        Ok(Self {
            surface,
            tab_strip: BrowserPhysicalRect::new(0, 0, size.width, tab_height),
            address_strip: BrowserPhysicalRect::new(0, tab_height, size.width, address_height),
            address_field,
            content: BrowserPhysicalRect::new(
                0,
                chrome_height,
                size.width,
                size.height.saturating_sub(chrome_height),
            ),
            status: BrowserPhysicalRect::new(
                0,
                size.height.saturating_sub(status_height),
                size.width,
                status_height,
            ),
            tab_max_width: scaled_axis(TAB_MAX_WIDTH_CSS_PX, scale)?,
            tab_close_width: scaled_axis(TAB_CLOSE_WIDTH_CSS_PX, scale)?,
        })
    }

    /// Exact surface identity from which this geometry was derived.
    #[must_use]
    pub const fn surface(self) -> WebRenderSurfaceSnapshot {
        self.surface
    }

    /// Tab-strip rectangle.
    #[must_use]
    pub const fn tab_strip(self) -> BrowserPhysicalRect {
        self.tab_strip
    }

    /// Address-strip background rectangle.
    #[must_use]
    pub const fn address_strip(self) -> BrowserPhysicalRect {
        self.address_strip
    }

    /// Editable address-field rectangle.
    #[must_use]
    pub const fn address_field(self) -> BrowserPhysicalRect {
        self.address_field
    }

    /// Page-content viewport below first-party chrome.
    #[must_use]
    pub const fn content(self) -> BrowserPhysicalRect {
        self.content
    }

    /// Status overlay rectangle at the bottom of the native window.
    #[must_use]
    pub const fn status(self) -> BrowserPhysicalRect {
        self.status
    }

    pub(crate) fn tab_rect(self, index: usize, count: usize) -> BrowserPhysicalRect {
        if count == 0 || self.tab_strip.width == 0 {
            return BrowserPhysicalRect::new(0, 0, 0, self.tab_strip.height);
        }
        let count = u32::try_from(count).expect("tab count is hard bounded");
        let available = self.tab_strip.width;
        let usable = available.min(self.tab_max_width.saturating_mul(count));
        let base_width = usable / count;
        let remainder = usable % count;
        let index = u32::try_from(index).expect("tab index is hard bounded");
        let x = base_width
            .saturating_mul(index)
            .saturating_add(index.min(remainder));
        let width = base_width + u32::from(index < remainder);
        BrowserPhysicalRect::new(
            x,
            0,
            width.min(available.saturating_sub(x)),
            self.tab_strip.height,
        )
    }

    pub(crate) fn tab_close_rect(self, tab: BrowserPhysicalRect) -> BrowserPhysicalRect {
        let width = if tab.width <= 1 {
            0
        } else {
            self.tab_close_width
                .min((tab.width / 3).max(1))
                .min(tab.width - 1)
        };
        BrowserPhysicalRect::new(
            tab.x.saturating_add(tab.width.saturating_sub(width)),
            tab.y,
            width,
            tab.height,
        )
    }
}

pub(crate) fn scaled_axis(css_px: f64, scale: f64) -> Result<u32, WebRenderWindowError> {
    let value = css_px * scale;
    if !value.is_finite() || value <= 0.0 || value > f64::from(u32::MAX) {
        return Err(WebRenderWindowError::new(
            WebRenderWindowFailureStage::ValidateRequest,
            WebRenderWindowErrorKind::SizeMismatch,
            "scaled browser chrome geometry is unrepresentable",
        ));
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let rounded = value.round() as u32;
    Ok(rounded.max(1))
}

/// Exact immutable page publication identity retained by the compositor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BrowserPageIdentity {
    navigation: BrowserNavigationIdentity,
    revision: BrowserPageSceneRevision,
    document: DocumentVersion,
    pipeline: PipelineKey,
}

impl BrowserPageIdentity {
    /// Capability-neutral navigation identity.
    #[must_use]
    pub const fn navigation(self) -> BrowserNavigationIdentity {
        self.navigation
    }

    /// Never-reused page-scene revision.
    #[must_use]
    pub const fn revision(self) -> BrowserPageSceneRevision {
        self.revision
    }

    /// Exact DOM document identity and local revision.
    #[must_use]
    pub const fn document_version(self) -> DocumentVersion {
        self.document
    }

    /// Renderer-independent page pipeline identity.
    #[must_use]
    pub const fn pipeline(self) -> PipelineKey {
        self.pipeline
    }
}

/// Exact page state represented by a composition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BrowserPageSnapshot {
    /// No page pipeline is live. Chrome may still be presented.
    Blank,
    /// One exact immutable page scene is live.
    Scene(BrowserPageIdentity),
}

/// Owned immutable page scene and its canonical shaped-text inventory.
///
/// Construction performs identity checks only. Renderer resource staging and
/// full scene text validation remain transactional inside the presenter.
pub struct BrowserPageScene {
    identity: BrowserPageIdentity,
    scene: CompiledScene,
    texts: Box<[ShapedSceneText]>,
}

impl BrowserPageScene {
    /// Binds a browser navigation and never-reused scene revision to an owned
    /// compiled page and its complete canonical shaped text.
    ///
    /// # Errors
    ///
    /// Rejects a missing, foreign, duplicate, or reordered shaped-text entry.
    pub fn new(
        navigation: BrowserNavigationIdentity,
        revision: BrowserPageSceneRevision,
        scene: CompiledScene,
        texts: Box<[ShapedSceneText]>,
    ) -> Result<Self, WebRenderWindowError> {
        let document = scene.document_version();
        let expected = scene.scene().pending_text().len();
        if texts.len() != expected {
            return Err(browser_contract_error(
                WebRenderWindowErrorKind::Text,
                "page shaped-text count differs from its compiled pending inventory",
            ));
        }
        for (index, text) in texts.iter().enumerate() {
            let expected_index = u32::try_from(index).map_err(|_| {
                browser_contract_error(
                    WebRenderWindowErrorKind::ResourceLimit,
                    "page shaped-text index exceeds u32 capacity",
                )
            })?;
            if text.document_version() != document || text.pending_index() != expected_index {
                return Err(browser_contract_error(
                    WebRenderWindowErrorKind::Text,
                    "page shaped-text inventory is foreign, missing, duplicate, or reordered",
                ));
            }
        }
        let identity = BrowserPageIdentity {
            navigation,
            revision,
            document,
            pipeline: scene.pipeline(),
        };
        Ok(Self {
            identity,
            scene,
            texts,
        })
    }

    /// Exact publication identity.
    #[must_use]
    pub const fn identity(&self) -> BrowserPageIdentity {
        self.identity
    }

    pub(crate) fn scene(&self) -> &CompiledScene {
        &self.scene
    }

    pub(crate) fn texts(&self) -> &[ShapedSceneText] {
        &self.texts
    }

    pub(crate) fn into_parts(self) -> (BrowserPageIdentity, CompiledScene, Box<[ShapedSceneText]>) {
        (self.identity, self.scene, self.texts)
    }
}

/// Candidate page transition for one browser frame.
// The owned scene is deliberately inline: page updates are consumed exactly
// once and this avoids adding an infallible heap allocation at the admission
// boundary solely to shrink a short-lived control value.
#[allow(clippy::large_enum_variant)]
pub enum BrowserPageUpdate {
    /// Reuse the exact currently retained Blank or Scene state.
    Retain,
    /// Atomically replace the page pipeline with this owned immutable scene.
    Install(BrowserPageScene),
    /// Atomically stop referencing and retire the current page pipeline.
    ClearToBlank,
}

/// Immutable shaped label and opaque identity for one tab.
#[derive(Clone, Debug)]
pub struct BrowserChromeTab {
    identity: BrowserTabIdentity,
    title: Arc<ShapedText>,
    loading: bool,
    interaction: BrowserElementInteraction,
    close_availability: BrowserElementAvailability,
    close_interaction: BrowserElementInteraction,
}

impl BrowserChromeTab {
    /// Creates one tab label from a normal Rust text-system shape.
    #[must_use]
    pub fn new(identity: BrowserTabIdentity, title: Arc<ShapedText>) -> Self {
        Self {
            identity,
            title,
            loading: false,
            interaction: BrowserElementInteraction::Idle,
            close_availability: BrowserElementAvailability::Enabled,
            close_interaction: BrowserElementInteraction::Idle,
        }
    }

    /// Marks whether the tab has a pending load.
    #[must_use]
    pub const fn with_loading(mut self, loading: bool) -> Self {
        self.loading = loading;
        self
    }

    /// Adds the exact browser-owned hover/press state for the tab body.
    #[must_use]
    pub const fn with_interaction(mut self, interaction: BrowserElementInteraction) -> Self {
        self.interaction = interaction;
        self
    }

    /// Adds the exact availability and hover/press state for this tab's close
    /// action. A disabled close action must remain idle.
    #[must_use]
    pub const fn with_close_state(
        mut self,
        availability: BrowserElementAvailability,
        interaction: BrowserElementInteraction,
    ) -> Self {
        self.close_availability = availability;
        self.close_interaction = interaction;
        self
    }

    /// Opaque tab identity.
    #[must_use]
    pub const fn identity(&self) -> BrowserTabIdentity {
        self.identity
    }

    /// Exact shaped title.
    #[must_use]
    pub const fn title(&self) -> &Arc<ShapedText> {
        &self.title
    }

    /// Whether the browser reported a pending load.
    #[must_use]
    pub const fn loading(&self) -> bool {
        self.loading
    }

    /// Exact browser-owned hover/press state for the tab body.
    #[must_use]
    pub const fn interaction(&self) -> BrowserElementInteraction {
        self.interaction
    }

    /// Whether the exact tab-close action is currently available.
    #[must_use]
    pub const fn close_availability(&self) -> BrowserElementAvailability {
        self.close_availability
    }

    /// Exact browser-owned hover/press state for the tab-close action.
    #[must_use]
    pub const fn close_interaction(&self) -> BrowserElementInteraction {
        self.close_interaction
    }
}

/// UTF-8 byte selection in the exact shaped address string.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BrowserAddressSelection {
    anchor: usize,
    focus: usize,
}

impl BrowserAddressSelection {
    /// Creates a selection. Bounds and UTF-8 boundaries are checked when the
    /// containing chrome scene is constructed.
    #[must_use]
    pub const fn new(anchor: usize, focus: usize) -> Self {
        Self { anchor, focus }
    }

    /// Anchor byte offset.
    #[must_use]
    pub const fn anchor(self) -> usize {
        self.anchor
    }

    /// Focus byte offset.
    #[must_use]
    pub const fn focus(self) -> usize {
        self.focus
    }

    pub(crate) fn normalized(self) -> Range<usize> {
        self.anchor.min(self.focus)..self.anchor.max(self.focus)
    }
}

/// Browser-owned focus state rendered by first-party chrome.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BrowserChromeFocus {
    /// No visible focus ring.
    None,
    /// One exact tab has focus.
    Tab(BrowserTabIdentity),
    /// The address editor has focus.
    AddressBar,
    /// The page viewport has focus.
    Page,
    /// One exact primary control has focus. URL editing uses `AddressBar`.
    PrimaryControl(BrowserChromeElementIdentity),
    /// One exact visible row in the sole open primary popup has focus.
    PopupRow(BrowserChromeElementIdentity),
}

/// Browser state from which one immutable chrome scene is authored.
#[derive(Clone, Debug)]
pub struct BrowserChromeState {
    tabs: Box<[BrowserChromeTab]>,
    active_tab: Option<BrowserTabIdentity>,
    address: Arc<ShapedText>,
    address_selection: BrowserAddressSelection,
    status: Option<Arc<ShapedText>>,
    focus: BrowserChromeFocus,
    primary: Option<BrowserPrimaryChromeState>,
}

impl BrowserChromeState {
    /// Creates the required tab/address portion of browser chrome.
    #[must_use]
    pub fn new(
        tabs: Box<[BrowserChromeTab]>,
        active_tab: Option<BrowserTabIdentity>,
        address: Arc<ShapedText>,
    ) -> Self {
        let caret = address.text().len();
        Self {
            tabs,
            active_tab,
            address,
            address_selection: BrowserAddressSelection::new(caret, caret),
            status: None,
            focus: BrowserChromeFocus::None,
            primary: None,
        }
    }

    /// Adds an exact address selection.
    #[must_use]
    pub const fn with_address_selection(mut self, selection: BrowserAddressSelection) -> Self {
        self.address_selection = selection;
        self
    }

    /// Adds or removes the shaped status overlay text.
    #[must_use]
    pub fn with_status(mut self, status: Option<Arc<ShapedText>>) -> Self {
        self.status = status;
        self
    }

    /// Sets the visible focus surface.
    #[must_use]
    pub const fn with_focus(mut self, focus: BrowserChromeFocus) -> Self {
        self.focus = focus;
        self
    }

    /// Installs or removes the immutable browser-session primary UI projection.
    #[must_use]
    pub fn with_primary_chrome(mut self, primary: Option<BrowserPrimaryChromeState>) -> Self {
        self.primary = primary;
        self
    }
}

/// Immutable, independently revisioned, Rust-authored browser chrome scene.
#[derive(Clone, Debug)]
pub struct BrowserChromeScene {
    revision: BrowserChromeRevision,
    geometry: BrowserChromeGeometry,
    state: BrowserChromeState,
    primary_layout: Option<BrowserPrimaryChromeLayout>,
    text_count: usize,
    text_bytes: usize,
    run_count: usize,
    glyph_count: usize,
}

impl BrowserChromeScene {
    /// Validates and freezes one chrome scene for an exact native surface.
    ///
    /// # Errors
    ///
    /// Rejects unbounded/duplicate tabs, foreign focus, invalid UTF-8
    /// selection boundaries, malformed shaped metrics, and suspended geometry.
    #[allow(clippy::too_many_lines)]
    pub fn new(
        revision: BrowserChromeRevision,
        surface: WebRenderSurfaceSnapshot,
        state: BrowserChromeState,
    ) -> Result<Self, WebRenderWindowError> {
        let geometry = BrowserChromeGeometry::for_surface(surface)?;
        if state.tabs.len() > MAX_BROWSER_CHROME_TABS {
            return Err(browser_resource_error(
                "chrome tab count exceeds its fixed limit",
            ));
        }
        for (index, tab) in state.tabs.iter().enumerate() {
            if state.tabs[..index]
                .iter()
                .any(|prior| prior.identity == tab.identity)
            {
                return Err(browser_contract_error(
                    WebRenderWindowErrorKind::Contract,
                    "chrome contains duplicate opaque tab identities",
                ));
            }
            if tab.close_availability == BrowserElementAvailability::Disabled
                && tab.close_interaction != BrowserElementInteraction::Idle
            {
                return Err(browser_contract_error(
                    WebRenderWindowErrorKind::Contract,
                    "disabled tab-close action cannot be hovered or pressed",
                ));
            }
        }
        if state
            .active_tab
            .is_some_and(|active| !state.tabs.iter().any(|tab| tab.identity == active))
        {
            return Err(browser_contract_error(
                WebRenderWindowErrorKind::Contract,
                "active tab identity is absent from the chrome tab inventory",
            ));
        }
        if let BrowserChromeFocus::Tab(focused) = state.focus
            && !state.tabs.iter().any(|tab| tab.identity == focused)
        {
            return Err(browser_contract_error(
                WebRenderWindowErrorKind::Contract,
                "focused tab identity is absent from the chrome tab inventory",
            ));
        }
        let address_text = state.address.text();
        for offset in [
            state.address_selection.anchor,
            state.address_selection.focus,
        ] {
            if offset > address_text.len() || !address_text.is_char_boundary(offset) {
                return Err(browser_contract_error(
                    WebRenderWindowErrorKind::Text,
                    "address selection is outside an exact UTF-8 boundary",
                ));
            }
        }

        if state.primary.is_none()
            && matches!(
                state.focus,
                BrowserChromeFocus::PrimaryControl(_) | BrowserChromeFocus::PopupRow(_)
            )
        {
            return Err(browser_contract_error(
                WebRenderWindowErrorKind::Contract,
                "primary control or popup focus exists without a primary chrome projection",
            ));
        }
        let primary_layout = state
            .primary
            .as_ref()
            .map(|primary| {
                BrowserPrimaryChromeLayout::resolve(
                    geometry,
                    &state.tabs,
                    state.active_tab,
                    state.focus,
                    primary,
                )
            })
            .transpose()?;

        let primary_control_texts = state
            .primary
            .as_ref()
            .map_or(0, |primary| primary.controls().len());
        let primary_popup_texts = primary_layout
            .as_ref()
            .and_then(BrowserPrimaryChromeLayout::popup)
            .map_or(0, |popup| popup.rows().len());
        let text_count = state
            .tabs
            .len()
            .checked_add(1 + usize::from(state.status.is_some()))
            .and_then(|count| count.checked_add(primary_control_texts))
            .and_then(|count| count.checked_add(primary_popup_texts))
            .ok_or_else(|| browser_resource_error("chrome shaped-text count overflowed"))?;
        if text_count > MAX_BROWSER_CHROME_TEXTS {
            return Err(browser_resource_error(
                "chrome shaped-text count exceeds its fixed limit",
            ));
        }
        let mut text_bytes = 0_usize;
        let mut run_count = 0_usize;
        let mut glyph_count = 0_usize;
        for shaped in
            state
                .tabs
                .iter()
                .map(|tab| &tab.title)
                .chain(std::iter::once(&state.address))
                .chain(state.status.iter())
                .chain(state.primary.iter().flat_map(|primary| {
                    primary.controls().iter().map(BrowserPrimaryControl::label)
                }))
                .chain(
                    primary_layout
                        .iter()
                        .filter_map(BrowserPrimaryChromeLayout::popup)
                        .flat_map(|popup| {
                            popup
                                .rows()
                                .iter()
                                .map(BrowserResolvedPrimaryPopupRow::label)
                        }),
                )
        {
            validate_shaped_chrome_text(shaped)?;
            text_bytes = text_bytes
                .checked_add(shaped.text().len())
                .ok_or_else(|| browser_resource_error("chrome UTF-8 byte accounting overflowed"))?;
            run_count = run_count
                .checked_add(shaped.runs().len())
                .ok_or_else(|| browser_resource_error("chrome run accounting overflowed"))?;
            for run in shaped.runs() {
                glyph_count = glyph_count
                    .checked_add(run.glyphs().len())
                    .ok_or_else(|| browser_resource_error("chrome glyph accounting overflowed"))?;
            }
        }
        if text_bytes > MAX_BROWSER_CHROME_TEXT_BYTES
            || run_count > MAX_BROWSER_CHROME_RUNS
            || glyph_count > MAX_BROWSER_CHROME_GLYPHS
        {
            return Err(browser_resource_error(
                "chrome text bytes, runs, or glyphs exceed fixed limits",
            ));
        }
        Ok(Self {
            revision,
            geometry,
            state,
            primary_layout,
            text_count,
            text_bytes,
            run_count,
            glyph_count,
        })
    }

    /// Exact independently monotone chrome revision.
    #[must_use]
    pub const fn revision(&self) -> BrowserChromeRevision {
        self.revision
    }

    /// Exact native surface geometry represented by this scene.
    #[must_use]
    pub const fn geometry(&self) -> BrowserChromeGeometry {
        self.geometry
    }

    /// Frozen resolved primary layout, including exact overflow inventory.
    #[must_use]
    pub const fn primary_layout(&self) -> Option<&BrowserPrimaryChromeLayout> {
        self.primary_layout.as_ref()
    }

    /// Bounded shaped text count.
    #[must_use]
    pub const fn text_count(&self) -> usize {
        self.text_count
    }

    /// Aggregate source UTF-8 bytes.
    #[must_use]
    pub const fn text_bytes(&self) -> usize {
        self.text_bytes
    }

    /// Aggregate shaped run count.
    #[must_use]
    pub const fn run_count(&self) -> usize {
        self.run_count
    }

    /// Aggregate positioned glyph count.
    #[must_use]
    pub const fn glyph_count(&self) -> usize {
        self.glyph_count
    }

    pub(crate) fn shaped_texts(&self) -> impl Iterator<Item = &Arc<ShapedText>> {
        self.state
            .tabs
            .iter()
            .map(|tab| &tab.title)
            .chain(std::iter::once(&self.state.address))
            .chain(self.state.status.iter())
            .chain(
                self.state.primary.iter().flat_map(|primary| {
                    primary.controls().iter().map(BrowserPrimaryControl::label)
                }),
            )
            .chain(
                self.primary_layout
                    .iter()
                    .filter_map(BrowserPrimaryChromeLayout::popup)
                    .flat_map(|popup| {
                        popup
                            .rows()
                            .iter()
                            .map(BrowserResolvedPrimaryPopupRow::label)
                    }),
            )
    }

    pub(crate) fn hit_map(&self) -> BrowserChromeHitMap {
        let mut tabs = Vec::with_capacity(self.state.tabs.len());
        for (index, tab) in self.state.tabs.iter().enumerate() {
            let rect = self.tab_rect(index);
            tabs.push(BrowserTabHitRegion {
                identity: tab.identity,
                rect,
                close: self.tab_close_rect(index, rect),
            });
        }
        BrowserChromeHitMap {
            geometry: self.geometry,
            tabs: tabs.into_boxed_slice(),
            status_visible: self.state.status.is_some(),
            primary: self.primary_layout.clone(),
        }
    }

    pub(crate) fn tab_rect(&self, index: usize) -> BrowserPhysicalRect {
        self.primary_layout.as_ref().map_or_else(
            || self.geometry.tab_rect(index, self.state.tabs.len()),
            |layout| layout.preview().tab_rects()[index],
        )
    }

    pub(crate) fn tab_close_rect(
        &self,
        index: usize,
        tab: BrowserPhysicalRect,
    ) -> BrowserPhysicalRect {
        self.primary_layout.as_ref().map_or_else(
            || self.geometry.tab_close_rect(tab),
            |layout| layout.preview().tab_close_rects()[index],
        )
    }

    pub(crate) fn tab_title_rect(
        &self,
        index: usize,
        tab: BrowserPhysicalRect,
        close: BrowserPhysicalRect,
    ) -> BrowserPhysicalRect {
        self.primary_layout.as_ref().map_or_else(
            || inset_right(tab, close.width),
            |layout| layout.preview().tab_title_rects()[index],
        )
    }

    pub(crate) fn address_field(&self) -> BrowserPhysicalRect {
        self.primary_layout.as_ref().map_or_else(
            || self.geometry.address_field,
            |layout| layout.preview().address_field(),
        )
    }
}

pub(crate) fn validate_shaped_chrome_text(shaped: &ShapedText) -> Result<(), WebRenderWindowError> {
    let metrics = shaped.metrics();
    for value in [
        metrics.width(),
        metrics.full_width(),
        metrics.height(),
        metrics.first_baseline(),
        metrics.ascent(),
        metrics.descent(),
        metrics.line_height(),
    ] {
        if !value.is_finite() {
            return Err(browser_contract_error(
                WebRenderWindowErrorKind::Text,
                "chrome shaped text contains non-finite metrics",
            ));
        }
    }
    Ok(())
}

/// Exact immutable browser composition request.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BrowserFrameRequest {
    surface: WebRenderSurfaceSnapshot,
    page: BrowserPageSnapshot,
    chrome_revision: BrowserChromeRevision,
    epoch: u32,
    sequence: u64,
}

impl BrowserFrameRequest {
    /// Binds page and chrome publication identities to one exact surface,
    /// `WebRender` root epoch, and native swap sequence.
    #[must_use]
    pub const fn new(
        surface: WebRenderSurfaceSnapshot,
        page: BrowserPageSnapshot,
        chrome_revision: BrowserChromeRevision,
        epoch: u32,
        sequence: u64,
    ) -> Self {
        Self {
            surface,
            page,
            chrome_revision,
            epoch,
            sequence,
        }
    }

    /// Exact native target snapshot.
    #[must_use]
    pub const fn surface(self) -> WebRenderSurfaceSnapshot {
        self.surface
    }

    /// Exact Blank-or-Scene page identity expected after publication.
    #[must_use]
    pub const fn page(self) -> BrowserPageSnapshot {
        self.page
    }

    /// Exact independently revisioned chrome expected after publication.
    #[must_use]
    pub const fn chrome_revision(self) -> BrowserChromeRevision {
        self.chrome_revision
    }

    /// Strictly increasing root-composition epoch.
    #[must_use]
    pub const fn epoch(self) -> u32 {
        self.epoch
    }

    /// Strictly increasing native swap sequence.
    #[must_use]
    pub const fn sequence(self) -> u64 {
        self.sequence
    }
}

/// Evidence returned only for one successfully published browser composition.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BrowserFrameReceipt {
    request: BrowserFrameRequest,
    page_epoch: Option<u32>,
    chrome_epoch: u32,
    backend_publish_id: u64,
    rgba8_byte_equivalent: u64,
    page_display_list_bytes: usize,
    chrome_display_list_bytes: usize,
    root_display_list_bytes: usize,
    chrome_primitives: usize,
}

impl BrowserFrameReceipt {
    /// Exact successful request, including surface/page/chrome/root/swap identity.
    #[must_use]
    pub const fn request(self) -> BrowserFrameRequest {
        self.request
    }

    /// Epoch of the live page pipeline, absent only for Blank.
    #[must_use]
    pub const fn page_epoch(self) -> Option<u32> {
        self.page_epoch
    }

    /// Epoch at which the currently live chrome pipeline was last replaced.
    #[must_use]
    pub const fn chrome_epoch(self) -> u32 {
        self.chrome_epoch
    }

    /// Root pipeline epoch; equal to [`BrowserFrameRequest::epoch`].
    #[must_use]
    pub const fn root_epoch(self) -> u32 {
        self.request.epoch
    }

    /// Nonzero `WebRender` backend publish identity.
    #[must_use]
    pub const fn backend_publish_id(self) -> u64 {
        self.backend_publish_id
    }

    /// Bounded RGBA8-equivalent framebuffer byte count; no pixels are copied.
    #[must_use]
    pub const fn rgba8_byte_equivalent(self) -> u64 {
        self.rgba8_byte_equivalent
    }

    /// Serialized bytes last submitted for the live page pipeline.
    #[must_use]
    pub const fn page_display_list_bytes(self) -> usize {
        self.page_display_list_bytes
    }

    /// Serialized bytes last submitted for the live chrome pipeline.
    #[must_use]
    pub const fn chrome_display_list_bytes(self) -> usize {
        self.chrome_display_list_bytes
    }

    /// Serialized bytes in this exact root composition.
    #[must_use]
    pub const fn root_display_list_bytes(self) -> usize {
        self.root_display_list_bytes
    }

    /// Bounded Rust-authored chrome primitive count.
    #[must_use]
    pub const fn chrome_primitives(self) -> usize {
        self.chrome_primitives
    }

    /// `WebRender` built and the renderer submitted this exact transaction.
    #[must_use]
    pub const fn renderer_frame_submitted(self) -> bool {
        true
    }

    /// EGL accepted the exact native swap.
    #[must_use]
    pub const fn egl_swap_submitted(self) -> bool {
        true
    }

    /// This boundary has no desktop-compositor display acknowledgement.
    #[must_use]
    pub const fn desktop_compositor_acknowledged(self) -> bool {
        false
    }
}

/// Failure while copying a checked content crop into caller-owned storage.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BrowserCaptureCopyError {
    /// The destination stride cannot hold one complete crop row.
    StrideTooSmall {
        /// Minimum bytes required for one crop row.
        minimum: usize,
        /// Supplied destination stride.
        supplied: usize,
    },
    /// The destination length is not exact for the supplied stride and crop.
    LengthMismatch {
        /// Exact required destination byte length.
        required: usize,
        /// Supplied destination byte length.
        supplied: usize,
    },
}

impl fmt::Display for BrowserCaptureCopyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::StrideTooSmall { minimum, supplied } => write!(
                formatter,
                "capture destination stride {supplied} is smaller than {minimum}"
            ),
            Self::LengthMismatch { required, supplied } => write!(
                formatter,
                "capture destination length {supplied} differs from exact length {required}"
            ),
        }
    }
}

impl Error for BrowserCaptureCopyError {}

/// Borrowed checked view of the physical page-content crop in one capture.
///
/// Rows and pixels use the same top-left physical coordinate system as
/// [`BrowserChromeGeometry`]. Every pixel is four raw bytes in B, G, R, A
/// order. No color conversion, alpha unpremultiplication, or scanout claim is
/// applied by this view.
#[derive(Clone, Copy, Debug)]
pub struct BrowserBgra8Crop<'a> {
    pixels: &'a [u8],
    full_stride: usize,
    rect: BrowserPhysicalRect,
    row_bytes: usize,
}

impl BrowserBgra8Crop<'_> {
    /// Exact physical crop rectangle in the full compositor framebuffer.
    #[must_use]
    pub const fn rect(&self) -> BrowserPhysicalRect {
        self.rect
    }

    /// Bytes occupied by one tightly packed crop row.
    #[must_use]
    pub const fn row_bytes(&self) -> usize {
        self.row_bytes
    }

    /// Number of physical rows in the crop.
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.rect.height
    }

    /// Returns one top-left-ordered tightly packed crop row.
    #[must_use]
    pub fn row(&self, row: u32) -> Option<&[u8]> {
        if row >= self.rect.height {
            return None;
        }
        let y = usize::try_from(self.rect.y.checked_add(row)?).ok()?;
        let x = usize::try_from(self.rect.x).ok()?;
        let start = y
            .checked_mul(self.full_stride)?
            .checked_add(x.checked_mul(BROWSER_CAPTURE_BYTES_PER_PIXEL)?)?;
        let end = start.checked_add(self.row_bytes)?;
        self.pixels.get(start..end)
    }

    /// Copies the crop into an exact caller-owned row-strided destination.
    ///
    /// The destination must have exactly `stride * (height - 1) + row_bytes`
    /// bytes. Padding bytes between rows are left unchanged.
    ///
    /// # Errors
    ///
    /// Rejects a short stride, arithmetic overflow, or any inexact length.
    pub fn copy_to(
        &self,
        destination: &mut [u8],
        stride: usize,
    ) -> Result<(), BrowserCaptureCopyError> {
        let supplied_length = destination.len();
        if stride < self.row_bytes {
            return Err(BrowserCaptureCopyError::StrideTooSmall {
                minimum: self.row_bytes,
                supplied: stride,
            });
        }
        let rows_before_last =
            usize::try_from(self.rect.height.saturating_sub(1)).map_err(|_| {
                BrowserCaptureCopyError::LengthMismatch {
                    required: usize::MAX,
                    supplied: supplied_length,
                }
            })?;
        let required = stride
            .checked_mul(rows_before_last)
            .and_then(|bytes| bytes.checked_add(self.row_bytes))
            .ok_or(BrowserCaptureCopyError::LengthMismatch {
                required: usize::MAX,
                supplied: supplied_length,
            })?;
        if supplied_length != required {
            return Err(BrowserCaptureCopyError::LengthMismatch {
                required,
                supplied: supplied_length,
            });
        }
        for row in 0..self.rect.height {
            let source = self
                .row(row)
                .ok_or(BrowserCaptureCopyError::LengthMismatch {
                    required,
                    supplied: supplied_length,
                })?;
            let start = usize::try_from(row)
                .ok()
                .and_then(|row| row.checked_mul(stride))
                .ok_or(BrowserCaptureCopyError::LengthMismatch {
                    required,
                    supplied: supplied_length,
                })?;
            let end = start.checked_add(self.row_bytes).ok_or(
                BrowserCaptureCopyError::LengthMismatch {
                    required,
                    supplied: supplied_length,
                },
            )?;
            let destination_row =
                destination
                    .get_mut(start..end)
                    .ok_or(BrowserCaptureCopyError::LengthMismatch {
                        required,
                        supplied: supplied_length,
                    })?;
            destination_row.copy_from_slice(source);
        }
        Ok(())
    }
}

/// One exact receipt-bound full compositor capture.
///
/// Construction is private to the presenter and occurs only after the exact
/// mapped frame has passed identity checks and EGL has accepted that frame's
/// swap. The sole owned pixel allocation is tightly packed top-left BGRA8.
/// Alpha is the raw default-framebuffer alpha byte returned by pinned
/// `WebRender`; it is not normalized or unpremultiplied. The bytes do not prove
/// that a desktop compositor displayed the buffer.
pub struct BrowserFrameCapture {
    receipt: BrowserFrameReceipt,
    size: PhysicalSize,
    stride: usize,
    content: BrowserPhysicalRect,
    content_row_bytes: usize,
    pixels: Vec<u8>,
}

impl BrowserFrameCapture {
    /// Exact successful browser receipt inseparable from these pixels.
    #[must_use]
    pub const fn receipt(&self) -> BrowserFrameReceipt {
        self.receipt
    }

    /// Exact full physical compositor extent.
    #[must_use]
    pub const fn size(&self) -> PhysicalSize {
        self.size
    }

    /// Exact tightly packed full-frame stride in bytes.
    #[must_use]
    pub const fn stride(&self) -> usize {
        self.stride
    }

    /// Exact full-frame top-left BGRA8 bytes.
    #[must_use]
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    /// Exact page-content crop in the full physical compositor framebuffer.
    #[must_use]
    pub const fn content_rect(&self) -> BrowserPhysicalRect {
        self.content
    }

    /// Checked zero-copy view of the page-content crop.
    #[must_use]
    pub fn content(&self) -> BrowserBgra8Crop<'_> {
        BrowserBgra8Crop {
            pixels: &self.pixels,
            full_stride: self.stride,
            rect: self.content,
            row_bytes: self.content_row_bytes,
        }
    }

    /// Returns one full top-left-ordered tightly packed framebuffer row.
    #[must_use]
    pub fn row(&self, row: u32) -> Option<&[u8]> {
        if row >= self.size.height {
            return None;
        }
        let start = usize::try_from(row).ok()?.checked_mul(self.stride)?;
        let end = start.checked_add(self.stride)?;
        self.pixels.get(start..end)
    }

    /// This boundary has no desktop-compositor display acknowledgement.
    #[must_use]
    pub const fn desktop_compositor_acknowledged(&self) -> bool {
        false
    }
}

impl fmt::Debug for BrowserFrameCapture {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BrowserFrameCapture")
            .field("receipt", &self.receipt)
            .field("size", &self.size)
            .field("stride", &self.stride)
            .field("content", &self.content)
            .field("content_row_bytes", &self.content_row_bytes)
            .field("pixel_bytes", &self.pixels.len())
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BrowserCaptureLayout {
    size: PhysicalSize,
    device_size: DeviceIntSize,
    stride: usize,
    byte_len: usize,
    content: BrowserPhysicalRect,
    content_row_bytes: usize,
}

pub(crate) struct PreparedBrowserCapture {
    request: BrowserFrameRequest,
    layout: BrowserCaptureLayout,
    pixels: Vec<u8>,
}

pub(crate) struct MappedBrowserCapture(PreparedBrowserCapture);

impl fmt::Debug for PreparedBrowserCapture {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedBrowserCapture")
            .field("request", &self.request)
            .field("layout", &self.layout)
            .field("pixel_bytes", &self.pixels.len())
            .finish()
    }
}

impl fmt::Debug for MappedBrowserCapture {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("MappedBrowserCapture")
            .field(&self.0)
            .finish()
    }
}

impl PreparedBrowserCapture {
    pub(crate) fn new(
        request: BrowserFrameRequest,
        geometry: BrowserChromeGeometry,
    ) -> Result<Self, WebRenderWindowError> {
        if geometry.surface() != request.surface() {
            return Err(capture_error(
                WebRenderWindowFailureStage::PrepareCapture,
                WebRenderWindowErrorKind::CaptureIdentityMismatch,
                "capture geometry does not name the exact browser-frame surface",
            ));
        }
        let layout = browser_capture_layout(request.surface().size(), geometry.content())?;
        let pixels = allocate_browser_capture_pixels(layout.byte_len, |buffer, bytes| {
            buffer.try_reserve_exact(bytes).map_err(|_| ())
        })?;
        Ok(Self {
            request,
            layout,
            pixels,
        })
    }

    pub(crate) const fn expected_device_size(&self) -> DeviceIntSize {
        self.layout.device_size
    }

    pub(crate) const fn stride(&self) -> usize {
        self.layout.stride
    }

    pub(crate) fn pixels_mut(&mut self) -> &mut [u8] {
        &mut self.pixels
    }

    pub(crate) fn validate_recorded_size(
        &self,
        actual: DeviceIntSize,
    ) -> Result<(), WebRenderWindowError> {
        if actual.width <= 0 || actual.height <= 0 || actual != self.expected_device_size() {
            return Err(capture_error(
                WebRenderWindowFailureStage::RecordCapture,
                WebRenderWindowErrorKind::CaptureSizeMismatch,
                format_args!(
                    "recorded frame size {actual:?} differs from exact expected {:?}",
                    self.expected_device_size()
                ),
            ));
        }
        Ok(())
    }

    pub(crate) fn into_mapped(self) -> MappedBrowserCapture {
        MappedBrowserCapture(self)
    }
}

impl MappedBrowserCapture {
    pub(crate) fn validate_completion(
        &self,
        request: BrowserFrameRequest,
        page_epoch: Option<u32>,
        chrome_epoch: u32,
        backend_publish_id: u64,
    ) -> Result<(), WebRenderWindowError> {
        let page_epoch_invalid = match request.page() {
            BrowserPageSnapshot::Blank => page_epoch.is_some(),
            BrowserPageSnapshot::Scene(_) => page_epoch.is_none(),
        };
        if self.0.request != request
            || page_epoch_invalid
            || page_epoch.is_some_and(|epoch| epoch == 0 || epoch > request.epoch())
            || chrome_epoch == 0
            || chrome_epoch > request.epoch()
            || backend_publish_id == 0
        {
            return Err(capture_error(
                WebRenderWindowFailureStage::BindCapture,
                WebRenderWindowErrorKind::CaptureIdentityMismatch,
                "mapped browser pixels do not match the exact receipt identity",
            ));
        }
        Ok(())
    }

    pub(crate) fn bind(self, receipt: BrowserFrameReceipt) -> BrowserFrameCapture {
        debug_assert_eq!(self.0.request, receipt.request());
        BrowserFrameCapture {
            receipt,
            size: self.0.layout.size,
            stride: self.0.layout.stride,
            content: self.0.layout.content,
            content_row_bytes: self.0.layout.content_row_bytes,
            pixels: self.0.pixels,
        }
    }
}

#[allow(clippy::too_many_lines)]
fn browser_capture_layout(
    size: PhysicalSize,
    content: BrowserPhysicalRect,
) -> Result<BrowserCaptureLayout, WebRenderWindowError> {
    if size.width < MIN_BROWSER_CAPTURE_DIMENSION || size.height < MIN_BROWSER_CAPTURE_DIMENSION {
        return Err(capture_error(
            WebRenderWindowFailureStage::PrepareCapture,
            WebRenderWindowErrorKind::SizeMismatch,
            "browser capture rejects zero or one-pixel framebuffer axes",
        ));
    }
    if size.width > MAX_BROWSER_CAPTURE_DIMENSION || size.height > MAX_BROWSER_CAPTURE_DIMENSION {
        return Err(capture_error(
            WebRenderWindowFailureStage::PrepareCapture,
            WebRenderWindowErrorKind::ResourceLimit,
            "browser capture dimensions exceed the fixed presentation limit",
        ));
    }
    if content.width == 0 || content.height == 0 {
        return Err(capture_error(
            WebRenderWindowFailureStage::PrepareCapture,
            WebRenderWindowErrorKind::SizeMismatch,
            "browser capture requires a nonempty physical content viewport",
        ));
    }
    let content_right = content.x.checked_add(content.width);
    let content_bottom = content.y.checked_add(content.height);
    if content_right.is_none_or(|right| right > size.width)
        || content_bottom.is_none_or(|bottom| bottom > size.height)
    {
        return Err(capture_error(
            WebRenderWindowFailureStage::PrepareCapture,
            WebRenderWindowErrorKind::CaptureIdentityMismatch,
            "browser content crop lies outside the exact capture extent",
        ));
    }
    let pixels = u64::from(size.width)
        .checked_mul(u64::from(size.height))
        .filter(|pixels| *pixels <= MAX_BROWSER_CAPTURE_PIXELS)
        .ok_or_else(|| {
            capture_error(
                WebRenderWindowFailureStage::PrepareCapture,
                WebRenderWindowErrorKind::ResourceLimit,
                "browser capture pixel count overflowed or exceeded its fixed limit",
            )
        })?;
    let byte_len_u64 = pixels
        .checked_mul(BROWSER_CAPTURE_BYTES_PER_PIXEL as u64)
        .filter(|bytes| *bytes <= MAX_BROWSER_CAPTURE_BYTES)
        .ok_or_else(|| {
            capture_error(
                WebRenderWindowFailureStage::PrepareCapture,
                WebRenderWindowErrorKind::ResourceLimit,
                "browser capture byte count overflowed or exceeded its fixed limit",
            )
        })?;
    let device_width = i32::try_from(size.width).map_err(|_| {
        capture_error(
            WebRenderWindowFailureStage::PrepareCapture,
            WebRenderWindowErrorKind::ResourceLimit,
            "browser capture width does not fit WebRender device coordinates",
        )
    })?;
    let device_height = i32::try_from(size.height).map_err(|_| {
        capture_error(
            WebRenderWindowFailureStage::PrepareCapture,
            WebRenderWindowErrorKind::ResourceLimit,
            "browser capture height does not fit WebRender device coordinates",
        )
    })?;
    let width = usize::try_from(size.width).map_err(|_| {
        capture_error(
            WebRenderWindowFailureStage::PrepareCapture,
            WebRenderWindowErrorKind::ResourceLimit,
            "browser capture width does not fit the target address space",
        )
    })?;
    let stride = width
        .checked_mul(BROWSER_CAPTURE_BYTES_PER_PIXEL)
        .ok_or_else(|| {
            capture_error(
                WebRenderWindowFailureStage::PrepareCapture,
                WebRenderWindowErrorKind::ResourceLimit,
                "browser capture stride overflowed",
            )
        })?;
    let content_row_bytes = usize::try_from(content.width)
        .ok()
        .and_then(|width| width.checked_mul(BROWSER_CAPTURE_BYTES_PER_PIXEL))
        .ok_or_else(|| {
            capture_error(
                WebRenderWindowFailureStage::PrepareCapture,
                WebRenderWindowErrorKind::ResourceLimit,
                "browser capture content-row byte count overflowed",
            )
        })?;
    let byte_len = usize::try_from(byte_len_u64).map_err(|_| {
        capture_error(
            WebRenderWindowFailureStage::PrepareCapture,
            WebRenderWindowErrorKind::ResourceLimit,
            "browser capture byte count does not fit the target address space",
        )
    })?;
    let checked_len = stride
        .checked_mul(usize::try_from(size.height).map_err(|_| {
            capture_error(
                WebRenderWindowFailureStage::PrepareCapture,
                WebRenderWindowErrorKind::ResourceLimit,
                "browser capture height does not fit the target address space",
            )
        })?)
        .ok_or_else(|| {
            capture_error(
                WebRenderWindowFailureStage::PrepareCapture,
                WebRenderWindowErrorKind::ResourceLimit,
                "browser capture destination length overflowed",
            )
        })?;
    if checked_len != byte_len {
        return Err(capture_error(
            WebRenderWindowFailureStage::PrepareCapture,
            WebRenderWindowErrorKind::CaptureSizeMismatch,
            "browser capture stride and byte accounting disagreed",
        ));
    }
    Ok(BrowserCaptureLayout {
        size,
        device_size: DeviceIntSize::new(device_width, device_height),
        stride,
        byte_len,
        content,
        content_row_bytes,
    })
}

fn allocate_browser_capture_pixels(
    byte_len: usize,
    reserve: impl FnOnce(&mut Vec<u8>, usize) -> Result<(), ()>,
) -> Result<Vec<u8>, WebRenderWindowError> {
    let mut pixels = Vec::new();
    reserve(&mut pixels, byte_len).map_err(|()| {
        capture_error(
            WebRenderWindowFailureStage::PrepareCapture,
            WebRenderWindowErrorKind::CaptureAllocationFailed,
            "browser capture buffer allocation failed before transaction submission",
        )
    })?;
    pixels.resize(byte_len, 0);
    Ok(pixels)
}

fn capture_error(
    stage: WebRenderWindowFailureStage,
    kind: WebRenderWindowErrorKind,
    detail: impl fmt::Display,
) -> WebRenderWindowError {
    WebRenderWindowError::new(stage, kind, detail)
}

/// Typed first-party hit target in the last successful composition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BrowserHitTarget {
    /// Page content with exact publication identity and content-local point.
    Page {
        /// Exact page publication identity.
        page: BrowserPageIdentity,
        /// Physical point relative to the clipped page viewport.
        point: PhysicalPoint,
    },
    /// Body of one exact tab.
    Tab(BrowserTabIdentity),
    /// Close affordance above one exact tab body.
    TabClose(BrowserTabIdentity),
    /// Editable address field.
    AddressBar,
    /// Status overlay above the page.
    Status,
    /// One visible primary control. URL editing remains `AddressBar`.
    PrimaryControl {
        /// Stable browser-session element identity.
        element: BrowserChromeElementIdentity,
        /// Stable semantic control kind.
        kind: BrowserPrimaryControlKind,
    },
    /// One visible row in the sole open primary popup.
    PrimaryPopupRow {
        /// Stable browser-session row identity.
        element: BrowserChromeElementIdentity,
        /// Stable semantic row kind.
        kind: BrowserPrimaryPopupRowKind,
    },
    /// Non-row interior of the topmost open popup.
    PrimaryPopupSurface {
        /// Sole popup kind.
        kind: BrowserPrimaryPopupKind,
        /// Exact anchor element.
        anchor: BrowserChromeElementIdentity,
    },
    /// Surface outside the open popup; the browser may request dismissal.
    PrimaryPopupDismiss {
        /// Sole popup kind.
        kind: BrowserPrimaryPopupKind,
        /// Exact anchor element.
        anchor: BrowserChromeElementIdentity,
    },
}

/// Hit result bound to the exact last successful composition receipt.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BrowserHitTestResult {
    receipt: BrowserFrameReceipt,
    target: BrowserHitTarget,
}

impl BrowserHitTestResult {
    /// Exact successful composition and root epoch used for this hit.
    #[must_use]
    pub const fn receipt(self) -> BrowserFrameReceipt {
        self.receipt
    }

    /// Typed target in deterministic topmost-first z order.
    #[must_use]
    pub const fn target(self) -> BrowserHitTarget {
        self.target
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BrowserPipelines {
    root: PipelineId,
    chrome: PipelineId,
}

impl BrowserPipelines {
    pub(crate) const fn new(renderer_namespace: u32) -> Self {
        Self {
            root: PipelineId(renderer_namespace, u32::MAX - 1),
            chrome: PipelineId(renderer_namespace, u32::MAX - 2),
        }
    }

    pub(crate) const fn root(self) -> PipelineId {
        self.root
    }

    pub(crate) const fn chrome(self) -> PipelineId {
        self.chrome
    }

    pub(crate) fn rejects_page(self, pipeline: PipelineKey) -> bool {
        let pipeline = PipelineId(pipeline.source(), pipeline.pipeline());
        pipeline == self.root || pipeline == self.chrome
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct BrowserCandidate {
    pub(crate) page: BrowserPageSnapshot,
    pub(crate) previous_page: BrowserPageSnapshot,
    pub(crate) page_epoch: Option<u32>,
    pub(crate) chrome_revision: BrowserChromeRevision,
    pub(crate) chrome_epoch: u32,
    pub(crate) page_replaced: bool,
    pub(crate) chrome_replaced: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct BrowserTabHitRegion {
    identity: BrowserTabIdentity,
    rect: BrowserPhysicalRect,
    close: BrowserPhysicalRect,
}

#[derive(Clone, Debug)]
pub(crate) struct BrowserChromeHitMap {
    geometry: BrowserChromeGeometry,
    tabs: Box<[BrowserTabHitRegion]>,
    status_visible: bool,
    primary: Option<BrowserPrimaryChromeLayout>,
}

#[derive(Clone, Debug)]
pub(crate) struct BrowserCompositorContract {
    page: BrowserPageSnapshot,
    page_epoch: Option<u32>,
    page_display_list_bytes: usize,
    chrome_revision: Option<BrowserChromeRevision>,
    chrome_epoch: Option<u32>,
    chrome_display_list_bytes: usize,
    chrome_primitives: usize,
    last_page_revision: Option<BrowserPageSceneRevision>,
    last_chrome_revision: Option<BrowserChromeRevision>,
    hit_map: Option<BrowserChromeHitMap>,
    last_receipt: Option<BrowserFrameReceipt>,
    surface_stale: bool,
    accepted_in_flight: bool,
    terminal: bool,
}

impl Default for BrowserCompositorContract {
    fn default() -> Self {
        Self {
            page: BrowserPageSnapshot::Blank,
            page_epoch: None,
            page_display_list_bytes: 0,
            chrome_revision: None,
            chrome_epoch: None,
            chrome_display_list_bytes: 0,
            chrome_primitives: 0,
            last_page_revision: None,
            last_chrome_revision: None,
            hit_map: None,
            last_receipt: None,
            surface_stale: false,
            accepted_in_flight: false,
            terminal: false,
        }
    }
}

impl BrowserCompositorContract {
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    pub(crate) fn validate_candidate(
        &self,
        page_update: &BrowserPageUpdate,
        chrome: Option<&BrowserChromeScene>,
        request: BrowserFrameRequest,
        pipelines: BrowserPipelines,
        current_surface: WebRenderSurfaceSnapshot,
        previous_epoch: Option<u32>,
        previous_sequence: Option<u64>,
        submitted_frames: u64,
    ) -> Result<BrowserCandidate, WebRenderWindowError> {
        if self.terminal {
            return Err(browser_contract_error(
                WebRenderWindowErrorKind::TerminalState,
                "browser compositor is terminal after an accepted failure",
            ));
        }
        if self.accepted_in_flight {
            return Err(browser_contract_error(
                WebRenderWindowErrorKind::Contract,
                "browser compositor already has an accepted transaction in flight",
            ));
        }
        if request.surface != current_surface {
            return Err(browser_contract_error(
                WebRenderWindowErrorKind::StaleSurfaceRevision,
                "browser request does not name the presenter's exact current surface snapshot",
            ));
        }
        if request.epoch == 0
            || request.epoch == u32::MAX
            || previous_epoch.is_some_and(|previous| request.epoch <= previous)
        {
            return Err(browser_contract_error(
                WebRenderWindowErrorKind::Epoch,
                "browser root epoch is zero, reserved, repeated, or nonmonotonic",
            ));
        }
        if request.sequence == 0
            || previous_sequence.is_some_and(|previous| request.sequence <= previous)
            || submitted_frames == u64::MAX
        {
            return Err(browser_contract_error(
                WebRenderWindowErrorKind::FrameSequence,
                "browser swap sequence is zero, repeated, exhausted, or nonmonotonic",
            ));
        }
        let (page, page_epoch, page_replaced) = match page_update {
            BrowserPageUpdate::Retain => (self.page, self.page_epoch, false),
            BrowserPageUpdate::Install(scene) => {
                let identity = scene.identity();
                if pipelines.rejects_page(identity.pipeline()) {
                    return Err(browser_contract_error(
                        WebRenderWindowErrorKind::PipelineMismatch,
                        "page pipeline collides with a presenter-private compositor pipeline",
                    ));
                }
                if self
                    .last_page_revision
                    .is_some_and(|previous| identity.revision() <= previous)
                {
                    return Err(browser_contract_error(
                        WebRenderWindowErrorKind::RevisionRegressed,
                        "page scene revision is repeated, stale, or reordered",
                    ));
                }
                (
                    BrowserPageSnapshot::Scene(identity),
                    Some(request.epoch),
                    true,
                )
            }
            BrowserPageUpdate::ClearToBlank => {
                if self.page == BrowserPageSnapshot::Blank {
                    return Err(browser_contract_error(
                        WebRenderWindowErrorKind::Contract,
                        "cannot clear an already blank compositor page",
                    ));
                }
                (BrowserPageSnapshot::Blank, None, true)
            }
        };
        if page != request.page {
            return Err(browser_contract_error(
                WebRenderWindowErrorKind::DocumentMismatch,
                "requested page identity differs from the supplied-or-retained page",
            ));
        }

        let (chrome_revision, chrome_epoch, chrome_replaced) = match chrome {
            Some(scene) => {
                if scene.geometry.surface != request.surface {
                    return Err(browser_contract_error(
                        WebRenderWindowErrorKind::SurfaceMismatch,
                        "chrome scene was authored for a different surface snapshot",
                    ));
                }
                if self
                    .last_chrome_revision
                    .is_some_and(|previous| scene.revision <= previous)
                {
                    return Err(browser_contract_error(
                        WebRenderWindowErrorKind::RevisionRegressed,
                        "chrome revision is repeated, stale, or reordered",
                    ));
                }
                (scene.revision, request.epoch, true)
            }
            None => (
                self.chrome_revision.ok_or_else(|| {
                    browser_contract_error(
                        WebRenderWindowErrorKind::Contract,
                        "first browser composition requires an explicit chrome scene",
                    )
                })?,
                self.chrome_epoch.expect("live chrome always has an epoch"),
                false,
            ),
        };
        if chrome_revision != request.chrome_revision {
            return Err(browser_contract_error(
                WebRenderWindowErrorKind::DocumentMismatch,
                "requested chrome revision differs from the supplied-or-retained chrome",
            ));
        }
        if self.surface_stale
            && (!chrome_replaced || (page != BrowserPageSnapshot::Blank && !page_replaced))
        {
            return Err(browser_contract_error(
                WebRenderWindowErrorKind::StaleComposition,
                "surface transition requires exact replacement chrome and live-page scenes",
            ));
        }
        Ok(BrowserCandidate {
            page,
            previous_page: self.page,
            page_epoch,
            chrome_revision,
            chrome_epoch,
            page_replaced,
            chrome_replaced,
        })
    }

    pub(crate) fn mark_accepted(&mut self) {
        self.accepted_in_flight = true;
    }

    pub(crate) const fn accepted_in_flight(&self) -> bool {
        self.accepted_in_flight
    }

    pub(crate) fn fail_after_acceptance(&mut self) {
        self.accepted_in_flight = false;
        self.last_receipt = None;
        self.hit_map = None;
        self.terminal = true;
    }

    pub(crate) fn mark_surface_stale(&mut self) {
        self.surface_stale = true;
    }

    pub(crate) fn retained_hit_map(&self) -> Option<BrowserChromeHitMap> {
        self.hit_map.clone()
    }

    pub(crate) fn retained_geometry(&self) -> Option<BrowserChromeGeometry> {
        self.hit_map.as_ref().map(|map| map.geometry)
    }

    pub(crate) fn invalidate_for_legacy_acceptance(&mut self) {
        self.accepted_in_flight = false;
        self.last_receipt = None;
        self.hit_map = None;
        self.terminal = true;
    }

    pub(crate) fn commit_success(
        &mut self,
        candidate: BrowserCandidate,
        request: BrowserFrameRequest,
        hit_map: BrowserChromeHitMap,
        accounting: BrowserFrameAccounting,
    ) -> BrowserFrameReceipt {
        self.page = candidate.page;
        self.page_epoch = candidate.page_epoch;
        self.chrome_revision = Some(candidate.chrome_revision);
        self.chrome_epoch = Some(candidate.chrome_epoch);
        if candidate.page_replaced {
            self.page_display_list_bytes = accounting.page_display_list_bytes;
            if let BrowserPageSnapshot::Scene(identity) = candidate.page {
                self.last_page_revision = Some(identity.revision());
            }
        }
        if candidate.chrome_replaced {
            self.chrome_display_list_bytes = accounting.chrome_display_list_bytes;
            self.chrome_primitives = accounting.chrome_primitives;
            self.last_chrome_revision = Some(candidate.chrome_revision);
        }
        let receipt = BrowserFrameReceipt {
            request,
            page_epoch: candidate.page_epoch,
            chrome_epoch: candidate.chrome_epoch,
            backend_publish_id: accounting.backend_publish_id,
            rgba8_byte_equivalent: accounting.rgba8_byte_equivalent,
            page_display_list_bytes: self.page_display_list_bytes,
            chrome_display_list_bytes: self.chrome_display_list_bytes,
            root_display_list_bytes: accounting.root_display_list_bytes,
            chrome_primitives: self.chrome_primitives,
        };
        self.hit_map = Some(hit_map);
        self.last_receipt = Some(receipt);
        self.surface_stale = false;
        self.accepted_in_flight = false;
        receipt
    }

    #[allow(clippy::too_many_lines)]
    pub(crate) fn hit_test(
        &self,
        point: PhysicalPoint,
        surface: WebRenderSurfaceSnapshot,
    ) -> Result<Option<BrowserHitTestResult>, WebRenderWindowError> {
        if self.accepted_in_flight || self.surface_stale {
            return Err(browser_hit_error(
                "browser composition has no authoritative hit map for the current surface",
            ));
        }
        let receipt = self
            .last_receipt
            .ok_or_else(|| browser_hit_error("no successful browser composition is live"))?;
        if receipt.request.surface != surface {
            return Err(browser_hit_error(
                "hit test named a surface other than the last successful composition",
            ));
        }
        let hit_map = self
            .hit_map
            .as_ref()
            .ok_or_else(|| browser_hit_error("successful receipt has no exact hit map"))?;

        if let Some(popup) = hit_map
            .primary
            .as_ref()
            .and_then(BrowserPrimaryChromeLayout::popup)
        {
            for row in popup.rows().iter().rev() {
                if row.rect().is_some_and(|rect| rect.contains(point)) {
                    return Ok(Some(BrowserHitTestResult {
                        receipt,
                        target: BrowserHitTarget::PrimaryPopupRow {
                            element: row.element(),
                            kind: row.kind(),
                        },
                    }));
                }
            }
            if popup.rect().contains(point) {
                return Ok(Some(BrowserHitTestResult {
                    receipt,
                    target: BrowserHitTarget::PrimaryPopupSurface {
                        kind: popup.kind(),
                        anchor: popup.anchor(),
                    },
                }));
            }
            let size = hit_map.geometry.surface.size();
            if BrowserPhysicalRect::new(0, 0, size.width, size.height).contains(point) {
                return Ok(Some(BrowserHitTestResult {
                    receipt,
                    target: BrowserHitTarget::PrimaryPopupDismiss {
                        kind: popup.kind(),
                        anchor: popup.anchor(),
                    },
                }));
            }
            return Ok(None);
        }

        if hit_map.status_visible && hit_map.geometry.status.contains(point) {
            return Ok(Some(BrowserHitTestResult {
                receipt,
                target: BrowserHitTarget::Status,
            }));
        }
        if hit_map
            .primary
            .as_ref()
            .map_or(hit_map.geometry.address_field, |layout| {
                layout.preview().address_field()
            })
            .contains(point)
        {
            return Ok(Some(BrowserHitTestResult {
                receipt,
                target: BrowserHitTarget::AddressBar,
            }));
        }
        if let Some(primary) = hit_map.primary.as_ref() {
            for control in primary.controls().iter().rev() {
                if control.kind() == BrowserPrimaryControlKind::UrlBar {
                    continue;
                }
                if control.rect().is_some_and(|rect| rect.contains(point)) {
                    return Ok(Some(BrowserHitTestResult {
                        receipt,
                        target: BrowserHitTarget::PrimaryControl {
                            element: control.element(),
                            kind: control.kind(),
                        },
                    }));
                }
            }
        }
        for tab in hit_map.tabs.iter().rev() {
            if tab.close.contains(point) {
                return Ok(Some(BrowserHitTestResult {
                    receipt,
                    target: BrowserHitTarget::TabClose(tab.identity),
                }));
            }
            if tab.rect.contains(point) {
                return Ok(Some(BrowserHitTestResult {
                    receipt,
                    target: BrowserHitTarget::Tab(tab.identity),
                }));
            }
        }
        if let BrowserPageSnapshot::Scene(page) = receipt.request.page
            && hit_map.geometry.content.contains(point)
        {
            return Ok(Some(BrowserHitTestResult {
                receipt,
                target: BrowserHitTarget::Page {
                    page,
                    point: hit_map.geometry.content.local_point(point),
                },
            }));
        }
        Ok(None)
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct BrowserFrameAccounting {
    pub(crate) backend_publish_id: u64,
    pub(crate) rgba8_byte_equivalent: u64,
    pub(crate) page_display_list_bytes: usize,
    pub(crate) chrome_display_list_bytes: usize,
    pub(crate) root_display_list_bytes: usize,
    pub(crate) chrome_primitives: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BrowserTextPartition {
    resource_version: DocumentVersion,
    page_count: usize,
    chrome_count: usize,
}

impl BrowserTextPartition {
    pub(crate) const fn page_count(self) -> usize {
        self.page_count
    }

    pub(crate) fn chrome_entries(
        self,
        entries: &[PreparedSceneTextEntry],
    ) -> Result<&[PreparedSceneTextEntry], WebRenderWindowError> {
        self.validate_entries(entries)?;
        Ok(&entries[self.page_count..])
    }

    pub(crate) fn validate_entries(
        self,
        entries: &[PreparedSceneTextEntry],
    ) -> Result<(), WebRenderWindowError> {
        let total = self
            .page_count
            .checked_add(self.chrome_count)
            .ok_or_else(|| {
                browser_resource_error("combined page/chrome text partition overflowed")
            })?;
        if entries.len() != total {
            return Err(browser_contract_error(
                WebRenderWindowErrorKind::Text,
                "prepared text count differs from the exact page/chrome partition",
            ));
        }
        for (index, entry) in entries.iter().enumerate() {
            let index = u32::try_from(index).map_err(|_| {
                browser_resource_error("prepared page/chrome text index exceeds u32 capacity")
            })?;
            if entry.document_version() != self.resource_version || entry.pending_index() != index {
                return Err(browser_contract_error(
                    WebRenderWindowErrorKind::Text,
                    "prepared text is stale, foreign, duplicated, or crossed between partitions",
                ));
            }
        }
        Ok(())
    }
}

pub(crate) fn stage_browser_texts(
    resource_version: DocumentVersion,
    page: Option<&[ShapedSceneText]>,
    chrome: Option<&BrowserChromeScene>,
) -> Result<(Vec<ShapedSceneText>, BrowserTextPartition), WebRenderWindowError> {
    let page_count = page.map_or(0, <[ShapedSceneText]>::len);
    let chrome_count = chrome.map_or(0, BrowserChromeScene::text_count);
    let total = page_count
        .checked_add(chrome_count)
        .ok_or_else(|| browser_resource_error("combined page/chrome text count overflowed"))?;
    if total > u32::MAX as usize {
        return Err(browser_resource_error(
            "combined page/chrome text count exceeds u32 capacity",
        ));
    }
    let mut staged = Vec::new();
    staged.try_reserve_exact(total).map_err(|_| {
        browser_resource_error("could not reserve the combined page/chrome text inventory")
    })?;
    if let Some(page) = page {
        for text in page {
            let index = u32::try_from(staged.len()).expect("combined count was checked");
            staged.push(ShapedSceneText::new(
                resource_version,
                index,
                Arc::clone(text.shaped()),
            ));
        }
    }
    if let Some(chrome) = chrome {
        for shaped in chrome.shaped_texts() {
            let index = u32::try_from(staged.len()).expect("combined count was checked");
            staged.push(ShapedSceneText::new(
                resource_version,
                index,
                Arc::clone(shaped),
            ));
        }
    }
    Ok((
        staged,
        BrowserTextPartition {
            resource_version,
            page_count,
            chrome_count,
        },
    ))
}

pub(crate) struct BrowserBuiltDisplayList {
    pub(crate) pipeline: PipelineId,
    pub(crate) display_list: BuiltDisplayList,
    pub(crate) primitive_count: usize,
}

#[allow(clippy::too_many_lines)]
pub(crate) fn build_browser_chrome_display_list(
    scene: &BrowserChromeScene,
    entries: &[PreparedSceneTextEntry],
    pipeline: PipelineId,
    first_staging_index: usize,
) -> Result<BrowserBuiltDisplayList, WebRenderWindowError> {
    if entries.len() != scene.text_count {
        return Err(browser_contract_error(
            WebRenderWindowErrorKind::Text,
            "prepared chrome text count differs from the immutable chrome scene",
        ));
    }
    let full = rect_to_layout(BrowserPhysicalRect::new(
        0,
        0,
        scene.geometry.surface.size().width,
        scene.geometry.surface.size().height,
    ));
    let root = SpaceAndClipInfo::root_scroll(pipeline);
    let mut builder = DisplayListBuilder::new(pipeline);
    builder.begin();
    let clip = builder.define_clip_rect(root.spatial_id, full);
    let clip_chain_id = builder.define_clip_chain(None, [clip]);
    let root_space = SpaceAndClipInfo {
        spatial_id: root.spatial_id,
        clip_chain_id,
    };
    let mut primitives = 0_usize;

    push_colored_rect(
        &mut builder,
        root_space,
        scene.geometry.tab_strip,
        ColorF::new(0.10, 0.12, 0.16, 1.0),
        &mut primitives,
    )?;
    push_colored_rect(
        &mut builder,
        root_space,
        scene.geometry.address_strip,
        ColorF::new(0.15, 0.17, 0.22, 1.0),
        &mut primitives,
    )?;
    push_colored_rect(
        &mut builder,
        root_space,
        scene
            .primary_layout
            .as_ref()
            .map_or(scene.geometry.address_field, |layout| {
                layout.preview().url_container()
            }),
        ColorF::new(0.96, 0.97, 0.98, 1.0),
        &mut primitives,
    )?;

    let mut text_index = 0_usize;
    for (index, tab) in scene.state.tabs.iter().enumerate() {
        let tab_rect = scene.tab_rect(index);
        let selected = scene.state.active_tab == Some(tab.identity);
        push_colored_rect(
            &mut builder,
            root_space,
            tab_rect,
            tab_background_color(selected, tab.interaction),
            &mut primitives,
        )?;
        if tab.loading {
            let indicator = BrowserPhysicalRect::new(
                tab_rect.x,
                tab_rect.y.saturating_add(tab_rect.height.saturating_sub(3)),
                tab_rect.width,
                3.min(tab_rect.height),
            );
            push_colored_rect(
                &mut builder,
                root_space,
                indicator,
                ColorF::new(0.20, 0.66, 0.96, 1.0),
                &mut primitives,
            )?;
        }
        let close = scene.tab_close_rect(index, tab_rect);
        let text_bounds = scene.tab_title_rect(index, tab_rect, close);
        push_prepared_text(
            &mut builder,
            root_space,
            text_bounds,
            prepared_chrome_entry(entries, text_index, first_staging_index)?,
            ColorF::new(0.94, 0.95, 0.97, 1.0),
            &mut primitives,
        )?;
        text_index += 1;
        push_close_affordance(
            &mut builder,
            root_space,
            close,
            selected,
            tab.close_availability,
            tab.close_interaction,
            &mut primitives,
        )?;
        builder.push_hit_test(
            rect_to_layout(tab_rect),
            clip_chain_id,
            root.spatial_id,
            PrimitiveFlags::default(),
            (tab.identity.get(), 1),
        );
        primitives = checked_primitive_increment(primitives)?;
        builder.push_hit_test(
            rect_to_layout(close),
            clip_chain_id,
            root.spatial_id,
            PrimitiveFlags::default(),
            (tab.identity.get(), 2),
        );
        primitives = checked_primitive_increment(primitives)?;
    }

    if let Some(primary) = scene.primary_layout.as_ref() {
        for control in primary.controls() {
            let Some(rect) = control.rect() else {
                continue;
            };
            if control.kind() == BrowserPrimaryControlKind::UrlBar {
                continue;
            }
            push_primary_control(
                &mut builder,
                root_space,
                *control,
                scene
                    .state
                    .primary
                    .as_ref()
                    .expect("resolved primary state"),
                &mut primitives,
            )?;
            builder.push_hit_test(
                rect_to_layout(rect),
                clip_chain_id,
                root.spatial_id,
                PrimitiveFlags::default(),
                (
                    control.element().get(),
                    10 + u16::try_from(control.kind().index())
                        .expect("fixed primary control index fits u16"),
                ),
            );
            primitives = checked_primitive_increment(primitives)?;
        }
        let address = primary.control(BrowserPrimaryControlKind::UrlBar);
        if let Some(rect) = address.rect() {
            push_colored_rect(
                &mut builder,
                root_space,
                rect,
                address_background_color(address.availability(), address.interaction()),
                &mut primitives,
            )?;
        }
    }

    if let Some(selection) = address_selection_rect(scene) {
        push_colored_rect(
            &mut builder,
            root_space,
            selection,
            ColorF::new(0.24, 0.51, 0.91, 0.45),
            &mut primitives,
        )?;
    }
    push_prepared_text(
        &mut builder,
        root_space,
        inset_all(scene.address_field(), 8),
        prepared_chrome_entry(entries, text_index, first_staging_index)?,
        ColorF::new(0.04, 0.05, 0.07, 1.0),
        &mut primitives,
    )?;
    text_index += 1;
    builder.push_hit_test(
        rect_to_layout(scene.address_field()),
        clip_chain_id,
        root.spatial_id,
        PrimitiveFlags::default(),
        (scene.revision.get(), 3),
    );
    primitives = checked_primitive_increment(primitives)?;

    if scene.state.status.is_some() {
        push_colored_rect(
            &mut builder,
            root_space,
            scene.geometry.status,
            ColorF::new(0.05, 0.06, 0.08, 0.94),
            &mut primitives,
        )?;
        push_prepared_text(
            &mut builder,
            root_space,
            inset_all(scene.geometry.status, 6),
            prepared_chrome_entry(entries, text_index, first_staging_index)?,
            ColorF::new(0.96, 0.97, 0.99, 1.0),
            &mut primitives,
        )?;
        text_index += 1;
        builder.push_hit_test(
            rect_to_layout(scene.geometry.status),
            clip_chain_id,
            root.spatial_id,
            PrimitiveFlags::default(),
            (scene.revision.get(), 4),
        );
        primitives = checked_primitive_increment(primitives)?;
    }

    let focus = match scene.state.focus {
        BrowserChromeFocus::None
        | BrowserChromeFocus::PrimaryControl(_)
        | BrowserChromeFocus::PopupRow(_) => None,
        BrowserChromeFocus::AddressBar => Some(scene.address_field()),
        BrowserChromeFocus::Page => Some(scene.geometry.content),
        BrowserChromeFocus::Tab(identity) => scene
            .state
            .tabs
            .iter()
            .position(|tab| tab.identity == identity)
            .map(|index| scene.tab_rect(index)),
    };
    if let Some(focus) = focus {
        push_focus_ring(&mut builder, root_space, focus, &mut primitives)?;
    }

    if let Some(primary) = scene.primary_layout.as_ref() {
        text_index = text_index
            .checked_add(primary.controls().len())
            .ok_or_else(|| browser_resource_error("primary control text index overflowed"))?;
        if text_index > entries.len() {
            return Err(browser_contract_error(
                WebRenderWindowErrorKind::Text,
                "primary control labels exceed their exact prepared partition",
            ));
        }
        if let Some(popup) = primary.popup() {
            push_primary_popup(
                &mut builder,
                root_space,
                clip_chain_id,
                root.spatial_id,
                popup,
                primary.preview().direction(),
                entries,
                &mut text_index,
                first_staging_index,
                scene.geometry.surface,
                &mut primitives,
            )?;
        }
    }
    if text_index != entries.len() {
        return Err(browser_contract_error(
            WebRenderWindowErrorKind::Text,
            "chrome display list did not consume its exact prepared text partition",
        ));
    }

    let (_, display_list) = builder.end();
    if display_list.size_in_bytes() > MAX_BROWSER_CHROME_DISPLAY_LIST_BYTES {
        return Err(browser_resource_error(
            "serialized browser chrome display list exceeds its fixed limit",
        ));
    }
    Ok(BrowserBuiltDisplayList {
        pipeline,
        display_list,
        primitive_count: primitives,
    })
}

pub(crate) fn build_browser_root_display_list(
    pipelines: BrowserPipelines,
    surface: WebRenderSurfaceSnapshot,
    geometry: BrowserChromeGeometry,
    page: BrowserPageSnapshot,
) -> Result<BrowserBuiltDisplayList, WebRenderWindowError> {
    if geometry.surface != surface {
        return Err(browser_contract_error(
            WebRenderWindowErrorKind::SurfaceMismatch,
            "root composition geometry differs from its exact surface snapshot",
        ));
    }
    let full_rect = BrowserPhysicalRect::new(0, 0, surface.size().width, surface.size().height);
    let full = rect_to_layout(full_rect);
    let root = SpaceAndClipInfo::root_scroll(pipelines.root);
    let mut builder = DisplayListBuilder::new(pipelines.root);
    builder.begin();
    let clip = builder.define_clip_rect(root.spatial_id, full);
    let clip_chain_id = builder.define_clip_chain(None, [clip]);
    let root_space = SpaceAndClipInfo {
        spatial_id: root.spatial_id,
        clip_chain_id,
    };
    let mut primitives = 0_usize;
    push_colored_rect(
        &mut builder,
        root_space,
        full_rect,
        ColorF::new(1.0, 1.0, 1.0, 1.0),
        &mut primitives,
    )?;
    if let BrowserPageSnapshot::Scene(page) = page {
        if geometry.content.width == 0 || geometry.content.height == 0 {
            return Err(browser_contract_error(
                WebRenderWindowErrorKind::SizeMismatch,
                "a live page cannot enter a zero-sized chrome content viewport",
            ));
        }
        builder.push_hit_test(
            rect_to_layout(geometry.content),
            clip_chain_id,
            root.spatial_id,
            PrimitiveFlags::default(),
            (page.navigation.get(), 5),
        );
        primitives = checked_primitive_increment(primitives)?;
        builder.push_iframe(
            rect_to_layout(geometry.content),
            rect_to_layout(geometry.content),
            &root_space,
            PipelineId(page.pipeline.source(), page.pipeline.pipeline()),
            false,
        );
        primitives = checked_primitive_increment(primitives)?;
    }
    builder.push_iframe(full, full, &root_space, pipelines.chrome, false);
    primitives = checked_primitive_increment(primitives)?;
    let (_, display_list) = builder.end();
    if display_list.size_in_bytes() > MAX_BROWSER_ROOT_DISPLAY_LIST_BYTES {
        return Err(browser_resource_error(
            "serialized browser root display list exceeds its fixed limit",
        ));
    }
    Ok(BrowserBuiltDisplayList {
        pipeline: pipelines.root,
        display_list,
        primitive_count: primitives,
    })
}

fn prepared_chrome_entry(
    entries: &[PreparedSceneTextEntry],
    local_index: usize,
    first_staging_index: usize,
) -> Result<&PreparedSceneTextEntry, WebRenderWindowError> {
    let entry = entries.get(local_index).ok_or_else(|| {
        browser_contract_error(
            WebRenderWindowErrorKind::Text,
            "chrome prepared text partition ended early",
        )
    })?;
    let expected = first_staging_index
        .checked_add(local_index)
        .and_then(|index| u32::try_from(index).ok())
        .ok_or_else(|| browser_resource_error("chrome staging index overflowed"))?;
    if entry.pending_index() != expected {
        return Err(browser_contract_error(
            WebRenderWindowErrorKind::Text,
            "equal or repeated page/chrome text crossed its exact staging partition",
        ));
    }
    Ok(entry)
}

#[allow(clippy::cast_precision_loss)]
fn push_prepared_text(
    builder: &mut DisplayListBuilder,
    space: SpaceAndClipInfo,
    bounds: BrowserPhysicalRect,
    entry: &PreparedSceneTextEntry,
    color: ColorF,
    primitives: &mut usize,
) -> Result<(), WebRenderWindowError> {
    let bounds_layout = rect_to_layout(bounds);
    let common = CommonItemProperties::new(bounds_layout, space);
    for run in entry.runs() {
        let mut glyphs = Vec::new();
        glyphs
            .try_reserve_exact(run.glyphs().len())
            .map_err(|_| browser_resource_error("could not reserve positioned chrome glyphs"))?;
        glyphs.extend(run.glyphs().iter().map(|glyph| GlyphInstance {
            index: glyph.index,
            point: LayoutPoint::new(
                glyph.point.x + bounds.x as f32,
                glyph.point.y + bounds.y as f32,
            ),
        }));
        builder.push_text(
            &common,
            bounds_layout,
            &glyphs,
            run.font_instance(),
            color,
            None,
        );
        *primitives = checked_primitive_increment(*primitives)?;
    }
    Ok(())
}

fn push_colored_rect(
    builder: &mut DisplayListBuilder,
    space: SpaceAndClipInfo,
    rect: BrowserPhysicalRect,
    color: ColorF,
    primitives: &mut usize,
) -> Result<(), WebRenderWindowError> {
    if rect.width == 0 || rect.height == 0 {
        return Ok(());
    }
    let rect = rect_to_layout(rect);
    builder.push_rect(&CommonItemProperties::new(rect, space), rect, color);
    *primitives = checked_primitive_increment(*primitives)?;
    Ok(())
}

fn tab_background_color(selected: bool, interaction: BrowserElementInteraction) -> ColorF {
    match (selected, interaction) {
        (_, BrowserElementInteraction::Pressed) => ColorF::new(0.12, 0.40, 0.66, 1.0),
        (true, BrowserElementInteraction::Hovered) => ColorF::new(0.34, 0.39, 0.49, 1.0),
        (false, BrowserElementInteraction::Hovered) => ColorF::new(0.24, 0.28, 0.36, 1.0),
        (true, BrowserElementInteraction::Idle) => ColorF::new(0.27, 0.31, 0.40, 1.0),
        (false, BrowserElementInteraction::Idle) => ColorF::new(0.16, 0.18, 0.23, 1.0),
    }
}

fn address_background_color(
    availability: BrowserElementAvailability,
    interaction: BrowserElementInteraction,
) -> ColorF {
    match (availability, interaction) {
        (BrowserElementAvailability::Disabled, _) => ColorF::new(0.86, 0.87, 0.89, 1.0),
        (_, BrowserElementInteraction::Pressed) => ColorF::new(0.78, 0.87, 0.97, 1.0),
        (_, BrowserElementInteraction::Hovered) => ColorF::new(0.91, 0.94, 0.97, 1.0),
        (_, BrowserElementInteraction::Idle) => ColorF::new(0.98, 0.99, 1.0, 1.0),
    }
}

fn close_button_colors(
    selected: bool,
    availability: BrowserElementAvailability,
    interaction: BrowserElementInteraction,
) -> (ColorF, ColorF) {
    if availability == BrowserElementAvailability::Disabled {
        return (
            ColorF::new(0.20, 0.21, 0.25, 1.0),
            ColorF::new(0.48, 0.50, 0.55, 1.0),
        );
    }
    let background = match interaction {
        BrowserElementInteraction::Pressed => ColorF::new(0.64, 0.15, 0.20, 1.0),
        BrowserElementInteraction::Hovered => ColorF::new(0.55, 0.22, 0.26, 1.0),
        BrowserElementInteraction::Idle if selected => ColorF::new(0.46, 0.20, 0.23, 1.0),
        BrowserElementInteraction::Idle => ColorF::new(0.33, 0.18, 0.21, 1.0),
    };
    (background, ColorF::new(0.98, 0.98, 0.99, 1.0))
}

fn push_close_affordance(
    builder: &mut DisplayListBuilder,
    space: SpaceAndClipInfo,
    hit_rect: BrowserPhysicalRect,
    selected: bool,
    availability: BrowserElementAvailability,
    interaction: BrowserElementInteraction,
    primitives: &mut usize,
) -> Result<(), WebRenderWindowError> {
    if hit_rect.width == 0 || hit_rect.height == 0 {
        return Ok(());
    }
    let side = hit_rect.width.min(hit_rect.height).min(18);
    let button = BrowserPhysicalRect::new(
        hit_rect
            .x
            .saturating_add((hit_rect.width.saturating_sub(side)) / 2),
        hit_rect
            .y
            .saturating_add((hit_rect.height.saturating_sub(side)) / 2),
        side,
        side,
    );
    let (background, foreground) = close_button_colors(selected, availability, interaction);
    push_colored_rect(builder, space, button, background, primitives)?;
    if side < 3 {
        return Ok(());
    }

    let mark_side = side.saturating_sub(4).max(1);
    let mark_x = button
        .x
        .saturating_add((side.saturating_sub(mark_side)) / 2);
    let mark_y = button
        .y
        .saturating_add((side.saturating_sub(mark_side)) / 2);
    let steps = mark_side.min(7);
    let dot = (mark_side / steps).max(1);
    for step in 0..steps {
        let offset = if steps == 1 {
            0
        } else {
            step.saturating_mul(mark_side.saturating_sub(dot)) / (steps - 1)
        };
        for y in [
            mark_y.saturating_add(offset),
            mark_y.saturating_add(mark_side.saturating_sub(dot).saturating_sub(offset)),
        ] {
            push_colored_rect(
                builder,
                space,
                BrowserPhysicalRect::new(mark_x.saturating_add(offset), y, dot, dot),
                foreground,
                primitives,
            )?;
        }
    }
    Ok(())
}

fn push_primary_control(
    builder: &mut DisplayListBuilder,
    space: SpaceAndClipInfo,
    control: BrowserResolvedPrimaryControl,
    state: &BrowserPrimaryChromeState,
    primitives: &mut usize,
) -> Result<(), WebRenderWindowError> {
    let Some(rect) = control.rect() else {
        return Ok(());
    };
    let on_light = control.kind() == BrowserPrimaryControlKind::SiteIdentity;
    let background = primary_control_background(control, on_light);
    push_colored_rect(builder, space, rect, background, primitives)?;
    let icon = inset_all(rect, (rect.width().min(rect.height()) / 4).max(1));
    let enabled = control.availability() == BrowserElementAvailability::Enabled;
    let foreground = primary_control_foreground(enabled, on_light);
    match control.kind() {
        BrowserPrimaryControlKind::Back => push_arrow_icon(
            builder,
            space,
            icon,
            state.direction(),
            true,
            foreground,
            primitives,
        )?,
        BrowserPrimaryControlKind::Forward => push_arrow_icon(
            builder,
            space,
            icon,
            state.direction(),
            false,
            foreground,
            primitives,
        )?,
        BrowserPrimaryControlKind::ReloadStop => match state.reload_stop_mode() {
            BrowserReloadStopMode::Reload => {
                push_reload_icon(
                    builder,
                    space,
                    icon,
                    state.direction(),
                    foreground,
                    primitives,
                )?;
            }
            BrowserReloadStopMode::Stop => {
                push_stop_icon(builder, space, icon, foreground, primitives)?;
            }
        },
        BrowserPrimaryControlKind::SiteIdentity => push_site_identity_icon(
            builder,
            space,
            icon,
            state.site_identity(),
            enabled,
            primitives,
        )?,
        BrowserPrimaryControlKind::NewTab => {
            push_plus_icon(builder, space, icon, foreground, primitives)?;
        }
        BrowserPrimaryControlKind::AllTabs => {
            push_all_tabs_icon(builder, space, icon, foreground, primitives)?;
        }
        BrowserPrimaryControlKind::ApplicationMenu => {
            push_menu_icon(builder, space, icon, foreground, primitives)?;
        }
        BrowserPrimaryControlKind::Overflow => push_overflow_icon(
            builder,
            space,
            icon,
            state.direction(),
            foreground,
            primitives,
        )?,
        BrowserPrimaryControlKind::UrlBar => {}
    }
    if control.focused() {
        push_focus_ring(builder, space, rect, primitives)?;
    }
    Ok(())
}

fn primary_control_background(control: BrowserResolvedPrimaryControl, on_light: bool) -> ColorF {
    match (
        control.availability(),
        control.interaction(),
        control.open(),
    ) {
        (BrowserElementAvailability::Disabled, _, _) if on_light => {
            ColorF::new(0.90, 0.91, 0.93, 1.0)
        }
        (BrowserElementAvailability::Disabled, _, _) => ColorF::new(0.13, 0.15, 0.19, 1.0),
        (_, BrowserElementInteraction::Pressed, _) | (_, _, true) => {
            ColorF::new(0.12, 0.40, 0.66, 1.0)
        }
        (_, BrowserElementInteraction::Hovered, _) if on_light => {
            ColorF::new(0.82, 0.86, 0.91, 1.0)
        }
        (_, BrowserElementInteraction::Hovered, _) => ColorF::new(0.30, 0.34, 0.42, 1.0),
        _ if on_light => ColorF::new(0.96, 0.97, 0.98, 1.0),
        _ => ColorF::new(0.18, 0.21, 0.27, 1.0),
    }
}

fn primary_control_foreground(enabled: bool, on_light: bool) -> ColorF {
    if !enabled {
        if on_light {
            ColorF::new(0.48, 0.51, 0.56, 1.0)
        } else {
            ColorF::new(0.42, 0.45, 0.51, 1.0)
        }
    } else if on_light {
        ColorF::new(0.10, 0.13, 0.18, 1.0)
    } else {
        ColorF::new(0.93, 0.95, 0.98, 1.0)
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn push_primary_popup(
    builder: &mut DisplayListBuilder,
    space: SpaceAndClipInfo,
    clip_chain_id: webrender_api::ClipChainId,
    spatial_id: webrender_api::SpatialId,
    popup: &BrowserResolvedPrimaryPopup,
    direction: crate::primary_chrome::BrowserChromeDirection,
    entries: &[PreparedSceneTextEntry],
    text_index: &mut usize,
    first_staging_index: usize,
    surface: WebRenderSurfaceSnapshot,
    primitives: &mut usize,
) -> Result<(), WebRenderWindowError> {
    let full = BrowserPhysicalRect::new(0, 0, surface.size().width, surface.size().height);
    push_colored_rect(
        builder,
        space,
        full,
        ColorF::new(0.01, 0.02, 0.04, 0.16),
        primitives,
    )?;
    builder.push_hit_test(
        rect_to_layout(full),
        clip_chain_id,
        spatial_id,
        PrimitiveFlags::default(),
        (popup.anchor().get(), 40),
    );
    *primitives = checked_primitive_increment(*primitives)?;

    push_colored_rect(
        builder,
        space,
        popup.rect(),
        ColorF::new(0.12, 0.14, 0.18, 1.0),
        primitives,
    )?;
    push_focus_ring(builder, space, popup.rect(), primitives)?;
    builder.push_hit_test(
        rect_to_layout(popup.rect()),
        clip_chain_id,
        spatial_id,
        PrimitiveFlags::default(),
        (popup.anchor().get(), 41),
    );
    *primitives = checked_primitive_increment(*primitives)?;

    for row in popup.rows() {
        let entry = prepared_chrome_entry(entries, *text_index, first_staging_index)?;
        *text_index = text_index
            .checked_add(1)
            .ok_or_else(|| browser_resource_error("primary popup text index overflowed"))?;
        let Some(rect) = row.rect() else {
            continue;
        };
        let background = match (row.availability(), row.interaction(), row.selection()) {
            (_, BrowserElementInteraction::Pressed, _) => ColorF::new(0.12, 0.40, 0.66, 1.0),
            (_, BrowserElementInteraction::Hovered, _) => ColorF::new(0.25, 0.29, 0.36, 1.0),
            (_, _, BrowserElementSelection::Selected) => ColorF::new(0.20, 0.32, 0.45, 1.0),
            (BrowserElementAvailability::Disabled, _, _) => ColorF::new(0.14, 0.16, 0.20, 1.0),
            _ => ColorF::new(0.17, 0.19, 0.24, 1.0),
        };
        push_colored_rect(builder, space, rect, background, primitives)?;
        if row.selection() == BrowserElementSelection::Selected {
            let accent_width = 3_u32.min(rect.width());
            let accent_x = match direction {
                crate::primary_chrome::BrowserChromeDirection::LeftToRight => rect.x(),
                crate::primary_chrome::BrowserChromeDirection::RightToLeft => rect
                    .x()
                    .saturating_add(rect.width().saturating_sub(accent_width)),
            };
            push_colored_rect(
                builder,
                space,
                BrowserPhysicalRect::new(accent_x, rect.y(), accent_width, rect.height()),
                ColorF::new(0.20, 0.66, 0.96, 1.0),
                primitives,
            )?;
        }
        let label_color = if row.availability() == BrowserElementAvailability::Disabled {
            ColorF::new(0.53, 0.56, 0.62, 1.0)
        } else {
            ColorF::new(0.95, 0.96, 0.98, 1.0)
        };
        push_prepared_text(
            builder,
            space,
            inset_all(rect, 10),
            entry,
            label_color,
            primitives,
        )?;
        if row.expansion() != BrowserElementExpansion::Leaf {
            return Err(browser_contract_error(
                WebRenderWindowErrorKind::Contract,
                "unvalidated popup expansion reached display-list construction",
            ));
        }
        if row.focused() {
            push_focus_ring(builder, space, rect, primitives)?;
        }
        builder.push_hit_test(
            rect_to_layout(rect),
            clip_chain_id,
            spatial_id,
            PrimitiveFlags::default(),
            (row.element().get(), 42),
        );
        *primitives = checked_primitive_increment(*primitives)?;
    }
    let first = popup.first_visible_row();
    let visible_end = first.saturating_add(popup.visible_row_count());
    if first > 0 {
        push_scroll_indicator(builder, space, popup.rect(), true, primitives)?;
    }
    if visible_end < popup.rows().len() {
        push_scroll_indicator(builder, space, popup.rect(), false, primitives)?;
    }
    Ok(())
}

fn push_arrow_icon(
    builder: &mut DisplayListBuilder,
    space: SpaceAndClipInfo,
    rect: BrowserPhysicalRect,
    direction: crate::primary_chrome::BrowserChromeDirection,
    back: bool,
    color: ColorF,
    primitives: &mut usize,
) -> Result<(), WebRenderWindowError> {
    let points_left = matches!(
        (direction, back),
        (
            crate::primary_chrome::BrowserChromeDirection::LeftToRight,
            true
        ) | (
            crate::primary_chrome::BrowserChromeDirection::RightToLeft,
            false
        )
    );
    let thickness = (rect.height() / 6).max(1).min(rect.height());
    let shaft_width = rect.width().saturating_sub(rect.width() / 3).max(1);
    let shaft_x = if points_left {
        rect.x().saturating_add(rect.width() / 4)
    } else {
        rect.x()
    };
    push_colored_rect(
        builder,
        space,
        BrowserPhysicalRect::new(
            shaft_x,
            rect.y()
                .saturating_add(rect.height().saturating_sub(thickness) / 2),
            shaft_width.min(rect.width()),
            thickness,
        ),
        color,
        primitives,
    )?;
    let steps = 4_u32.min(rect.width().max(1)).min(rect.height().max(1));
    let dot = thickness.min(rect.width()).max(1);
    for step in 0..steps {
        let x_offset = step.saturating_mul(rect.width().saturating_sub(dot)) / steps.max(1);
        let x = if points_left {
            rect.x().saturating_add(x_offset)
        } else {
            rect.x()
                .saturating_add(rect.width().saturating_sub(dot).saturating_sub(x_offset))
        };
        let spread = step.saturating_mul(rect.height().saturating_sub(dot)) / steps.max(1);
        for y in [
            rect.y()
                .saturating_add(rect.height() / 2)
                .saturating_sub(spread / 2),
            rect.y()
                .saturating_add(rect.height() / 2)
                .saturating_add(spread / 2)
                .saturating_sub(dot),
        ] {
            push_colored_rect(
                builder,
                space,
                BrowserPhysicalRect::new(x, y, dot, dot),
                color,
                primitives,
            )?;
        }
    }
    Ok(())
}

fn push_reload_icon(
    builder: &mut DisplayListBuilder,
    space: SpaceAndClipInfo,
    rect: BrowserPhysicalRect,
    direction: crate::primary_chrome::BrowserChromeDirection,
    color: ColorF,
    primitives: &mut usize,
) -> Result<(), WebRenderWindowError> {
    let thickness = (rect.width().min(rect.height()) / 6).max(1);
    for edge in [
        BrowserPhysicalRect::new(rect.x(), rect.y(), rect.width(), thickness),
        BrowserPhysicalRect::new(rect.x(), rect.y(), thickness, rect.height()),
        BrowserPhysicalRect::new(
            rect.x(),
            rect.y()
                .saturating_add(rect.height().saturating_sub(thickness)),
            rect.width().saturating_sub(rect.width() / 3),
            thickness,
        ),
        BrowserPhysicalRect::new(
            rect.x()
                .saturating_add(rect.width().saturating_sub(thickness)),
            rect.y(),
            thickness,
            rect.height().saturating_sub(rect.height() / 3),
        ),
    ] {
        let edge = mirror_directional_rect(rect, edge, direction);
        push_colored_rect(builder, space, edge, color, primitives)?;
    }
    let arrow = BrowserPhysicalRect::new(
        rect.x()
            .saturating_add(rect.width().saturating_sub(thickness.saturating_mul(3))),
        rect.y(),
        thickness.saturating_mul(3).min(rect.width()),
        thickness.saturating_mul(3).min(rect.height()),
    );
    push_colored_rect(
        builder,
        space,
        mirror_directional_rect(rect, arrow, direction),
        color,
        primitives,
    )
}

fn mirror_directional_rect(
    container: BrowserPhysicalRect,
    rect: BrowserPhysicalRect,
    direction: crate::primary_chrome::BrowserChromeDirection,
) -> BrowserPhysicalRect {
    if direction == crate::primary_chrome::BrowserChromeDirection::LeftToRight {
        return rect;
    }
    let relative_x = rect.x().saturating_sub(container.x());
    BrowserPhysicalRect::new(
        container.x().saturating_add(
            container
                .width()
                .saturating_sub(relative_x)
                .saturating_sub(rect.width()),
        ),
        rect.y(),
        rect.width(),
        rect.height(),
    )
}

fn push_stop_icon(
    builder: &mut DisplayListBuilder,
    space: SpaceAndClipInfo,
    rect: BrowserPhysicalRect,
    color: ColorF,
    primitives: &mut usize,
) -> Result<(), WebRenderWindowError> {
    let inset = rect.width().min(rect.height()) / 5;
    push_colored_rect(builder, space, inset_all(rect, inset), color, primitives)
}

fn push_site_identity_icon(
    builder: &mut DisplayListBuilder,
    space: SpaceAndClipInfo,
    rect: BrowserPhysicalRect,
    kind: BrowserSiteIdentityKind,
    enabled: bool,
    primitives: &mut usize,
) -> Result<(), WebRenderWindowError> {
    let color = if !enabled || kind == BrowserSiteIdentityKind::Empty {
        ColorF::new(0.48, 0.51, 0.56, 1.0)
    } else {
        match kind {
            BrowserSiteIdentityKind::Internal | BrowserSiteIdentityKind::Secure => {
                ColorF::new(0.08, 0.45, 0.29, 1.0)
            }
            BrowserSiteIdentityKind::LoopbackHttp => ColorF::new(0.13, 0.38, 0.62, 1.0),
            BrowserSiteIdentityKind::Insecure => ColorF::new(0.70, 0.24, 0.20, 1.0),
            BrowserSiteIdentityKind::Mixed => ColorF::new(0.77, 0.48, 0.10, 1.0),
            BrowserSiteIdentityKind::Empty => ColorF::new(0.48, 0.51, 0.56, 1.0),
        }
    };
    let third = (rect.width() / 3).max(1);
    let body = BrowserPhysicalRect::new(
        rect.x().saturating_add(third / 2),
        rect.y(),
        rect.width().saturating_sub(third),
        rect.height().saturating_sub(rect.height() / 4),
    );
    push_colored_rect(builder, space, body, color, primitives)?;
    push_colored_rect(
        builder,
        space,
        BrowserPhysicalRect::new(
            rect.x().saturating_add(third),
            body.y().saturating_add(body.height()),
            rect.width().saturating_sub(third.saturating_mul(2)),
            rect.height().saturating_sub(body.height()),
        ),
        color,
        primitives,
    )
}

fn push_plus_icon(
    builder: &mut DisplayListBuilder,
    space: SpaceAndClipInfo,
    rect: BrowserPhysicalRect,
    color: ColorF,
    primitives: &mut usize,
) -> Result<(), WebRenderWindowError> {
    let thickness = (rect.width().min(rect.height()) / 6).max(1);
    push_colored_rect(
        builder,
        space,
        BrowserPhysicalRect::new(
            rect.x(),
            rect.y()
                .saturating_add(rect.height().saturating_sub(thickness) / 2),
            rect.width(),
            thickness,
        ),
        color,
        primitives,
    )?;
    push_colored_rect(
        builder,
        space,
        BrowserPhysicalRect::new(
            rect.x()
                .saturating_add(rect.width().saturating_sub(thickness) / 2),
            rect.y(),
            thickness,
            rect.height(),
        ),
        color,
        primitives,
    )
}

fn push_all_tabs_icon(
    builder: &mut DisplayListBuilder,
    space: SpaceAndClipInfo,
    rect: BrowserPhysicalRect,
    color: ColorF,
    primitives: &mut usize,
) -> Result<(), WebRenderWindowError> {
    let thickness = (rect.height() / 7).max(1);
    for row in 0..2_u32 {
        push_colored_rect(
            builder,
            space,
            BrowserPhysicalRect::new(
                rect.x(),
                rect.y()
                    .saturating_add(row.saturating_mul(thickness.saturating_mul(3))),
                rect.width(),
                thickness,
            ),
            color,
            primitives,
        )?;
    }
    let arrow_width = rect.width() / 2;
    push_colored_rect(
        builder,
        space,
        BrowserPhysicalRect::new(
            rect.x()
                .saturating_add((rect.width().saturating_sub(arrow_width)) / 2),
            rect.y()
                .saturating_add(rect.height().saturating_sub(thickness.saturating_mul(2))),
            arrow_width,
            thickness.saturating_mul(2).min(rect.height()),
        ),
        color,
        primitives,
    )
}

fn push_menu_icon(
    builder: &mut DisplayListBuilder,
    space: SpaceAndClipInfo,
    rect: BrowserPhysicalRect,
    color: ColorF,
    primitives: &mut usize,
) -> Result<(), WebRenderWindowError> {
    let thickness = (rect.height() / 8).max(1);
    for row in 0..3_u32 {
        let y = rect
            .y()
            .saturating_add(row.saturating_mul(rect.height().saturating_sub(thickness)) / 2);
        push_colored_rect(
            builder,
            space,
            BrowserPhysicalRect::new(rect.x(), y, rect.width(), thickness),
            color,
            primitives,
        )?;
    }
    Ok(())
}

fn push_overflow_icon(
    builder: &mut DisplayListBuilder,
    space: SpaceAndClipInfo,
    rect: BrowserPhysicalRect,
    direction: crate::primary_chrome::BrowserChromeDirection,
    color: ColorF,
    primitives: &mut usize,
) -> Result<(), WebRenderWindowError> {
    let half = rect.width() / 2;
    for offset in [0, half] {
        let chevron = match direction {
            crate::primary_chrome::BrowserChromeDirection::LeftToRight => BrowserPhysicalRect::new(
                rect.x().saturating_add(offset),
                rect.y(),
                half.max(1),
                rect.height(),
            ),
            crate::primary_chrome::BrowserChromeDirection::RightToLeft => BrowserPhysicalRect::new(
                rect.x()
                    .saturating_add(rect.width().saturating_sub(offset).saturating_sub(half)),
                rect.y(),
                half.max(1),
                rect.height(),
            ),
        };
        push_arrow_icon(builder, space, chevron, direction, false, color, primitives)?;
    }
    Ok(())
}

fn push_scroll_indicator(
    builder: &mut DisplayListBuilder,
    space: SpaceAndClipInfo,
    panel: BrowserPhysicalRect,
    top: bool,
    primitives: &mut usize,
) -> Result<(), WebRenderWindowError> {
    let height = 3_u32.min(panel.height());
    let width = (panel.width() / 4).max(1);
    let y = if top {
        panel.y()
    } else {
        panel
            .y()
            .saturating_add(panel.height().saturating_sub(height))
    };
    push_colored_rect(
        builder,
        space,
        BrowserPhysicalRect::new(
            panel
                .x()
                .saturating_add(panel.width().saturating_sub(width) / 2),
            y,
            width,
            height,
        ),
        ColorF::new(0.20, 0.66, 0.96, 1.0),
        primitives,
    )
}

fn push_focus_ring(
    builder: &mut DisplayListBuilder,
    space: SpaceAndClipInfo,
    rect: BrowserPhysicalRect,
    primitives: &mut usize,
) -> Result<(), WebRenderWindowError> {
    if rect.width == 0 || rect.height == 0 {
        return Ok(());
    }
    let thickness = 2_u32.min(rect.width).min(rect.height);
    let color = ColorF::new(0.23, 0.64, 1.0, 1.0);
    for edge in [
        BrowserPhysicalRect::new(rect.x, rect.y, rect.width, thickness),
        BrowserPhysicalRect::new(
            rect.x,
            rect.y.saturating_add(rect.height.saturating_sub(thickness)),
            rect.width,
            thickness,
        ),
        BrowserPhysicalRect::new(rect.x, rect.y, thickness, rect.height),
        BrowserPhysicalRect::new(
            rect.x.saturating_add(rect.width.saturating_sub(thickness)),
            rect.y,
            thickness,
            rect.height,
        ),
    ] {
        push_colored_rect(builder, space, edge, color, primitives)?;
    }
    if *primitives > MAX_BROWSER_CHROME_GLYPHS {
        return Err(browser_resource_error(
            "browser chrome primitive accounting exceeds its fixed limit",
        ));
    }
    Ok(())
}

#[allow(clippy::cast_precision_loss)]
fn address_selection_rect(scene: &BrowserChromeScene) -> Option<BrowserPhysicalRect> {
    let bounds = inset_all(scene.address_field(), 8);
    if bounds.width == 0 || bounds.height == 0 {
        return None;
    }
    let range = scene.state.address_selection.normalized();
    let shaped = &scene.state.address;
    let full_width = shaped.metrics().full_width().max(0.0);
    let (start, end) = if range.is_empty() {
        let x = shaped_caret_x(shaped, range.start);
        (x, x + 2.0)
    } else {
        let mut start = f32::INFINITY;
        let mut end = f32::NEG_INFINITY;
        for run in shaped.runs() {
            for cluster in run.clusters() {
                let cluster_range = cluster.text_range();
                if cluster_range.start < range.end && cluster_range.end > range.start {
                    start = start.min(cluster.x());
                    end = end.max(cluster.x() + cluster.advance());
                }
            }
        }
        if !start.is_finite() || !end.is_finite() {
            let text_len = shaped.text().len().max(1);
            #[allow(clippy::cast_precision_loss)]
            let start_ratio = range.start as f32 / text_len as f32;
            #[allow(clippy::cast_precision_loss)]
            let end_ratio = range.end as f32 / text_len as f32;
            (full_width * start_ratio, full_width * end_ratio)
        } else {
            (start, end)
        }
    };
    let width = bounds.width as f32;
    let start = start.max(0.0).min(width - 1.0).floor();
    let end = end.max(start + 1.0).min(width).ceil().min(width);
    let painted_width = (end - start).max(1.0).min(width - start);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Some(BrowserPhysicalRect::new(
        bounds.x.saturating_add(start as u32),
        bounds.y,
        painted_width as u32,
        bounds.height,
    ))
}

#[allow(clippy::cast_precision_loss)]
fn shaped_caret_x(shaped: &ShapedText, offset: usize) -> f32 {
    let full_width = shaped.metrics().full_width().max(0.0);
    if offset == 0 {
        return 0.0;
    }
    if offset >= shaped.text().len() {
        return full_width;
    }

    let mut nearest: Option<(usize, usize, f32)> = None;
    for run in shaped.runs() {
        for cluster in run.clusters() {
            let range = cluster.text_range();
            let visual_start = cluster.x();
            let visual_end = cluster.x() + cluster.advance();
            let (logical_start_x, logical_end_x) = match run.direction() {
                RunDirection::LeftToRight => (visual_start, visual_end),
                RunDirection::RightToLeft => (visual_end, visual_start),
            };
            if offset == range.start {
                return logical_start_x.clamp(0.0, full_width);
            }
            if offset == range.end {
                return logical_end_x.clamp(0.0, full_width);
            }
            if range.start < offset && offset < range.end {
                let ratio = (offset - range.start) as f32 / (range.end - range.start) as f32;
                return (logical_start_x + (logical_end_x - logical_start_x) * ratio)
                    .clamp(0.0, full_width);
            }
            for (boundary, x) in [(range.start, logical_start_x), (range.end, logical_end_x)] {
                let candidate = (offset.abs_diff(boundary), boundary, x);
                if nearest.is_none_or(|current| (candidate.0, candidate.1) < (current.0, current.1))
                {
                    nearest = Some(candidate);
                }
            }
        }
    }
    nearest.map_or_else(
        || {
            if offset <= shaped.text().len() / 2 {
                0.0
            } else {
                full_width
            }
        },
        |(_, _, x)| x.clamp(0.0, full_width),
    )
}

fn inset_all(rect: BrowserPhysicalRect, inset: u32) -> BrowserPhysicalRect {
    let x_inset = inset.min(rect.width / 2);
    let y_inset = inset.min(rect.height / 2);
    BrowserPhysicalRect::new(
        rect.x.saturating_add(x_inset),
        rect.y.saturating_add(y_inset),
        rect.width.saturating_sub(x_inset.saturating_mul(2)),
        rect.height.saturating_sub(y_inset.saturating_mul(2)),
    )
}

fn inset_right(rect: BrowserPhysicalRect, right: u32) -> BrowserPhysicalRect {
    let right = right.min(rect.width);
    let available = rect.width.saturating_sub(right);
    let left = 8.min(available / 4);
    let gap = 4.min(available.saturating_sub(left) / 3);
    BrowserPhysicalRect::new(
        rect.x.saturating_add(left),
        rect.y,
        available.saturating_sub(left).saturating_sub(gap),
        rect.height,
    )
}

fn checked_primitive_increment(value: usize) -> Result<usize, WebRenderWindowError> {
    value
        .checked_add(1)
        .filter(|value| *value <= MAX_BROWSER_CHROME_GLYPHS)
        .ok_or_else(|| browser_resource_error("browser primitive accounting exceeded its limit"))
}

#[allow(clippy::cast_precision_loss)]
fn rect_to_layout(rect: BrowserPhysicalRect) -> LayoutRect {
    LayoutRect::from_origin_and_size(
        LayoutPoint::new(rect.x as f32, rect.y as f32),
        LayoutSize::new(rect.width as f32, rect.height as f32),
    )
}

pub(crate) fn browser_contract_error(
    kind: WebRenderWindowErrorKind,
    detail: &'static str,
) -> WebRenderWindowError {
    WebRenderWindowError::new(WebRenderWindowFailureStage::ValidateRequest, kind, detail)
}

pub(crate) fn browser_resource_error(detail: &'static str) -> WebRenderWindowError {
    browser_contract_error(WebRenderWindowErrorKind::ResourceLimit, detail)
}

fn browser_hit_error(detail: &'static str) -> WebRenderWindowError {
    WebRenderWindowError::new(
        WebRenderWindowFailureStage::HitTest,
        WebRenderWindowErrorKind::StaleComposition,
        detail,
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use webrender_api::units::DeviceIntSize;
    use webrender_api::{ColorF, DisplayItem, DisplayListBuilder, PipelineId, SpaceAndClipInfo};
    use wild_buzzard_dom::{Document, DocumentVersion};
    use wild_buzzard_layout::{Au, LayoutOutput, Size, Viewport};
    use wild_buzzard_platform::{
        PhysicalPoint, PhysicalSize, PixelFormat, ScaleFactor, SurfaceDescriptor,
        SurfaceIdAllocator, SurfaceNamespace, SurfaceRole,
    };
    use wild_buzzard_renderer::{CompileRequest, PipelineKey, SceneCompiler};
    use wild_buzzard_text::{RunDirection, TextLimits, TextRequest, TextSystem};
    use wild_buzzard_text_webrender::ShapedSceneText;

    use super::{
        BrowserCandidate, BrowserCaptureCopyError, BrowserChromeGeometry, BrowserChromeHitMap,
        BrowserChromeRevision, BrowserChromeScene, BrowserChromeState, BrowserChromeTab,
        BrowserCompositorContract, BrowserFrameAccounting, BrowserFrameReceipt,
        BrowserFrameRequest, BrowserHitTarget, BrowserNavigationIdentity, BrowserPageIdentity,
        BrowserPageScene, BrowserPageSceneRevision, BrowserPageSnapshot, BrowserPageUpdate,
        BrowserPhysicalRect, BrowserPipelines, BrowserTabHitRegion, BrowserTabIdentity,
        MAX_BROWSER_CAPTURE_BYTES, MAX_BROWSER_CAPTURE_DIMENSION, MAX_BROWSER_CAPTURE_PIXELS,
        MAX_BROWSER_CHROME_TABS, MappedBrowserCapture, PreparedBrowserCapture,
        address_background_color, address_selection_rect, allocate_browser_capture_pixels,
        browser_capture_layout, build_browser_root_display_list, close_button_colors, inset_right,
        push_close_affordance, push_reload_icon, shaped_caret_x, stage_browser_texts,
        tab_background_color,
    };
    use crate::primary_chrome::{
        BrowserChromeDirection, BrowserChromeElementIdentity, BrowserElementAvailability,
        BrowserElementInteraction, BrowserPrimaryChromeState, BrowserPrimaryControl,
        BrowserPrimaryControlKind, BrowserPrimaryLayoutPreview, BrowserPrimaryPopup,
        BrowserPrimaryPopupKind, BrowserPrimaryPopupRow, BrowserPrimaryPopupRowKind,
        BrowserReloadStopMode, BrowserSiteIdentityKind,
    };
    use crate::{LinuxAccelerationClass, LinuxPresentationCapabilities, LinuxResetProtection};
    use crate::{WebRenderSurfaceSnapshot, WebRenderWindowErrorKind, WebRenderWindowFailureStage};

    fn surface(width: u32, height: u32, scale: f64) -> WebRenderSurfaceSnapshot {
        surface_in_namespace(6_004, width, height, scale)
    }

    fn surface_in_namespace(
        namespace: u64,
        width: u32,
        height: u32,
        scale: f64,
    ) -> WebRenderSurfaceSnapshot {
        let mut allocator = SurfaceIdAllocator::new(
            SurfaceNamespace::new(namespace).expect("nonzero surface namespace"),
        );
        WebRenderSurfaceSnapshot::initial(SurfaceDescriptor {
            id: allocator.allocate().expect("surface identity"),
            size: PhysicalSize::new(width, height).expect("bounded surface"),
            scale: ScaleFactor::new(scale).expect("valid scale"),
            format: PixelFormat::Rgba8Srgb,
            role: SurfaceRole::Window,
        })
    }

    fn page_identity(revision: u64, pipeline: PipelineKey) -> BrowserPageIdentity {
        BrowserPageIdentity {
            navigation: BrowserNavigationIdentity::new(10).expect("navigation identity"),
            revision: BrowserPageSceneRevision::new(revision).expect("page revision"),
            document: Document::new().version(),
            pipeline,
        }
    }

    fn simple_chrome(surface: WebRenderSurfaceSnapshot, revision: u64) -> BrowserChromeScene {
        let mut text = TextSystem::new_deterministic(TextLimits::default())
            .expect("deterministic test font initializes");
        let address = text
            .shape(&TextRequest::new("https://example.test/", 14.0))
            .expect("address shapes");
        BrowserChromeScene::new(
            BrowserChromeRevision::new(revision).expect("chrome revision"),
            surface,
            BrowserChromeState::new(Box::new([]), None, address),
        )
        .expect("simple chrome")
    }

    fn capture_receipt(
        request: BrowserFrameRequest,
        page_epoch: Option<u32>,
        chrome_epoch: u32,
        publish_id: u64,
    ) -> BrowserFrameReceipt {
        let bytes = u64::from(request.surface().size().width)
            * u64::from(request.surface().size().height)
            * 4;
        BrowserFrameReceipt {
            request,
            page_epoch,
            chrome_epoch,
            backend_publish_id: publish_id,
            rgba8_byte_equivalent: bytes,
            page_display_list_bytes: usize::from(page_epoch.is_some()),
            chrome_display_list_bytes: 1,
            root_display_list_bytes: 1,
            chrome_primitives: 1,
        }
    }

    fn mapped_capture(request: BrowserFrameRequest) -> MappedBrowserCapture {
        let geometry = BrowserChromeGeometry::for_surface(request.surface())
            .expect("capture geometry must be nonempty");
        PreparedBrowserCapture::new(request, geometry)
            .expect("capture buffer preflight")
            .into_mapped()
    }

    fn primary_controls(
        text: &mut TextSystem,
        preview: &BrowserPrimaryLayoutPreview,
    ) -> Box<[BrowserPrimaryControl]> {
        BrowserPrimaryControlKind::ALL
            .into_iter()
            .enumerate()
            .map(|(index, kind)| {
                let availability = if kind == BrowserPrimaryControlKind::Overflow
                    && (preview.hidden_controls().is_empty() || preview.popup_row_capacity() == 0)
                    || preview.popup_row_capacity() == 0
                        && matches!(
                            kind,
                            BrowserPrimaryControlKind::SiteIdentity
                                | BrowserPrimaryControlKind::AllTabs
                                | BrowserPrimaryControlKind::ApplicationMenu
                        ) {
                    BrowserElementAvailability::Disabled
                } else {
                    BrowserElementAvailability::Enabled
                };
                let label = text
                    .shape(&TextRequest::new(format!("{kind:?}"), 14.0))
                    .expect("control label shapes");
                BrowserPrimaryControl::new(
                    BrowserChromeElementIdentity::new(
                        100 + u64::try_from(index).expect("small index"),
                    )
                    .expect("control identity"),
                    kind,
                    label,
                    availability,
                )
            })
            .collect::<Vec<_>>()
            .into_boxed_slice()
    }

    fn empty_page_scene(
        surface: WebRenderSurfaceSnapshot,
        revision: u64,
        pipeline: PipelineKey,
    ) -> BrowserPageScene {
        let geometry = BrowserChromeGeometry::for_surface(surface).expect("browser geometry");
        let width = i32::try_from(geometry.content().width()).expect("small fixture width");
        let height = i32::try_from(geometry.content().height()).expect("small fixture height");
        let document = Document::new();
        let layout = LayoutOutput {
            document_version: document.version(),
            viewport: Viewport::from_css_pixels(width, height),
            root: None,
            boxes: Vec::new(),
            content_size: Size {
                width: Au::from_px(width),
                height: Au::from_px(height),
            },
            warnings: Vec::new(),
        };
        let scene = SceneCompiler::default()
            .compile(&layout, CompileRequest::new(document.version(), pipeline))
            .expect("empty page scene compiles");
        BrowserPageScene::new(
            BrowserNavigationIdentity::new(10).expect("navigation identity"),
            BrowserPageSceneRevision::new(revision).expect("page revision"),
            scene,
            Box::new([]),
        )
        .expect("empty scene has no shaped text")
    }

    struct SeededContract {
        contract: BrowserCompositorContract,
        surface: WebRenderSurfaceSnapshot,
        geometry: BrowserChromeGeometry,
        page: BrowserPageIdentity,
        chrome: BrowserChromeRevision,
        pipelines: BrowserPipelines,
        receipt: BrowserFrameReceipt,
    }

    fn seeded_contract() -> SeededContract {
        let surface = surface(800, 600, 1.0);
        let geometry = BrowserChromeGeometry::for_surface(surface).expect("browser geometry");
        let page = page_identity(1, PipelineKey::new(40, 7));
        let chrome = BrowserChromeRevision::new(1).expect("chrome revision");
        let tab = BrowserTabIdentity::new(19).expect("tab identity");
        let tab_rect = geometry.tab_rect(0, 1);
        let hit_map = BrowserChromeHitMap {
            geometry,
            tabs: vec![BrowserTabHitRegion {
                identity: tab,
                rect: tab_rect,
                close: geometry.tab_close_rect(tab_rect),
            }]
            .into_boxed_slice(),
            status_visible: false,
            primary: None,
        };
        let request =
            BrowserFrameRequest::new(surface, BrowserPageSnapshot::Scene(page), chrome, 1, 1);
        let mut contract = BrowserCompositorContract::default();
        let receipt = contract.commit_success(
            BrowserCandidate {
                page: BrowserPageSnapshot::Scene(page),
                previous_page: BrowserPageSnapshot::Blank,
                page_epoch: Some(1),
                chrome_revision: chrome,
                chrome_epoch: 1,
                page_replaced: true,
                chrome_replaced: true,
            },
            request,
            hit_map,
            BrowserFrameAccounting {
                backend_publish_id: 3,
                rgba8_byte_equivalent: 1_920_000,
                page_display_list_bytes: 40,
                chrome_display_list_bytes: 50,
                root_display_list_bytes: 60,
                chrome_primitives: 7,
            },
        );
        SeededContract {
            contract,
            surface,
            geometry,
            page,
            chrome,
            pipelines: BrowserPipelines::new(55),
            receipt,
        }
    }

    #[test]
    fn geometry_is_exactly_scaled_and_clipped_to_small_surfaces() {
        let normal = BrowserChromeGeometry::for_surface(surface(1_280, 720, 2.0))
            .expect("normal chrome geometry");
        assert_eq!(normal.tab_strip().height(), 72);
        assert_eq!(normal.address_strip().height(), 88);
        assert_eq!(
            normal.content().size().expect("nonempty content").height,
            560
        );
        assert_eq!(
            normal.content(),
            BrowserPhysicalRect::new(0, 160, 1_280, 560)
        );

        let tiny = BrowserChromeGeometry::for_surface(surface(90, 30, 1.0))
            .expect("tiny chrome remains clipped");
        assert_eq!(tiny.content().height(), 0);
        assert!(tiny.content().size().is_none());
        assert_eq!(
            tiny.tab_strip().height() + tiny.address_strip().height(),
            30
        );
    }

    #[test]
    fn receipt_bound_bgra_capture_preserves_top_left_rows_and_scaled_content_crop() {
        let surface = surface(12, 200, 2.0);
        let geometry = BrowserChromeGeometry::for_surface(surface).expect("scaled geometry");
        let request = BrowserFrameRequest::new(
            surface,
            BrowserPageSnapshot::Blank,
            BrowserChromeRevision::new(1).expect("chrome revision"),
            1,
            1,
        );
        let mut prepared =
            PreparedBrowserCapture::new(request, geometry).expect("capture preflight");
        let stride = prepared.stride();
        for (y, row) in prepared.pixels_mut().chunks_exact_mut(stride).enumerate() {
            for (x, pixel) in row.chunks_exact_mut(4).enumerate() {
                pixel.copy_from_slice(&[
                    u8::try_from(y).expect("fixture y fits u8"),
                    u8::try_from(x).expect("fixture x fits u8"),
                    0xa5,
                    0xff,
                ]);
            }
        }
        let mapped = prepared.into_mapped();
        mapped
            .validate_completion(request, None, 1, 7)
            .expect("exact completion identity");
        let capture = mapped.bind(capture_receipt(request, None, 1, 7));

        assert_eq!(capture.size(), surface.size());
        assert_eq!(capture.stride(), 48);
        assert_eq!(capture.pixels().len(), 9_600);
        assert_eq!(
            &capture.row(0).expect("first row")[..4],
            &[0, 0, 0xa5, 0xff]
        );
        assert_eq!(
            &capture.row(199).expect("last row")[44..48],
            &[199, 11, 0xa5, 0xff]
        );
        assert_eq!(capture.content_rect(), geometry.content());
        assert_eq!(
            capture.content_rect(),
            BrowserPhysicalRect::new(0, 160, 12, 40)
        );
        let content = capture.content();
        assert_eq!(
            &content.row(0).expect("first content row")[..4],
            &[160, 0, 0xa5, 0xff]
        );
        assert_eq!(
            &content.row(39).expect("last content row")[44..48],
            &[199, 11, 0xa5, 0xff]
        );
        assert!(content.row(40).is_none());
        assert!(!capture.desktop_compositor_acknowledged());

        let destination_stride = content.row_bytes() + 3;
        let destination_len = destination_stride * 39 + content.row_bytes();
        let mut destination = vec![0xcc; destination_len];
        content
            .copy_to(&mut destination, destination_stride)
            .expect("checked padded crop copy");
        assert_eq!(&destination[..4], &[160, 0, 0xa5, 0xff]);
        assert_eq!(destination[content.row_bytes()], 0xcc);
        assert_eq!(
            content.copy_to(&mut destination, content.row_bytes() - 1),
            Err(BrowserCaptureCopyError::StrideTooSmall {
                minimum: content.row_bytes(),
                supplied: content.row_bytes() - 1,
            })
        );
        destination.push(0);
        assert!(matches!(
            content.copy_to(&mut destination, destination_stride),
            Err(BrowserCaptureCopyError::LengthMismatch { .. })
        ));
    }

    #[test]
    fn capture_preflight_rejects_zero_tiny_empty_outside_and_fixed_caps() {
        let cases = [
            (
                PhysicalSize {
                    width: 0,
                    height: 100,
                },
                BrowserPhysicalRect::new(0, 1, 1, 1),
                WebRenderWindowErrorKind::SizeMismatch,
            ),
            (
                PhysicalSize {
                    width: 1,
                    height: 100,
                },
                BrowserPhysicalRect::new(0, 1, 1, 1),
                WebRenderWindowErrorKind::SizeMismatch,
            ),
            (
                PhysicalSize {
                    width: 100,
                    height: 100,
                },
                BrowserPhysicalRect::new(0, 80, 100, 0),
                WebRenderWindowErrorKind::SizeMismatch,
            ),
            (
                PhysicalSize {
                    width: 100,
                    height: 100,
                },
                BrowserPhysicalRect::new(99, 80, 2, 20),
                WebRenderWindowErrorKind::CaptureIdentityMismatch,
            ),
            (
                PhysicalSize {
                    width: MAX_BROWSER_CAPTURE_DIMENSION + 1,
                    height: 2,
                },
                BrowserPhysicalRect::new(0, 1, 2, 1),
                WebRenderWindowErrorKind::ResourceLimit,
            ),
            (
                PhysicalSize {
                    width: MAX_BROWSER_CAPTURE_DIMENSION,
                    height: MAX_BROWSER_CAPTURE_DIMENSION,
                },
                BrowserPhysicalRect::new(0, 1, 2, 1),
                WebRenderWindowErrorKind::ResourceLimit,
            ),
        ];
        for (size, content, kind) in cases {
            let error = browser_capture_layout(size, content).expect_err("capture must reject");
            assert_eq!(error.stage(), WebRenderWindowFailureStage::PrepareCapture);
            assert_eq!(error.kind(), kind);
        }
        let maximum = browser_capture_layout(
            PhysicalSize {
                width: MAX_BROWSER_CAPTURE_DIMENSION,
                height: u32::try_from(
                    MAX_BROWSER_CAPTURE_PIXELS / u64::from(MAX_BROWSER_CAPTURE_DIMENSION),
                )
                .expect("fixed maximum height fits u32"),
            },
            BrowserPhysicalRect::new(0, 1, 2, 1),
        )
        .expect("exact fixed pixel/byte limit is representable without allocating");
        assert_eq!(
            u64::try_from(maximum.byte_len).expect("byte length fits u64"),
            MAX_BROWSER_CAPTURE_BYTES
        );

        let error = allocate_browser_capture_pixels(64, |_, _| Err(()))
            .expect_err("synthetic allocation failure must be typed");
        assert_eq!(error.stage(), WebRenderWindowFailureStage::PrepareCapture);
        assert_eq!(
            error.kind(),
            WebRenderWindowErrorKind::CaptureAllocationFailed
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn mapped_capture_rejects_every_foreign_or_replayed_receipt_component() {
        let expected_surface = surface(100, 120, 1.0);
        let expected_page = page_identity(1, PipelineKey::new(41, 7));
        let expected = BrowserFrameRequest::new(
            expected_surface,
            BrowserPageSnapshot::Scene(expected_page),
            BrowserChromeRevision::new(4).expect("chrome revision"),
            9,
            10,
        );
        let mapped = mapped_capture(expected);
        mapped
            .validate_completion(expected, Some(8), 7, 11)
            .expect("exact retained epochs are admitted");

        let stale_surface = expected_surface.with_revision_for_test(2);
        let foreign_surface = surface_in_namespace(6_005, 100, 120, 1.0);
        let different_scale = surface(100, 120, 2.0);
        let different_extent = surface(101, 120, 1.0);
        let different_capabilities =
            expected_surface.with_capabilities_for_test(LinuxPresentationCapabilities::new(
                LinuxAccelerationClass::Software,
                LinuxResetProtection::LoseContextOnReset,
            ));
        let different_navigation = BrowserPageIdentity {
            navigation: BrowserNavigationIdentity::new(11).expect("navigation"),
            ..expected_page
        };
        let different_revision = BrowserPageIdentity {
            revision: BrowserPageSceneRevision::new(2).expect("revision"),
            ..expected_page
        };
        let different_document = BrowserPageIdentity {
            document: Document::new().version(),
            ..expected_page
        };
        let different_pipeline = BrowserPageIdentity {
            pipeline: PipelineKey::new(41, 8),
            ..expected_page
        };
        let mismatches = [
            BrowserFrameRequest::new(
                stale_surface,
                expected.page(),
                expected.chrome_revision(),
                expected.epoch(),
                expected.sequence(),
            ),
            BrowserFrameRequest::new(
                foreign_surface,
                expected.page(),
                expected.chrome_revision(),
                expected.epoch(),
                expected.sequence(),
            ),
            BrowserFrameRequest::new(
                different_scale,
                expected.page(),
                expected.chrome_revision(),
                expected.epoch(),
                expected.sequence(),
            ),
            BrowserFrameRequest::new(
                different_extent,
                expected.page(),
                expected.chrome_revision(),
                expected.epoch(),
                expected.sequence(),
            ),
            BrowserFrameRequest::new(
                different_capabilities,
                expected.page(),
                expected.chrome_revision(),
                expected.epoch(),
                expected.sequence(),
            ),
            BrowserFrameRequest::new(
                expected_surface,
                BrowserPageSnapshot::Scene(different_navigation),
                expected.chrome_revision(),
                expected.epoch(),
                expected.sequence(),
            ),
            BrowserFrameRequest::new(
                expected_surface,
                BrowserPageSnapshot::Scene(different_revision),
                expected.chrome_revision(),
                expected.epoch(),
                expected.sequence(),
            ),
            BrowserFrameRequest::new(
                expected_surface,
                BrowserPageSnapshot::Scene(different_document),
                expected.chrome_revision(),
                expected.epoch(),
                expected.sequence(),
            ),
            BrowserFrameRequest::new(
                expected_surface,
                BrowserPageSnapshot::Scene(different_pipeline),
                expected.chrome_revision(),
                expected.epoch(),
                expected.sequence(),
            ),
            BrowserFrameRequest::new(
                expected_surface,
                expected.page(),
                BrowserChromeRevision::new(5).expect("chrome revision"),
                expected.epoch(),
                expected.sequence(),
            ),
            BrowserFrameRequest::new(
                expected_surface,
                expected.page(),
                expected.chrome_revision(),
                expected.epoch() + 1,
                expected.sequence(),
            ),
            BrowserFrameRequest::new(
                expected_surface,
                expected.page(),
                expected.chrome_revision(),
                expected.epoch(),
                expected.sequence() - 1,
            ),
        ];
        for actual in mismatches {
            let error = mapped
                .validate_completion(actual, Some(8), 7, 11)
                .expect_err("foreign or replayed receipt identity must fail");
            assert_eq!(error.stage(), WebRenderWindowFailureStage::BindCapture);
            assert_eq!(
                error.kind(),
                WebRenderWindowErrorKind::CaptureIdentityMismatch
            );
        }
        for (page_epoch, chrome_epoch, publish_id) in [
            (None, 7, 11),
            (Some(0), 7, 11),
            (Some(10), 7, 11),
            (Some(8), 0, 11),
            (Some(8), 10, 11),
            (Some(8), 7, 0),
        ] {
            let error = mapped
                .validate_completion(expected, page_epoch, chrome_epoch, publish_id)
                .expect_err("invalid epoch/publish evidence must fail");
            assert_eq!(
                error.kind(),
                WebRenderWindowErrorKind::CaptureIdentityMismatch
            );
        }
    }

    #[test]
    fn recorded_capture_size_must_be_positive_and_exact() {
        let surface = surface(100, 120, 1.0);
        let request = BrowserFrameRequest::new(
            surface,
            BrowserPageSnapshot::Blank,
            BrowserChromeRevision::new(1).expect("chrome revision"),
            1,
            1,
        );
        let geometry = BrowserChromeGeometry::for_surface(surface).expect("geometry");
        let prepared = PreparedBrowserCapture::new(request, geometry).expect("capture");
        prepared
            .validate_recorded_size(DeviceIntSize::new(100, 120))
            .expect("exact size");
        for actual in [
            DeviceIntSize::new(0, 120),
            DeviceIntSize::new(-1, 120),
            DeviceIntSize::new(99, 120),
            DeviceIntSize::new(100, 121),
        ] {
            let error = prepared
                .validate_recorded_size(actual)
                .expect_err("malformed recorded size must fail");
            assert_eq!(error.stage(), WebRenderWindowFailureStage::RecordCapture);
            assert_eq!(error.kind(), WebRenderWindowErrorKind::CaptureSizeMismatch);
        }
    }

    #[test]
    fn opaque_identity_types_reserve_zero() {
        assert!(BrowserNavigationIdentity::new(0).is_none());
        assert!(BrowserTabIdentity::new(0).is_none());
        assert!(BrowserPageSceneRevision::new(0).is_none());
        assert!(BrowserChromeRevision::new(0).is_none());
        assert_eq!(BrowserTabIdentity::new(7).expect("identity").get(), 7);
    }

    #[test]
    fn receipt_contains_no_private_resource_document_identity() {
        let surface = surface(800, 600, 1.0);
        let request = BrowserFrameRequest::new(
            surface,
            BrowserPageSnapshot::Blank,
            BrowserChromeRevision::new(1).expect("revision"),
            3,
            4,
        );
        let accounting = BrowserFrameAccounting {
            backend_publish_id: 8,
            rgba8_byte_equivalent: 1_920_000,
            page_display_list_bytes: 0,
            chrome_display_list_bytes: 100,
            root_display_list_bytes: 80,
            chrome_primitives: 9,
        };
        let receipt = super::BrowserFrameReceipt {
            request,
            page_epoch: None,
            chrome_epoch: 3,
            backend_publish_id: accounting.backend_publish_id,
            rgba8_byte_equivalent: accounting.rgba8_byte_equivalent,
            page_display_list_bytes: accounting.page_display_list_bytes,
            chrome_display_list_bytes: accounting.chrome_display_list_bytes,
            root_display_list_bytes: accounting.root_display_list_bytes,
            chrome_primitives: accounting.chrome_primitives,
        };
        assert_eq!(receipt.request().page(), BrowserPageSnapshot::Blank);
        assert_eq!(receipt.root_epoch(), 3);
    }

    #[test]
    fn scene_identity_is_capability_neutral_value_data() {
        let _assert_copy = BrowserPageIdentity {
            navigation: BrowserNavigationIdentity::new(1).expect("navigation"),
            revision: BrowserPageSceneRevision::new(2).expect("scene revision"),
            document: wild_buzzard_dom::Document::new().version(),
            pipeline: wild_buzzard_renderer::PipelineKey::new(3, 4),
        };
    }

    #[test]
    fn equal_page_and_chrome_arcs_keep_distinct_dense_resource_slots() {
        let mut text = TextSystem::new_deterministic(TextLimits::default())
            .expect("deterministic test font initializes");
        let shared = text
            .shape(&TextRequest::new("same Arc", 14.0))
            .expect("test string shapes");
        let source = Document::new().version();
        let page = [ShapedSceneText::new(source, 0, Arc::clone(&shared))];
        let surface = surface(800, 600, 1.0);
        let chrome = BrowserChromeScene::new(
            BrowserChromeRevision::new(1).expect("chrome revision"),
            surface,
            BrowserChromeState::new(Box::new([]), None, Arc::clone(&shared)),
        )
        .expect("chrome scene");
        let resource_document = Document::new();
        let resource = resource_document.version();
        let (staged, partition) =
            stage_browser_texts(resource, Some(&page), Some(&chrome)).expect("combined staging");

        assert_eq!(partition.page_count(), 1);
        assert_eq!(staged.len(), 2);
        assert_eq!(staged[0].document_version(), resource);
        assert_eq!(staged[0].pending_index(), 0);
        assert_eq!(staged[1].document_version(), resource);
        assert_eq!(staged[1].pending_index(), 1);
        assert!(Arc::ptr_eq(staged[0].shaped(), staged[1].shaped()));

        let next_resource = DocumentVersion::new(resource.document_id(), resource.revision() + 1);
        let (restaged, _) = stage_browser_texts(next_resource, Some(&page), Some(&chrome))
            .expect("fresh resource revision stages");
        assert!(
            restaged
                .iter()
                .all(|entry| entry.document_version() == next_resource)
        );
        assert!(
            staged
                .iter()
                .all(|entry| entry.document_version() != next_resource)
        );
    }

    #[test]
    #[allow(clippy::float_cmp)]
    fn root_translates_and_clips_page_below_topmost_full_window_chrome() {
        let surface = surface(800, 600, 1.0);
        let geometry = BrowserChromeGeometry::for_surface(surface).expect("geometry");
        let page = page_identity(1, PipelineKey::new(42, 8));
        let pipelines = BrowserPipelines::new(77);
        let built = build_browser_root_display_list(
            pipelines,
            surface,
            geometry,
            BrowserPageSnapshot::Scene(page),
        )
        .expect("root list");
        let mut iterator = built.display_list.iter();
        let mut iframes = Vec::new();
        while let Some(item) = iterator.next() {
            if let DisplayItem::Iframe(iframe) = *item.item() {
                iframes.push(iframe);
            }
        }
        assert_eq!(iframes.len(), 2);
        let page_iframe = iframes[0];
        assert_eq!(page_iframe.pipeline_id, PipelineId(42, 8));
        assert_eq!(page_iframe.bounds.min.x, 0.0);
        assert_eq!(page_iframe.bounds.min.y, 80.0);
        assert_eq!(page_iframe.bounds.width(), 800.0);
        assert_eq!(page_iframe.bounds.height(), 520.0);
        assert_eq!(page_iframe.clip_rect, page_iframe.bounds);
        let chrome_iframe = iframes[1];
        assert_eq!(chrome_iframe.pipeline_id, pipelines.chrome());
        assert_eq!(chrome_iframe.bounds.min.y, 0.0);
        assert_eq!(chrome_iframe.bounds.height(), 600.0);
    }

    #[test]
    fn blank_root_has_no_page_iframe() {
        let surface = surface(800, 600, 1.0);
        let geometry = BrowserChromeGeometry::for_surface(surface).expect("geometry");
        let pipelines = BrowserPipelines::new(77);
        let built = build_browser_root_display_list(
            pipelines,
            surface,
            geometry,
            BrowserPageSnapshot::Blank,
        )
        .expect("blank root list");
        let mut iterator = built.display_list.iter();
        let mut iframes = Vec::new();
        while let Some(item) = iterator.next() {
            if let DisplayItem::Iframe(iframe) = *item.item() {
                iframes.push(iframe);
            }
        }
        assert_eq!(iframes.len(), 1);
        assert_eq!(iframes[0].pipeline_id, pipelines.chrome());
    }

    #[test]
    fn clear_to_blank_drops_page_hit_authority_and_page_accounting() {
        let mut seeded = seeded_contract();
        let request = BrowserFrameRequest::new(
            seeded.surface,
            BrowserPageSnapshot::Blank,
            seeded.chrome,
            2,
            2,
        );
        let candidate = seeded
            .contract
            .validate_candidate(
                &BrowserPageUpdate::ClearToBlank,
                None,
                request,
                seeded.pipelines,
                seeded.surface,
                Some(1),
                Some(1),
                1,
            )
            .expect("exact clear is admitted");
        assert!(candidate.page_replaced);
        assert_eq!(candidate.page, BrowserPageSnapshot::Blank);
        let hit_map = seeded
            .contract
            .retained_hit_map()
            .expect("retained hit map");
        let receipt = seeded.contract.commit_success(
            candidate,
            request,
            hit_map,
            BrowserFrameAccounting {
                backend_publish_id: 4,
                rgba8_byte_equivalent: 1_920_000,
                page_display_list_bytes: 0,
                chrome_display_list_bytes: 0,
                root_display_list_bytes: 40,
                chrome_primitives: 0,
            },
        );
        assert_eq!(receipt.page_epoch(), None);
        assert_eq!(receipt.page_display_list_bytes(), 0);
        let point = PhysicalPoint {
            x: 20,
            y: i32::try_from(seeded.geometry.content().y() + 20).expect("small y"),
        };
        assert_eq!(
            seeded
                .contract
                .hit_test(point, seeded.surface)
                .expect("blank hit test"),
            None
        );
    }

    #[test]
    fn foreign_surface_and_repeated_epoch_or_sequence_preserve_live_receipt() {
        let seeded = seeded_contract();
        let foreign = surface(801, 600, 1.0);
        let foreign_request = BrowserFrameRequest::new(
            foreign,
            BrowserPageSnapshot::Scene(seeded.page),
            seeded.chrome,
            2,
            2,
        );
        let foreign_error = seeded
            .contract
            .validate_candidate(
                &BrowserPageUpdate::Retain,
                None,
                foreign_request,
                seeded.pipelines,
                seeded.surface,
                Some(1),
                Some(1),
                1,
            )
            .expect_err("foreign retained surface must reject");
        assert_eq!(
            foreign_error.kind(),
            WebRenderWindowErrorKind::StaleSurfaceRevision
        );

        for (epoch, sequence, expected) in [
            (1, 2, WebRenderWindowErrorKind::Epoch),
            (2, 1, WebRenderWindowErrorKind::FrameSequence),
        ] {
            let request = BrowserFrameRequest::new(
                seeded.surface,
                BrowserPageSnapshot::Scene(seeded.page),
                seeded.chrome,
                epoch,
                sequence,
            );
            let error = seeded
                .contract
                .validate_candidate(
                    &BrowserPageUpdate::Retain,
                    None,
                    request,
                    seeded.pipelines,
                    seeded.surface,
                    Some(1),
                    Some(1),
                    1,
                )
                .expect_err("repeated identity must reject");
            assert_eq!(error.kind(), expected);
        }

        let point = PhysicalPoint {
            x: 20,
            y: i32::try_from(seeded.geometry.content().y() + 20).expect("small y"),
        };
        let hit = seeded
            .contract
            .hit_test(point, seeded.surface)
            .expect("preaccept errors preserve hit authority")
            .expect("page remains live");
        assert_eq!(hit.receipt(), seeded.receipt);
        assert!(matches!(
            hit.target(),
            BrowserHitTarget::Page {
                page,
                point: PhysicalPoint { x: 20, y: 20 }
            } if page == seeded.page
        ));
    }

    #[test]
    fn accepted_in_flight_and_legacy_acceptance_invalidate_hit_authority() {
        let mut seeded = seeded_contract();
        seeded.contract.mark_accepted();
        let request = BrowserFrameRequest::new(
            seeded.surface,
            BrowserPageSnapshot::Scene(seeded.page),
            seeded.chrome,
            2,
            2,
        );
        let error = seeded
            .contract
            .validate_candidate(
                &BrowserPageUpdate::Retain,
                None,
                request,
                seeded.pipelines,
                seeded.surface,
                Some(1),
                Some(1),
                1,
            )
            .expect_err("in-flight reentry must reject");
        assert_eq!(error.kind(), WebRenderWindowErrorKind::Contract);
        assert!(
            seeded
                .contract
                .hit_test(PhysicalPoint { x: 0, y: 100 }, seeded.surface)
                .is_err()
        );
        seeded.contract.fail_after_acceptance();
        assert!(
            seeded
                .contract
                .hit_test(PhysicalPoint { x: 0, y: 100 }, seeded.surface)
                .is_err()
        );

        let mut legacy = seeded_contract();
        assert!(
            legacy
                .contract
                .hit_test(
                    PhysicalPoint {
                        x: 10,
                        y: i32::try_from(legacy.geometry.content().y() + 10).expect("small y"),
                    },
                    legacy.surface,
                )
                .expect("live before legacy acceptance")
                .is_some()
        );
        legacy.contract.invalidate_for_legacy_acceptance();
        assert!(
            legacy
                .contract
                .hit_test(PhysicalPoint { x: 10, y: 100 }, legacy.surface)
                .is_err()
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn stale_surface_requires_exact_chrome_and_live_page_replacement() {
        let new_surface = surface(900, 700, 1.0);
        let fresh_chrome = simple_chrome(new_surface, 2);

        let mut retain = seeded_contract();
        retain.contract.mark_surface_stale();
        let retained_request = BrowserFrameRequest::new(
            new_surface,
            BrowserPageSnapshot::Scene(retain.page),
            retain.chrome,
            2,
            2,
        );
        let error = retain
            .contract
            .validate_candidate(
                &BrowserPageUpdate::Retain,
                None,
                retained_request,
                retain.pipelines,
                new_surface,
                Some(1),
                Some(1),
                1,
            )
            .expect_err("old page and chrome cannot relabel a new surface");
        assert_eq!(error.kind(), WebRenderWindowErrorKind::StaleComposition);
        assert_eq!(retain.contract.last_receipt, Some(retain.receipt));

        let chrome_only_request = BrowserFrameRequest::new(
            new_surface,
            BrowserPageSnapshot::Scene(retain.page),
            fresh_chrome.revision(),
            2,
            2,
        );
        let error = retain
            .contract
            .validate_candidate(
                &BrowserPageUpdate::Retain,
                Some(&fresh_chrome),
                chrome_only_request,
                retain.pipelines,
                new_surface,
                Some(1),
                Some(1),
                1,
            )
            .expect_err("fresh chrome alone cannot relabel a retained live page");
        assert_eq!(error.kind(), WebRenderWindowErrorKind::StaleComposition);
        assert_eq!(retain.contract.last_receipt, Some(retain.receipt));

        let mut page_only = seeded_contract();
        page_only.contract.mark_surface_stale();
        let fresh_page = empty_page_scene(new_surface, 2, PipelineKey::new(40, 8));
        let page_only_request = BrowserFrameRequest::new(
            new_surface,
            BrowserPageSnapshot::Scene(fresh_page.identity()),
            page_only.chrome,
            2,
            2,
        );
        let error = page_only
            .contract
            .validate_candidate(
                &BrowserPageUpdate::Install(fresh_page),
                None,
                page_only_request,
                page_only.pipelines,
                new_surface,
                Some(1),
                Some(1),
                1,
            )
            .expect_err("fresh live page alone cannot retain old-surface chrome");
        assert_eq!(error.kind(), WebRenderWindowErrorKind::StaleComposition);
        assert_eq!(page_only.contract.last_receipt, Some(page_only.receipt));

        let mut clear = seeded_contract();
        clear.contract.mark_surface_stale();
        let clear_request = BrowserFrameRequest::new(
            new_surface,
            BrowserPageSnapshot::Blank,
            fresh_chrome.revision(),
            2,
            2,
        );
        let clear_candidate = clear
            .contract
            .validate_candidate(
                &BrowserPageUpdate::ClearToBlank,
                Some(&fresh_chrome),
                clear_request,
                clear.pipelines,
                new_surface,
                Some(1),
                Some(1),
                1,
            )
            .expect("blank plus exact new-surface chrome is sufficient");
        assert_eq!(clear_candidate.page, BrowserPageSnapshot::Blank);
        assert!(clear_candidate.chrome_replaced);

        let mut both = seeded_contract();
        both.contract.mark_surface_stale();
        let fresh_page = empty_page_scene(new_surface, 2, PipelineKey::new(40, 8));
        let both_request = BrowserFrameRequest::new(
            new_surface,
            BrowserPageSnapshot::Scene(fresh_page.identity()),
            fresh_chrome.revision(),
            2,
            2,
        );
        let both_candidate = both
            .contract
            .validate_candidate(
                &BrowserPageUpdate::Install(fresh_page),
                Some(&fresh_chrome),
                both_request,
                both.pipelines,
                new_surface,
                Some(1),
                Some(1),
                1,
            )
            .expect("live page and chrome exact replacements are admitted");
        assert!(both_candidate.page_replaced);
        assert!(both_candidate.chrome_replaced);
    }

    #[test]
    fn chrome_revision_replaces_and_retains_independently_from_page() {
        let seeded = seeded_contract();
        let fresh = simple_chrome(seeded.surface, 2);
        let request = BrowserFrameRequest::new(
            seeded.surface,
            BrowserPageSnapshot::Scene(seeded.page),
            fresh.revision(),
            2,
            2,
        );
        let candidate = seeded
            .contract
            .validate_candidate(
                &BrowserPageUpdate::Retain,
                Some(&fresh),
                request,
                seeded.pipelines,
                seeded.surface,
                Some(1),
                Some(1),
                1,
            )
            .expect("fresh chrome can replace independently");
        assert!(!candidate.page_replaced);
        assert!(candidate.chrome_replaced);
        assert_eq!(candidate.page_epoch, Some(1));
        assert_eq!(candidate.chrome_epoch, 2);

        let repeated = simple_chrome(seeded.surface, 1);
        let repeated_request = BrowserFrameRequest::new(
            seeded.surface,
            BrowserPageSnapshot::Scene(seeded.page),
            repeated.revision(),
            2,
            2,
        );
        let error = seeded
            .contract
            .validate_candidate(
                &BrowserPageUpdate::Retain,
                Some(&repeated),
                repeated_request,
                seeded.pipelines,
                seeded.surface,
                Some(1),
                Some(1),
                1,
            )
            .expect_err("repeated chrome revision rejects");
        assert_eq!(error.kind(), WebRenderWindowErrorKind::RevisionRegressed);
        assert_eq!(seeded.contract.last_receipt, Some(seeded.receipt));

        let retain_request = BrowserFrameRequest::new(
            seeded.surface,
            BrowserPageSnapshot::Scene(seeded.page),
            seeded.chrome,
            2,
            2,
        );
        let retained = seeded
            .contract
            .validate_candidate(
                &BrowserPageUpdate::Retain,
                None,
                retain_request,
                seeded.pipelines,
                seeded.surface,
                Some(1),
                Some(1),
                1,
            )
            .expect("omitted chrome retains exact independent revision");
        assert!(!retained.page_replaced);
        assert!(!retained.chrome_replaced);
        assert_eq!(retained.chrome_epoch, 1);
    }

    #[test]
    fn page_pipeline_collision_and_repeated_page_revision_reject_preaccept() {
        let pipelines = BrowserPipelines::new(91);
        let surface = surface(800, 600, 1.0);
        let collision = empty_page_scene(
            surface,
            1,
            PipelineKey::new(pipelines.root().0, pipelines.root().1),
        );
        let request = BrowserFrameRequest::new(
            surface,
            BrowserPageSnapshot::Scene(collision.identity()),
            BrowserChromeRevision::new(1).expect("chrome revision"),
            1,
            1,
        );
        let error = BrowserCompositorContract::default()
            .validate_candidate(
                &BrowserPageUpdate::Install(collision),
                None,
                request,
                pipelines,
                surface,
                None,
                None,
                0,
            )
            .expect_err("private pipeline collision must reject");
        assert_eq!(error.kind(), WebRenderWindowErrorKind::PipelineMismatch);

        let seeded = seeded_contract();
        let repeated = empty_page_scene(surface, 1, PipelineKey::new(41, 9));
        let repeated_request = BrowserFrameRequest::new(
            surface,
            BrowserPageSnapshot::Scene(repeated.identity()),
            seeded.chrome,
            2,
            2,
        );
        let error = seeded
            .contract
            .validate_candidate(
                &BrowserPageUpdate::Install(repeated),
                None,
                repeated_request,
                seeded.pipelines,
                surface,
                Some(1),
                Some(1),
                1,
            )
            .expect_err("repeated page resource revision must reject");
        assert_eq!(error.kind(), WebRenderWindowErrorKind::RevisionRegressed);
    }

    #[test]
    fn maximum_tab_inventory_is_visible_hittable_and_leaves_title_body() {
        let surface = surface(800, 600, 1.0);
        let mut text = TextSystem::new_deterministic(TextLimits::default())
            .expect("deterministic test font initializes");
        let label = text
            .shape(&TextRequest::new("T", 12.0))
            .expect("tab label shapes");
        let address = text
            .shape(&TextRequest::new("about:tabs", 12.0))
            .expect("address shapes");
        let tabs: Vec<_> = (0..MAX_BROWSER_CHROME_TABS)
            .map(|index| {
                BrowserChromeTab::new(
                    BrowserTabIdentity::new(u64::try_from(index + 1).expect("small tab identity"))
                        .expect("nonzero tab identity"),
                    Arc::clone(&label),
                )
            })
            .collect();
        let active = tabs.first().map(BrowserChromeTab::identity);
        let chrome = BrowserChromeScene::new(
            BrowserChromeRevision::new(1).expect("chrome revision"),
            surface,
            BrowserChromeState::new(tabs.into_boxed_slice(), active, address),
        )
        .expect("maximum tab inventory fits 800 pixels");
        let hit_map = chrome.hit_map();
        assert_eq!(hit_map.tabs.len(), MAX_BROWSER_CHROME_TABS);

        let pipeline = PipelineId(18, 4);
        let mut builder = DisplayListBuilder::new(pipeline);
        builder.begin();
        let space = SpaceAndClipInfo::root_scroll(pipeline);
        let mut primitives = 0;
        for tab in &hit_map.tabs {
            assert!(tab.rect.width() > 0);
            assert!(tab.close.width() > 0);
            assert!(tab.close.width() < tab.rect.width());
            assert!(inset_right(tab.rect, tab.close.width()).width() > 0);
            assert!(tab.rect.contains(PhysicalPoint {
                x: i32::try_from(tab.rect.x()).expect("small x"),
                y: 1,
            }));
            assert!(!tab.close.contains(PhysicalPoint {
                x: i32::try_from(tab.rect.x()).expect("small x"),
                y: 1,
            }));
            push_close_affordance(
                &mut builder,
                space,
                tab.close,
                Some(tab.identity) == active,
                BrowserElementAvailability::Enabled,
                BrowserElementInteraction::Idle,
                &mut primitives,
            )
            .expect("bounded close paint");
        }
        let (_, display_list) = builder.end();
        let mut iterator = display_list.iter();
        let mut painted = 0;
        while let Some(item) = iterator.next() {
            if matches!(item.item(), DisplayItem::Rectangle(_)) {
                painted += 1;
            }
        }
        assert_eq!(painted, primitives);
        assert!(painted >= MAX_BROWSER_CHROME_TABS);
    }

    #[test]
    fn multibyte_variable_width_caret_uses_exact_shaped_cluster_boundary() {
        let mut text = TextSystem::new_deterministic(TextLimits::default())
            .expect("deterministic test font initializes");
        let shaped = text
            .shape(&TextRequest::new("é iW", 18.0).with_word_spacing_px(11.0))
            .expect("multibyte variable-width address shapes");
        let first = shaped
            .runs()
            .iter()
            .flat_map(|run| run.clusters().iter().map(move |cluster| (run, cluster)))
            .find(|(_, cluster)| cluster.text_range() == (0..2))
            .expect("accented character cluster");
        let expected = match first.0.direction() {
            RunDirection::LeftToRight => first.1.x() + first.1.advance(),
            RunDirection::RightToLeft => first.1.x(),
        };
        let caret = shaped_caret_x(&shaped, 2);
        assert!((caret - expected).abs() <= f32::EPSILON);

        #[allow(clippy::cast_precision_loss)]
        let old_byte_ratio = shaped.metrics().full_width() * 2.0 / shaped.text().len() as f32;
        assert!((caret - old_byte_ratio).abs() > 0.5);
        let advances: Vec<_> = shaped
            .runs()
            .iter()
            .flat_map(|run| {
                run.clusters()
                    .iter()
                    .map(wild_buzzard_text::GlyphCluster::advance)
            })
            .collect();
        let min = advances.iter().copied().fold(f32::INFINITY, f32::min);
        let max = advances.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        assert!(max - min > 1.0, "word spacing must make widths variable");
    }

    #[test]
    fn end_caret_remains_inside_one_pixel_address_field() {
        let surface = surface(1, 100, 1.0);
        let chrome = simple_chrome(surface, 1);
        let field = chrome.geometry().address_field();
        assert_eq!(field.width(), 1);
        let caret = address_selection_rect(&chrome).expect("end caret remains drawable");
        assert_eq!(caret.width(), 1);
        assert_eq!(caret.x(), field.x());
        assert!(caret.x() + caret.width() <= field.x() + field.width());
        assert!(caret.y() + caret.height() <= field.y() + field.height());
    }

    #[test]
    fn tiny_tab_close_hit_region_always_has_bounded_visible_paint() {
        let pipeline = PipelineId(18, 3);
        let mut builder = DisplayListBuilder::new(pipeline);
        builder.begin();
        let space = SpaceAndClipInfo::root_scroll(pipeline);
        let hit = BrowserPhysicalRect::new(4, 5, 3, 2);
        let mut primitives = 0;
        push_close_affordance(
            &mut builder,
            space,
            hit,
            false,
            BrowserElementAvailability::Enabled,
            BrowserElementInteraction::Idle,
            &mut primitives,
        )
        .expect("tiny close affordance");
        let (_, display_list) = builder.end();
        let mut iterator = display_list.iter();
        let mut painted = 0;
        while let Some(item) = iterator.next() {
            if let DisplayItem::Rectangle(rectangle) = *item.item() {
                painted += 1;
                assert!(rectangle.bounds.min.x >= 4.0);
                assert!(rectangle.bounds.min.y >= 5.0);
                assert!(rectangle.bounds.max.x <= 7.0);
                assert!(rectangle.bounds.max.y <= 7.0);
            }
        }
        assert_eq!(painted, primitives);
        assert!(painted > 0);
        assert!(hit.contains(PhysicalPoint { x: 5, y: 5 }));
    }

    #[test]
    fn tab_close_and_address_interaction_colors_are_distinct() {
        assert_ne!(
            tab_background_color(false, BrowserElementInteraction::Idle),
            tab_background_color(false, BrowserElementInteraction::Hovered)
        );
        assert_ne!(
            tab_background_color(true, BrowserElementInteraction::Hovered),
            tab_background_color(true, BrowserElementInteraction::Pressed)
        );
        assert_ne!(
            address_background_color(
                BrowserElementAvailability::Enabled,
                BrowserElementInteraction::Idle,
            ),
            address_background_color(
                BrowserElementAvailability::Enabled,
                BrowserElementInteraction::Hovered,
            )
        );
        assert_ne!(
            close_button_colors(
                false,
                BrowserElementAvailability::Enabled,
                BrowserElementInteraction::Hovered,
            ),
            close_button_colors(
                false,
                BrowserElementAvailability::Enabled,
                BrowserElementInteraction::Pressed,
            )
        );
    }

    #[test]
    fn tab_close_and_address_interactions_freeze_exact_typed_state() {
        let surface = surface(800, 600, 1.0);
        let mut text = TextSystem::new_deterministic(TextLimits::default())
            .expect("deterministic test font initializes");
        let title = text
            .shape(&TextRequest::new("Interactive", 14.0))
            .expect("title shapes");
        let tab = BrowserChromeTab::new(BrowserTabIdentity::new(91).expect("tab identity"), title)
            .with_interaction(BrowserElementInteraction::Hovered)
            .with_close_state(
                BrowserElementAvailability::Enabled,
                BrowserElementInteraction::Pressed,
            );
        let preview = BrowserPrimaryLayoutPreview::for_surface(
            surface,
            BrowserChromeDirection::LeftToRight,
            1,
        )
        .expect("primary preview");
        let mut controls = primary_controls(&mut text, &preview);
        let url_index = BrowserPrimaryControlKind::UrlBar.index();
        controls[url_index] = controls[url_index]
            .clone()
            .with_interaction(BrowserElementInteraction::Hovered);
        let primary = BrowserPrimaryChromeState::new(
            BrowserChromeDirection::LeftToRight,
            controls,
            BrowserReloadStopMode::Reload,
            BrowserSiteIdentityKind::LoopbackHttp,
        );
        let address = text
            .shape(&TextRequest::new("about:interaction", 14.0))
            .expect("address shapes");
        let scene = BrowserChromeScene::new(
            BrowserChromeRevision::new(1).expect("revision"),
            surface,
            BrowserChromeState::new(
                vec![tab].into_boxed_slice(),
                BrowserTabIdentity::new(91),
                address,
            )
            .with_primary_chrome(Some(primary)),
        )
        .expect("typed interactions freeze");
        assert_eq!(
            scene.state.tabs[0].interaction(),
            BrowserElementInteraction::Hovered
        );
        assert_eq!(
            scene.state.tabs[0].close_interaction(),
            BrowserElementInteraction::Pressed
        );
        assert_eq!(
            scene
                .primary_layout()
                .expect("primary layout")
                .control(BrowserPrimaryControlKind::UrlBar)
                .interaction(),
            BrowserElementInteraction::Hovered
        );
    }

    #[test]
    fn disabled_tab_close_and_address_interactions_fail_closed() {
        let surface = surface(800, 600, 1.0);
        let mut text = TextSystem::new_deterministic(TextLimits::default())
            .expect("deterministic test font initializes");
        let invalid_title = text
            .shape(&TextRequest::new("Invalid", 14.0))
            .expect("invalid title shapes");
        let invalid_tab = BrowserChromeTab::new(
            BrowserTabIdentity::new(92).expect("tab identity"),
            invalid_title,
        )
        .with_close_state(
            BrowserElementAvailability::Disabled,
            BrowserElementInteraction::Hovered,
        );
        let invalid_address = text
            .shape(&TextRequest::new("about:invalid", 14.0))
            .expect("invalid address shapes");
        let error = BrowserChromeScene::new(
            BrowserChromeRevision::new(2).expect("revision"),
            surface,
            BrowserChromeState::new(
                vec![invalid_tab].into_boxed_slice(),
                BrowserTabIdentity::new(92),
                invalid_address,
            ),
        )
        .expect_err("disabled close cannot retain pointer interaction");
        assert_eq!(error.kind(), WebRenderWindowErrorKind::Contract);

        let preview = BrowserPrimaryLayoutPreview::for_surface(
            surface,
            BrowserChromeDirection::LeftToRight,
            0,
        )
        .expect("primary preview");
        let mut invalid_controls = primary_controls(&mut text, &preview);
        let url_index = BrowserPrimaryControlKind::UrlBar.index();
        let url = &invalid_controls[url_index];
        invalid_controls[url_index] = BrowserPrimaryControl::new(
            url.element(),
            url.kind(),
            Arc::clone(url.label()),
            BrowserElementAvailability::Disabled,
        )
        .with_interaction(BrowserElementInteraction::Pressed);
        let invalid_primary = BrowserPrimaryChromeState::new(
            BrowserChromeDirection::LeftToRight,
            invalid_controls,
            BrowserReloadStopMode::Reload,
            BrowserSiteIdentityKind::LoopbackHttp,
        );
        let invalid_address = text
            .shape(&TextRequest::new("about:disabled-address", 14.0))
            .expect("disabled address shapes");
        let error = BrowserChromeScene::new(
            BrowserChromeRevision::new(3).expect("revision"),
            surface,
            BrowserChromeState::new(Box::new([]), None, invalid_address)
                .with_primary_chrome(Some(invalid_primary)),
        )
        .expect_err("disabled address cannot retain pointer interaction");
        assert_eq!(error.kind(), WebRenderWindowErrorKind::Contract);
    }

    #[test]
    #[allow(clippy::float_cmp)]
    fn reload_artwork_mirrors_every_asymmetric_mark_in_rtl() {
        let rect = BrowserPhysicalRect::new(10, 20, 18, 18);
        let build = |direction| {
            let pipeline = PipelineId(22, 1);
            let mut builder = DisplayListBuilder::new(pipeline);
            builder.begin();
            let mut primitives = 0;
            push_reload_icon(
                &mut builder,
                SpaceAndClipInfo::root_scroll(pipeline),
                rect,
                direction,
                ColorF::new(1.0, 1.0, 1.0, 1.0),
                &mut primitives,
            )
            .expect("reload artwork");
            let (_, display_list) = builder.end();
            let mut items = Vec::new();
            let mut iterator = display_list.iter();
            while let Some(item) = iterator.next() {
                if let DisplayItem::Rectangle(item) = *item.item() {
                    items.push(item.bounds);
                }
            }
            assert_eq!(items.len(), primitives);
            items
        };
        let ltr = build(BrowserChromeDirection::LeftToRight);
        let rtl = build(BrowserChromeDirection::RightToLeft);
        assert_eq!(ltr.len(), rtl.len());
        for (left, right) in ltr.iter().zip(&rtl) {
            assert_eq!(left.min.y, right.min.y);
            assert_eq!(left.height(), right.height());
            assert_eq!(left.width(), right.width());
            assert_eq!(
                right.min.x,
                10.0 + 18.0 - (left.min.x - 10.0) - left.width()
            );
        }
        assert_ne!(ltr, rtl, "reload mark is intentionally asymmetric");
    }

    #[test]
    fn receipt_bound_primary_hits_preserve_address_and_exact_control_identity() {
        let surface = surface(800, 600, 1.0);
        let mut text = TextSystem::new_deterministic(TextLimits::default())
            .expect("deterministic font initializes");
        let title = text
            .shape(&TextRequest::new("Primary", 14.0))
            .expect("title shapes");
        let address = text
            .shape(&TextRequest::new("about:primary", 14.0))
            .expect("address shapes");
        let tab = BrowserChromeTab::new(BrowserTabIdentity::new(1).expect("tab identity"), title);
        let preview = BrowserPrimaryLayoutPreview::for_surface(
            surface,
            BrowserChromeDirection::LeftToRight,
            1,
        )
        .expect("preview");
        let controls = primary_controls(&mut text, &preview);
        let back = controls[BrowserPrimaryControlKind::Back.index()].element();
        let primary = BrowserPrimaryChromeState::new(
            BrowserChromeDirection::LeftToRight,
            controls,
            BrowserReloadStopMode::Reload,
            BrowserSiteIdentityKind::LoopbackHttp,
        );
        let chrome = BrowserChromeScene::new(
            BrowserChromeRevision::new(1).expect("revision"),
            surface,
            BrowserChromeState::new(
                vec![tab].into_boxed_slice(),
                BrowserTabIdentity::new(1),
                address,
            )
            .with_primary_chrome(Some(primary)),
        )
        .expect("primary scene");
        let hit_map = chrome.hit_map();
        let request =
            BrowserFrameRequest::new(surface, BrowserPageSnapshot::Blank, chrome.revision(), 1, 1);
        let mut contract = BrowserCompositorContract::default();
        let receipt = contract.commit_success(
            BrowserCandidate {
                page: BrowserPageSnapshot::Blank,
                previous_page: BrowserPageSnapshot::Blank,
                page_epoch: None,
                chrome_revision: chrome.revision(),
                chrome_epoch: 1,
                page_replaced: false,
                chrome_replaced: true,
            },
            request,
            hit_map,
            BrowserFrameAccounting {
                backend_publish_id: 8,
                rgba8_byte_equivalent: 1_920_000,
                page_display_list_bytes: 0,
                chrome_display_list_bytes: 100,
                root_display_list_bytes: 80,
                chrome_primitives: 30,
            },
        );
        let back_rect = preview
            .control(BrowserPrimaryControlKind::Back)
            .rect()
            .expect("back visible");
        let back_hit = contract
            .hit_test(
                PhysicalPoint {
                    x: i32::try_from(back_rect.x() + 1).expect("small x"),
                    y: i32::try_from(back_rect.y() + 1).expect("small y"),
                },
                surface,
            )
            .expect("back hit")
            .expect("back target");
        assert_eq!(back_hit.receipt(), receipt);
        assert_eq!(
            back_hit.target(),
            BrowserHitTarget::PrimaryControl {
                element: back,
                kind: BrowserPrimaryControlKind::Back,
            }
        );
        let address_rect = preview.address_field();
        let address_hit = contract
            .hit_test(
                PhysicalPoint {
                    x: i32::try_from(address_rect.x() + 1).expect("small x"),
                    y: i32::try_from(address_rect.y() + 1).expect("small y"),
                },
                surface,
            )
            .expect("address hit")
            .expect("address target");
        assert_eq!(address_hit.receipt(), receipt);
        assert_eq!(address_hit.target(), BrowserHitTarget::AddressBar);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn popup_hit_z_order_is_row_then_surface_then_dismiss_shield() {
        let surface = surface(360, 600, 1.0);
        let mut text = TextSystem::new_deterministic(TextLimits::default())
            .expect("deterministic font initializes");
        let title = text
            .shape(&TextRequest::new("Overflow", 14.0))
            .expect("title shapes");
        let address = text
            .shape(&TextRequest::new("about:overflow", 14.0))
            .expect("address shapes");
        let tab = BrowserChromeTab::new(BrowserTabIdentity::new(1).expect("tab identity"), title);
        let preview = BrowserPrimaryLayoutPreview::for_surface(
            surface,
            BrowserChromeDirection::LeftToRight,
            1,
        )
        .expect("preview");
        let controls = primary_controls(&mut text, &preview);
        let overflow = controls[BrowserPrimaryControlKind::Overflow.index()].element();
        let new_tab = controls[BrowserPrimaryControlKind::NewTab.index()].element();
        let popup = BrowserPrimaryPopup::new(
            BrowserPrimaryPopupKind::Overflow,
            overflow,
            vec![BrowserPrimaryPopupRow::relocated_control(
                new_tab,
                BrowserPrimaryControlKind::NewTab,
                BrowserElementAvailability::Enabled,
            )]
            .into_boxed_slice(),
        );
        let primary = BrowserPrimaryChromeState::new(
            BrowserChromeDirection::LeftToRight,
            controls,
            BrowserReloadStopMode::Reload,
            BrowserSiteIdentityKind::LoopbackHttp,
        )
        .with_popup(Some(popup));
        let chrome = BrowserChromeScene::new(
            BrowserChromeRevision::new(1).expect("revision"),
            surface,
            BrowserChromeState::new(
                vec![tab].into_boxed_slice(),
                BrowserTabIdentity::new(1),
                address,
            )
            .with_primary_chrome(Some(primary)),
        )
        .expect("popup scene");
        let layout = chrome.primary_layout().expect("primary");
        let popup = layout.popup().expect("popup");
        let panel = popup.rect();
        let row = popup.rows()[0].rect().expect("visible row");
        let request =
            BrowserFrameRequest::new(surface, BrowserPageSnapshot::Blank, chrome.revision(), 1, 1);
        let mut contract = BrowserCompositorContract::default();
        let receipt = contract.commit_success(
            BrowserCandidate {
                page: BrowserPageSnapshot::Blank,
                previous_page: BrowserPageSnapshot::Blank,
                page_epoch: None,
                chrome_revision: chrome.revision(),
                chrome_epoch: 1,
                page_replaced: false,
                chrome_replaced: true,
            },
            request,
            chrome.hit_map(),
            BrowserFrameAccounting {
                backend_publish_id: 9,
                rgba8_byte_equivalent: 864_000,
                page_display_list_bytes: 0,
                chrome_display_list_bytes: 120,
                root_display_list_bytes: 80,
                chrome_primitives: 40,
            },
        );
        let hit = |point| {
            contract
                .hit_test(point, surface)
                .expect("hit test")
                .expect("target")
        };
        let row_hit = hit(PhysicalPoint {
            x: i32::try_from(row.x() + 1).expect("small x"),
            y: i32::try_from(row.y() + 1).expect("small y"),
        });
        assert_eq!(row_hit.receipt(), receipt);
        assert_eq!(
            row_hit.target(),
            BrowserHitTarget::PrimaryPopupRow {
                element: new_tab,
                kind: BrowserPrimaryPopupRowKind::Control(BrowserPrimaryControlKind::NewTab),
            }
        );
        let surface_hit = hit(PhysicalPoint {
            x: i32::try_from(panel.x() + 1).expect("small x"),
            y: i32::try_from(panel.y() + 1).expect("small y"),
        });
        assert_eq!(
            surface_hit.target(),
            BrowserHitTarget::PrimaryPopupSurface {
                kind: BrowserPrimaryPopupKind::Overflow,
                anchor: overflow,
            }
        );
        let dismiss_hit = hit(PhysicalPoint { x: 1, y: 1 });
        assert_eq!(
            dismiss_hit.target(),
            BrowserHitTarget::PrimaryPopupDismiss {
                kind: BrowserPrimaryPopupKind::Overflow,
                anchor: overflow,
            }
        );
        assert_eq!(
            contract
                .hit_test(PhysicalPoint { x: -1, y: -1 }, surface)
                .expect("outside hit"),
            None
        );
    }
}
