#![forbid(unsafe_code)]

use std::num::NonZeroU64;
use std::sync::Arc;

use wild_buzzard_text::ShapedText;

use crate::browser_compositor::{
    BrowserChromeFocus, BrowserChromeGeometry, BrowserChromeTab, BrowserPhysicalRect,
    BrowserTabIdentity, MAX_BROWSER_CHROME_TABS, browser_contract_error, browser_resource_error,
    scaled_axis, validate_shaped_chrome_text,
};
use crate::{WebRenderSurfaceSnapshot, WebRenderWindowError, WebRenderWindowErrorKind};

/// Exact fixed primary-control inventory authored by the browser shell.
pub const MAX_BROWSER_PRIMARY_CONTROLS: usize = 9;
/// Maximum rows retained by one open primary popup.
pub const MAX_BROWSER_PRIMARY_POPUP_ROWS: usize = MAX_BROWSER_CHROME_TABS;

const PRIMARY_BUTTON_CSS_PX: f64 = 32.0;
const PRIMARY_GAP_CSS_PX: f64 = 4.0;
const SITE_IDENTITY_CSS_PX: f64 = 28.0;
const MIN_URL_BAR_CSS_PX: f64 = 64.0;
const NEW_TAB_NARROW_CSS_PX: f64 = 420.0;
const MIN_TAB_WITH_NEW_TAB_CSS_PX: f64 = 72.0;
const POPUP_WIDTH_CSS_PX: f64 = 300.0;
const POPUP_MAX_HEIGHT_CSS_PX: f64 = 420.0;
const POPUP_MIN_WIDTH_CSS_PX: f64 = 160.0;
const POPUP_ROW_HEIGHT_CSS_PX: f64 = 36.0;
const POPUP_MARGIN_CSS_PX: f64 = 8.0;
const POPUP_PADDING_CSS_PX: f64 = 8.0;

/// Browser-owned opaque identity for one primary control or popup row.
///
/// Graphics uses this only as immutable value data. The browser session must
/// revalidate the successful chrome revision, identity, semantic kind, and
/// availability before dispatching an effect.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct BrowserChromeElementIdentity(NonZeroU64);

impl BrowserChromeElementIdentity {
    /// Creates an opaque identity. Zero is reserved.
    #[must_use]
    pub const fn new(value: u64) -> Option<Self> {
        match NonZeroU64::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    /// Numeric value for checked transport and browser-side revalidation.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// Logical direction for primary chrome layout and directional artwork.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BrowserChromeDirection {
    /// Logical start is the physical left edge.
    LeftToRight,
    /// Logical start is the physical right edge.
    RightToLeft,
}

/// Canonical action availability supplied by the browser session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BrowserElementAvailability {
    /// No action is currently dispatchable.
    Disabled,
    /// The exact revision/identity/kind has a live browser action mapping.
    Enabled,
}

/// Mutually exclusive pointer interaction for one exact element.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BrowserElementInteraction {
    /// Neither hovered nor pressed.
    Idle,
    /// Hovered but not pressed.
    Hovered,
    /// Pressed; this is also the authoritative hover/press visual state.
    Pressed,
}

/// Canonical selection state for a popup row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BrowserElementSelection {
    /// The row is not selected.
    NotSelected,
    /// The row is the exact selected member of its inventory.
    Selected,
}

/// Canonical expansion state for a popup row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BrowserElementExpansion {
    /// The row has no child view.
    Leaf,
    /// A child view exists and is closed.
    Collapsed,
    /// A child view exists and is open.
    Expanded,
}

/// Stable semantic kind for the fixed primary-control inventory.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum BrowserPrimaryControlKind {
    /// Back through session history.
    Back,
    /// Forward through session history.
    Forward,
    /// One combined reload-or-stop surface.
    ReloadStop,
    /// Site identity surface embedded at the logical start of the URL field.
    SiteIdentity,
    /// URL editor visual state. Pointer hits remain `BrowserHitTarget::AddressBar`.
    UrlBar,
    /// Create a new tab.
    NewTab,
    /// Open the exact live tab inventory.
    AllTabs,
    /// Open the browser application menu.
    ApplicationMenu,
    /// Open controls deterministically relocated from the toolbar.
    Overflow,
}

impl BrowserPrimaryControlKind {
    /// Canonical order required by [`BrowserPrimaryChromeState`].
    pub const ALL: [Self; MAX_BROWSER_PRIMARY_CONTROLS] = [
        Self::Back,
        Self::Forward,
        Self::ReloadStop,
        Self::SiteIdentity,
        Self::UrlBar,
        Self::NewTab,
        Self::AllTabs,
        Self::ApplicationMenu,
        Self::Overflow,
    ];

    pub(crate) const fn index(self) -> usize {
        match self {
            Self::Back => 0,
            Self::Forward => 1,
            Self::ReloadStop => 2,
            Self::SiteIdentity => 3,
            Self::UrlBar => 4,
            Self::NewTab => 5,
            Self::AllTabs => 6,
            Self::ApplicationMenu => 7,
            Self::Overflow => 8,
        }
    }
}

/// Sole visual/action mode for the combined reload-or-stop control.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BrowserReloadStopMode {
    /// No pending page load; activate to reload.
    Reload,
    /// A page load is pending; activate to stop.
    Stop,
}

/// Browser-owned site identity classification rendered without branded art.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BrowserSiteIdentityKind {
    /// No accepted navigation identity is available.
    Empty,
    /// A browser-internal page.
    Internal,
    /// Loopback HTTP, distinguished from a public insecure origin.
    LoopbackHttp,
    /// A transport-authenticated secure origin.
    Secure,
    /// An unauthenticated public origin.
    Insecure,
    /// A secure top-level origin with mixed active or passive content.
    Mixed,
}

/// One immutable fixed primary-control projection.
#[derive(Clone, Debug)]
pub struct BrowserPrimaryControl {
    element: BrowserChromeElementIdentity,
    kind: BrowserPrimaryControlKind,
    label: Arc<ShapedText>,
    availability: BrowserElementAvailability,
    interaction: BrowserElementInteraction,
}

impl BrowserPrimaryControl {
    /// Creates one control with an idle pointer state.
    #[must_use]
    pub fn new(
        element: BrowserChromeElementIdentity,
        kind: BrowserPrimaryControlKind,
        label: Arc<ShapedText>,
        availability: BrowserElementAvailability,
    ) -> Self {
        Self {
            element,
            kind,
            label,
            availability,
            interaction: BrowserElementInteraction::Idle,
        }
    }

    /// Sets the sole pointer interaction state.
    #[must_use]
    pub const fn with_interaction(mut self, interaction: BrowserElementInteraction) -> Self {
        self.interaction = interaction;
        self
    }

    /// Opaque stable element identity.
    #[must_use]
    pub const fn element(&self) -> BrowserChromeElementIdentity {
        self.element
    }

    /// Stable semantic kind.
    #[must_use]
    pub const fn kind(&self) -> BrowserPrimaryControlKind {
        self.kind
    }

    /// Exact shaped localized name used if this control relocates to overflow.
    #[must_use]
    pub const fn label(&self) -> &Arc<ShapedText> {
        &self.label
    }

    /// Canonical browser action availability.
    #[must_use]
    pub const fn availability(&self) -> BrowserElementAvailability {
        self.availability
    }

    /// Canonical pointer interaction.
    #[must_use]
    pub const fn interaction(&self) -> BrowserElementInteraction {
        self.interaction
    }
}

/// Popup kind and the fixed control from which it is anchored.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BrowserPrimaryPopupKind {
    /// Site information surface anchored to `SiteIdentity`.
    SiteIdentity,
    /// Exact tab inventory anchored to `AllTabs`.
    AllTabs,
    /// Application actions anchored to `ApplicationMenu`.
    ApplicationMenu,
    /// Deterministically relocated controls anchored to Overflow.
    Overflow,
}

impl BrowserPrimaryPopupKind {
    pub(crate) const fn anchor_kind(self) -> BrowserPrimaryControlKind {
        match self {
            Self::SiteIdentity => BrowserPrimaryControlKind::SiteIdentity,
            Self::AllTabs => BrowserPrimaryControlKind::AllTabs,
            Self::ApplicationMenu => BrowserPrimaryControlKind::ApplicationMenu,
            Self::Overflow => BrowserPrimaryControlKind::Overflow,
        }
    }
}

/// Browser actions admitted in the first primary popup surface.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum BrowserPrimaryActionKind {
    /// Create a new tab.
    NewTab,
    /// Close the exact active tab.
    CloseTab,
    /// Navigate backward.
    Back,
    /// Navigate forward.
    Forward,
    /// Invoke the currently selected reload-or-stop action.
    ReloadStop,
    /// Informational site summary; W7 renders it disabled.
    SiteInformation,
    /// Informational permission summary; W7 renders it disabled.
    SitePermissions,
}

/// Semantic kind returned for one popup row hit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BrowserPrimaryPopupRowKind {
    /// One exact tab from the live tab inventory.
    Tab(BrowserTabIdentity),
    /// One fixed control relocated without changing its element identity.
    Control(BrowserPrimaryControlKind),
    /// One browser action supplied by the session projection.
    Action(BrowserPrimaryActionKind),
}

/// One immutable popup-row projection.
#[derive(Clone, Debug)]
pub struct BrowserPrimaryPopupRow {
    element: BrowserChromeElementIdentity,
    kind: BrowserPrimaryPopupRowKind,
    action_label: Option<Arc<ShapedText>>,
    availability: BrowserElementAvailability,
    interaction: BrowserElementInteraction,
    selection: BrowserElementSelection,
    expansion: BrowserElementExpansion,
}

impl BrowserPrimaryPopupRow {
    /// Creates an all-tabs row. Its exact title is derived from the named tab.
    #[must_use]
    pub const fn tab(
        element: BrowserChromeElementIdentity,
        tab: BrowserTabIdentity,
        availability: BrowserElementAvailability,
    ) -> Self {
        Self::without_label(element, BrowserPrimaryPopupRowKind::Tab(tab), availability)
    }

    /// Creates an overflow row. Its label and interaction must exactly match
    /// the named fixed control and are validated when the scene is frozen.
    #[must_use]
    pub const fn relocated_control(
        element: BrowserChromeElementIdentity,
        control: BrowserPrimaryControlKind,
        availability: BrowserElementAvailability,
    ) -> Self {
        Self::without_label(
            element,
            BrowserPrimaryPopupRowKind::Control(control),
            availability,
        )
    }

    /// Creates one application or informational action row.
    #[must_use]
    pub fn action(
        element: BrowserChromeElementIdentity,
        action: BrowserPrimaryActionKind,
        label: Arc<ShapedText>,
        availability: BrowserElementAvailability,
    ) -> Self {
        Self {
            element,
            kind: BrowserPrimaryPopupRowKind::Action(action),
            action_label: Some(label),
            availability,
            interaction: BrowserElementInteraction::Idle,
            selection: BrowserElementSelection::NotSelected,
            expansion: BrowserElementExpansion::Leaf,
        }
    }

    const fn without_label(
        element: BrowserChromeElementIdentity,
        kind: BrowserPrimaryPopupRowKind,
        availability: BrowserElementAvailability,
    ) -> Self {
        Self {
            element,
            kind,
            action_label: None,
            availability,
            interaction: BrowserElementInteraction::Idle,
            selection: BrowserElementSelection::NotSelected,
            expansion: BrowserElementExpansion::Leaf,
        }
    }

    /// Sets the sole pointer interaction state.
    #[must_use]
    pub const fn with_interaction(mut self, interaction: BrowserElementInteraction) -> Self {
        self.interaction = interaction;
        self
    }

    /// Sets the canonical selection state.
    #[must_use]
    pub const fn with_selection(mut self, selection: BrowserElementSelection) -> Self {
        self.selection = selection;
        self
    }

    /// Sets the canonical expansion state.
    #[must_use]
    pub const fn with_expansion(mut self, expansion: BrowserElementExpansion) -> Self {
        self.expansion = expansion;
        self
    }

    /// Opaque stable row identity.
    #[must_use]
    pub const fn element(&self) -> BrowserChromeElementIdentity {
        self.element
    }

    /// Stable row semantic kind.
    #[must_use]
    pub const fn kind(&self) -> BrowserPrimaryPopupRowKind {
        self.kind
    }

    /// Canonical browser action availability.
    #[must_use]
    pub const fn availability(&self) -> BrowserElementAvailability {
        self.availability
    }

    /// Canonical pointer interaction.
    #[must_use]
    pub const fn interaction(&self) -> BrowserElementInteraction {
        self.interaction
    }

    /// Canonical selection state.
    #[must_use]
    pub const fn selection(&self) -> BrowserElementSelection {
        self.selection
    }

    /// Canonical expansion state.
    #[must_use]
    pub const fn expansion(&self) -> BrowserElementExpansion {
        self.expansion
    }
}

/// Sole open popup supplied by the browser session.
#[derive(Clone, Debug)]
pub struct BrowserPrimaryPopup {
    kind: BrowserPrimaryPopupKind,
    anchor: BrowserChromeElementIdentity,
    rows: Box<[BrowserPrimaryPopupRow]>,
    first_visible_row: usize,
}

impl BrowserPrimaryPopup {
    /// Creates an open popup at the first row.
    #[must_use]
    pub fn new(
        kind: BrowserPrimaryPopupKind,
        anchor: BrowserChromeElementIdentity,
        rows: Box<[BrowserPrimaryPopupRow]>,
    ) -> Self {
        Self {
            kind,
            anchor,
            rows,
            first_visible_row: 0,
        }
    }

    /// Selects the first row in the visible scroll window.
    #[must_use]
    pub const fn with_first_visible_row(mut self, first: usize) -> Self {
        self.first_visible_row = first;
        self
    }

    /// Popup semantic kind.
    #[must_use]
    pub const fn kind(&self) -> BrowserPrimaryPopupKind {
        self.kind
    }

    /// Exact anchor element.
    #[must_use]
    pub const fn anchor(&self) -> BrowserChromeElementIdentity {
        self.anchor
    }

    /// Complete bounded popup-row inventory.
    #[must_use]
    pub const fn rows(&self) -> &[BrowserPrimaryPopupRow] {
        &self.rows
    }

    /// First row in the visible scroll window.
    #[must_use]
    pub const fn first_visible_row(&self) -> usize {
        self.first_visible_row
    }
}

/// Browser session projection used to freeze one primary chrome scene.
#[derive(Clone, Debug)]
pub struct BrowserPrimaryChromeState {
    direction: BrowserChromeDirection,
    controls: Box<[BrowserPrimaryControl]>,
    reload_stop_mode: BrowserReloadStopMode,
    site_identity: BrowserSiteIdentityKind,
    popup: Option<BrowserPrimaryPopup>,
}

impl BrowserPrimaryChromeState {
    /// Creates a closed primary surface. Controls must be in
    /// [`BrowserPrimaryControlKind::ALL`] order and are checked on freeze.
    #[must_use]
    pub fn new(
        direction: BrowserChromeDirection,
        controls: Box<[BrowserPrimaryControl]>,
        reload_stop_mode: BrowserReloadStopMode,
        site_identity: BrowserSiteIdentityKind,
    ) -> Self {
        Self {
            direction,
            controls,
            reload_stop_mode,
            site_identity,
            popup: None,
        }
    }

    /// Supplies the sole open popup.
    #[must_use]
    pub fn with_popup(mut self, popup: Option<BrowserPrimaryPopup>) -> Self {
        self.popup = popup;
        self
    }

    /// Logical layout direction.
    #[must_use]
    pub const fn direction(&self) -> BrowserChromeDirection {
        self.direction
    }

    /// Exact fixed control projection.
    #[must_use]
    pub const fn controls(&self) -> &[BrowserPrimaryControl] {
        &self.controls
    }

    /// Sole reload-or-stop mode.
    #[must_use]
    pub const fn reload_stop_mode(&self) -> BrowserReloadStopMode {
        self.reload_stop_mode
    }

    /// Site identity classification.
    #[must_use]
    pub const fn site_identity(&self) -> BrowserSiteIdentityKind {
        self.site_identity
    }

    /// Sole open popup, if any.
    #[must_use]
    pub const fn popup(&self) -> Option<&BrowserPrimaryPopup> {
        self.popup.as_ref()
    }
}

/// Deterministic placement of one fixed control.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BrowserPrimaryControlPlacement {
    /// Semantically presented in the tab or navigation toolbar. Its exact
    /// rectangle may be zero-area while an ordinary tiny resize is collapsed.
    Toolbar,
    /// Semantically embedded in the combined site-identity/URL field. Its
    /// exact rectangle may be zero-area while that row is collapsed.
    AddressField,
    /// Relocated into the overflow popup.
    OverflowPanel,
    /// Not presented because its derived inventory is empty.
    Hidden,
}

/// Read-only kind-level geometry available before a session projection exists.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BrowserPrimaryPreviewControl {
    kind: BrowserPrimaryControlKind,
    placement: BrowserPrimaryControlPlacement,
    rect: Option<BrowserPhysicalRect>,
}

impl BrowserPrimaryPreviewControl {
    /// Semantic control kind.
    #[must_use]
    pub const fn kind(self) -> BrowserPrimaryControlKind {
        self.kind
    }

    /// Deterministically resolved placement.
    #[must_use]
    pub const fn placement(self) -> BrowserPrimaryControlPlacement {
        self.placement
    }

    /// Physical hit/paint rectangle when presented in primary chrome. A
    /// zero-area value retains semantic membership without paint or hit
    /// authority on a tiny surface.
    #[must_use]
    pub const fn rect(self) -> Option<BrowserPhysicalRect> {
        self.rect
    }
}

/// Kind-level primary layout used to predict overflow without circular state.
#[derive(Clone, Debug, PartialEq)]
pub struct BrowserPrimaryLayoutPreview {
    surface: WebRenderSurfaceSnapshot,
    direction: BrowserChromeDirection,
    controls: [BrowserPrimaryPreviewControl; MAX_BROWSER_PRIMARY_CONTROLS],
    hidden_controls: Box<[BrowserPrimaryControlKind]>,
    url_container: BrowserPhysicalRect,
    address_field: BrowserPhysicalRect,
    tabs: Box<[BrowserPhysicalRect]>,
    tab_titles: Box<[BrowserPhysicalRect]>,
    tab_closes: Box<[BrowserPhysicalRect]>,
    popup_row_capacity: usize,
}

impl BrowserPrimaryLayoutPreview {
    /// Resolves scale, direction, tab capacity, toolbar rectangles, and the
    /// exact overflow inventory before the browser constructs a popup.
    ///
    /// # Errors
    ///
    /// Rejects a suspended or over-capacity surface. Every supported nonzero
    /// drawable extent has a bounded result; rows that cannot fit collapse to
    /// zero-area rectangles instead of turning an ordinary resize terminal.
    pub fn for_surface(
        surface: WebRenderSurfaceSnapshot,
        direction: BrowserChromeDirection,
        tab_count: usize,
    ) -> Result<Self, WebRenderWindowError> {
        let geometry = BrowserChromeGeometry::for_surface(surface)?;
        Self::from_geometry(geometry, direction, tab_count)
    }

    #[allow(clippy::too_many_lines)]
    pub(crate) fn from_geometry(
        geometry: BrowserChromeGeometry,
        direction: BrowserChromeDirection,
        tab_count: usize,
    ) -> Result<Self, WebRenderWindowError> {
        if tab_count > MAX_BROWSER_CHROME_TABS {
            return Err(browser_resource_error(
                "primary layout tab count exceeds its fixed limit",
            ));
        }
        let surface = geometry.surface();
        let scale = surface.descriptor().scale.get();
        let tab_strip = geometry.tab_strip();
        let address_strip = geometry.address_field();
        let button = scaled_axis(PRIMARY_BUTTON_CSS_PX, scale)?;
        let gap = scaled_axis(PRIMARY_GAP_CSS_PX, scale)?;
        let min_tab = scaled_axis(MIN_TAB_WITH_NEW_TAB_CSS_PX, scale)?;
        let narrow = scaled_axis(NEW_TAB_NARROW_CSS_PX, scale)?;
        let tab_button = button.min(tab_strip.height());
        let tab_y = tab_strip
            .y()
            .saturating_add(tab_strip.height().saturating_sub(tab_button) / 2);

        let fixed_all_tabs = tab_button.saturating_add(gap);
        let with_new_reserve = fixed_all_tabs.saturating_add(tab_button.saturating_add(gap));
        let width_with_new = tab_strip.width().saturating_sub(with_new_reserve);
        let tab_count_u32 = u32::try_from(tab_count).map_err(|_| {
            browser_resource_error("primary layout tab count cannot be represented")
        })?;
        let average_accepts_new =
            tab_count_u32 == 0 || width_with_new / tab_count_u32.max(1) >= min_tab;
        let show_new_tab = tab_strip.width() >= narrow
            && average_accepts_new
            && tab_strip.width() >= with_new_reserve.saturating_add(tab_count_u32);
        let reserved = if show_new_tab {
            with_new_reserve
        } else {
            fixed_all_tabs
        };
        let tab_row_drawable = tab_strip.width() >= fixed_all_tabs;
        let tab_area_width = if tab_row_drawable {
            tab_strip.width().saturating_sub(reserved)
        } else {
            0
        };
        let tab_area = logical_rect(tab_strip, 0, tab_area_width, tab_strip.height(), direction);
        let new_tab_rect = show_new_tab.then(|| {
            logical_rect(
                tab_strip,
                tab_area_width.saturating_add(gap),
                tab_button,
                tab_button,
                direction,
            )
            .with_y(tab_y)
        });
        let all_tabs_rect = if tab_row_drawable {
            logical_rect(
                tab_strip,
                tab_strip.width().saturating_sub(tab_button),
                tab_button,
                tab_button,
                direction,
            )
            .with_y(tab_y)
        } else {
            collapsed_logical_rect(tab_strip, direction).with_y(tab_y)
        };

        let mut tabs = Vec::new();
        let mut tab_titles = Vec::new();
        let mut tab_closes = Vec::new();
        tabs.try_reserve_exact(tab_count)
            .map_err(|_| browser_resource_error("could not reserve primary tab layout"))?;
        tab_titles
            .try_reserve_exact(tab_count)
            .map_err(|_| browser_resource_error("could not reserve primary tab title layout"))?;
        tab_closes
            .try_reserve_exact(tab_count)
            .map_err(|_| browser_resource_error("could not reserve primary tab close layout"))?;
        for index in 0..tab_count {
            let tab = split_logical_rect(tab_area, index, tab_count, direction);
            let close = directional_tab_close(geometry, tab, direction);
            let title = directional_tab_title(tab, close, direction);
            tabs.push(tab);
            tab_titles.push(title);
            tab_closes.push(close);
        }

        let hidden_controls: Box<[_]> = if show_new_tab {
            Box::new([])
        } else {
            Box::new([BrowserPrimaryControlKind::NewTab])
        };
        let show_overflow = !hidden_controls.is_empty();

        let nav_button = button.min(address_strip.height());
        let nav_y = address_strip
            .y()
            .saturating_add(address_strip.height().saturating_sub(nav_button) / 2);
        let start_width = nav_button
            .saturating_mul(3)
            .saturating_add(gap.saturating_mul(3));
        let end_count = 1_u32 + u32::from(show_overflow);
        let end_width = nav_button
            .saturating_mul(end_count)
            .saturating_add(gap.saturating_mul(end_count));
        let site_width = scaled_axis(SITE_IDENTITY_CSS_PX, scale)?.min(nav_button);
        let min_url = scaled_axis(MIN_URL_BAR_CSS_PX, scale)?;
        let required = start_width
            .saturating_add(end_width)
            .saturating_add(site_width)
            .saturating_add(gap)
            .saturating_add(min_url);
        let navigation_row_drawable =
            address_strip.height() > 0 && address_strip.width() >= required;
        let (url_container, site_rect, address_field) = if navigation_row_drawable {
            let url_width = address_strip
                .width()
                .saturating_sub(start_width)
                .saturating_sub(end_width);
            let url_container = logical_rect(
                address_strip,
                start_width,
                url_width,
                address_strip.height(),
                direction,
            );
            let site_rect = logical_rect(
                url_container,
                0,
                site_width,
                url_container.height(),
                direction,
            );
            let address_field = logical_rect(
                url_container,
                site_width.saturating_add(gap),
                url_width.saturating_sub(site_width.saturating_add(gap)),
                url_container.height(),
                direction,
            );
            (url_container, site_rect, address_field)
        } else {
            let collapsed = collapsed_logical_rect(address_strip, direction);
            (collapsed, collapsed, collapsed)
        };
        let nav_control = |logical_index: u32| {
            if navigation_row_drawable {
                logical_rect(
                    address_strip,
                    logical_index.saturating_mul(nav_button.saturating_add(gap)),
                    nav_button,
                    nav_button,
                    direction,
                )
                .with_y(nav_y)
            } else {
                collapsed_logical_rect(address_strip, direction).with_y(nav_y)
            }
        };
        let app_rect = if navigation_row_drawable {
            logical_rect(
                address_strip,
                address_strip.width().saturating_sub(nav_button),
                nav_button,
                nav_button,
                direction,
            )
            .with_y(nav_y)
        } else {
            collapsed_logical_rect(address_strip, direction).with_y(nav_y)
        };
        let overflow_rect = show_overflow.then(|| {
            if navigation_row_drawable {
                logical_rect(
                    address_strip,
                    address_strip
                        .width()
                        .saturating_sub(nav_button.saturating_mul(2).saturating_add(gap)),
                    nav_button,
                    nav_button,
                    direction,
                )
                .with_y(nav_y)
            } else {
                collapsed_logical_rect(address_strip, direction).with_y(nav_y)
            }
        });
        let controls = [
            preview_control(
                BrowserPrimaryControlKind::Back,
                BrowserPrimaryControlPlacement::Toolbar,
                Some(nav_control(0)),
            ),
            preview_control(
                BrowserPrimaryControlKind::Forward,
                BrowserPrimaryControlPlacement::Toolbar,
                Some(nav_control(1)),
            ),
            preview_control(
                BrowserPrimaryControlKind::ReloadStop,
                BrowserPrimaryControlPlacement::Toolbar,
                Some(nav_control(2)),
            ),
            preview_control(
                BrowserPrimaryControlKind::SiteIdentity,
                BrowserPrimaryControlPlacement::AddressField,
                Some(site_rect),
            ),
            preview_control(
                BrowserPrimaryControlKind::UrlBar,
                BrowserPrimaryControlPlacement::AddressField,
                Some(address_field),
            ),
            preview_control(
                BrowserPrimaryControlKind::NewTab,
                if show_new_tab {
                    BrowserPrimaryControlPlacement::Toolbar
                } else {
                    BrowserPrimaryControlPlacement::OverflowPanel
                },
                new_tab_rect,
            ),
            preview_control(
                BrowserPrimaryControlKind::AllTabs,
                BrowserPrimaryControlPlacement::Toolbar,
                Some(all_tabs_rect),
            ),
            preview_control(
                BrowserPrimaryControlKind::ApplicationMenu,
                BrowserPrimaryControlPlacement::Toolbar,
                Some(app_rect),
            ),
            preview_control(
                BrowserPrimaryControlKind::Overflow,
                if show_overflow {
                    BrowserPrimaryControlPlacement::Toolbar
                } else {
                    BrowserPrimaryControlPlacement::Hidden
                },
                overflow_rect,
            ),
        ];
        let popup_margin = scaled_axis(POPUP_MARGIN_CSS_PX, scale)?;
        let popup_padding = scaled_axis(POPUP_PADDING_CSS_PX, scale)?;
        let popup_row_height = scaled_axis(POPUP_ROW_HEIGHT_CSS_PX, scale)?;
        let popup_max_height = scaled_axis(POPUP_MAX_HEIGHT_CSS_PX, scale)?;
        let popup_min_width = scaled_axis(POPUP_MIN_WIDTH_CSS_PX, scale)?;
        let popup_available_width = surface
            .size()
            .width
            .saturating_sub(popup_margin.saturating_mul(2));
        let popup_available_height = surface
            .size()
            .height
            .saturating_sub(geometry.content().y())
            .saturating_sub(popup_margin.saturating_mul(2));
        let popup_capacity_height = popup_max_height.min(popup_available_height);
        let popup_row_capacity = if !navigation_row_drawable
            || popup_available_width < popup_min_width
            || popup_capacity_height
                < popup_row_height.saturating_add(popup_padding.saturating_mul(2))
        {
            0
        } else {
            usize::try_from(
                popup_capacity_height.saturating_sub(popup_padding.saturating_mul(2))
                    / popup_row_height,
            )
            .map_err(|_| browser_resource_error("primary popup row capacity overflowed"))?
            .min(MAX_BROWSER_PRIMARY_POPUP_ROWS)
        };
        Ok(Self {
            surface,
            direction,
            controls,
            hidden_controls,
            url_container,
            address_field,
            tabs: tabs.into_boxed_slice(),
            tab_titles: tab_titles.into_boxed_slice(),
            tab_closes: tab_closes.into_boxed_slice(),
            popup_row_capacity,
        })
    }

    /// Exact surface from which this preview was resolved.
    #[must_use]
    pub const fn surface(&self) -> WebRenderSurfaceSnapshot {
        self.surface
    }

    /// Logical layout direction.
    #[must_use]
    pub const fn direction(&self) -> BrowserChromeDirection {
        self.direction
    }

    /// Exact fixed kind-level control inventory.
    #[must_use]
    pub const fn controls(&self) -> &[BrowserPrimaryPreviewControl; MAX_BROWSER_PRIMARY_CONTROLS] {
        &self.controls
    }

    /// Exact controls relocated into overflow, in deterministic order.
    #[must_use]
    pub const fn hidden_controls(&self) -> &[BrowserPrimaryControlKind] {
        &self.hidden_controls
    }

    /// Combined site-identity and URL background rectangle.
    #[must_use]
    pub const fn url_container(&self) -> BrowserPhysicalRect {
        self.url_container
    }

    /// Canonical editable URL rectangle.
    #[must_use]
    pub const fn address_field(&self) -> BrowserPhysicalRect {
        self.address_field
    }

    /// Exact ordered tab body rectangles.
    #[must_use]
    pub const fn tab_rects(&self) -> &[BrowserPhysicalRect] {
        &self.tabs
    }

    /// Exact ordered tab-title rectangles with the direction-aware close edge
    /// removed. Empty narrow tabs retain an empty, bounded title rectangle.
    #[must_use]
    pub const fn tab_title_rects(&self) -> &[BrowserPhysicalRect] {
        &self.tab_titles
    }

    /// Exact ordered tab-close rectangles.
    #[must_use]
    pub const fn tab_close_rects(&self) -> &[BrowserPhysicalRect] {
        &self.tab_closes
    }

    /// Maximum rows visible in one popup scroll window on this exact surface.
    /// Zero means the surface cannot admit a popup.
    #[must_use]
    pub const fn popup_row_capacity(&self) -> usize {
        self.popup_row_capacity
    }

    /// Resolves one fixed kind without indexing by an unchecked integer.
    #[must_use]
    pub const fn control(&self, kind: BrowserPrimaryControlKind) -> BrowserPrimaryPreviewControl {
        self.controls[kind.index()]
    }
}

fn preview_control(
    kind: BrowserPrimaryControlKind,
    placement: BrowserPrimaryControlPlacement,
    rect: Option<BrowserPhysicalRect>,
) -> BrowserPrimaryPreviewControl {
    BrowserPrimaryPreviewControl {
        kind,
        placement,
        rect,
    }
}

/// One exact resolved fixed control, including derived visual state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BrowserResolvedPrimaryControl {
    element: BrowserChromeElementIdentity,
    kind: BrowserPrimaryControlKind,
    placement: BrowserPrimaryControlPlacement,
    rect: Option<BrowserPhysicalRect>,
    availability: BrowserElementAvailability,
    interaction: BrowserElementInteraction,
    focused: bool,
    open: bool,
    loading: bool,
}

impl BrowserResolvedPrimaryControl {
    /// Opaque stable identity.
    #[must_use]
    pub const fn element(self) -> BrowserChromeElementIdentity {
        self.element
    }

    /// Stable semantic kind.
    #[must_use]
    pub const fn kind(self) -> BrowserPrimaryControlKind {
        self.kind
    }

    /// Exact resolved placement.
    #[must_use]
    pub const fn placement(self) -> BrowserPrimaryControlPlacement {
        self.placement
    }

    /// Exact presented toolbar/address rectangle, if any. A zero-area value
    /// is a collapsed semantic member and grants no paint/hit authority.
    #[must_use]
    pub const fn rect(self) -> Option<BrowserPhysicalRect> {
        self.rect
    }

    /// Canonical action availability.
    #[must_use]
    pub const fn availability(self) -> BrowserElementAvailability {
        self.availability
    }

    /// Canonical pointer interaction.
    #[must_use]
    pub const fn interaction(self) -> BrowserElementInteraction {
        self.interaction
    }

    /// Whether the sole chrome focus names this exact control.
    #[must_use]
    pub const fn focused(self) -> bool {
        self.focused
    }

    /// Whether the sole open popup is anchored to this exact control.
    #[must_use]
    pub const fn open(self) -> bool {
        self.open
    }

    /// Whether this is `ReloadStop` in its sole Stop/loading mode.
    #[must_use]
    pub const fn loading(self) -> bool {
        self.loading
    }
}

/// One exact resolved popup row.
#[derive(Clone, Debug)]
pub struct BrowserResolvedPrimaryPopupRow {
    element: BrowserChromeElementIdentity,
    kind: BrowserPrimaryPopupRowKind,
    label: Arc<ShapedText>,
    rect: Option<BrowserPhysicalRect>,
    availability: BrowserElementAvailability,
    interaction: BrowserElementInteraction,
    selection: BrowserElementSelection,
    expansion: BrowserElementExpansion,
    focused: bool,
}

impl BrowserResolvedPrimaryPopupRow {
    /// Opaque stable row identity.
    #[must_use]
    pub const fn element(&self) -> BrowserChromeElementIdentity {
        self.element
    }

    /// Stable row semantic kind.
    #[must_use]
    pub const fn kind(&self) -> BrowserPrimaryPopupRowKind {
        self.kind
    }

    /// Exact shaped row label after tab/control derivation.
    #[must_use]
    pub const fn label(&self) -> &Arc<ShapedText> {
        &self.label
    }

    /// Visible row rectangle, absent outside the supplied scroll window.
    #[must_use]
    pub const fn rect(&self) -> Option<BrowserPhysicalRect> {
        self.rect
    }

    /// Canonical action availability.
    #[must_use]
    pub const fn availability(&self) -> BrowserElementAvailability {
        self.availability
    }

    /// Canonical pointer interaction.
    #[must_use]
    pub const fn interaction(&self) -> BrowserElementInteraction {
        self.interaction
    }

    /// Canonical selection state.
    #[must_use]
    pub const fn selection(&self) -> BrowserElementSelection {
        self.selection
    }

    /// Canonical expansion state.
    #[must_use]
    pub const fn expansion(&self) -> BrowserElementExpansion {
        self.expansion
    }

    /// Whether the sole chrome focus names this exact visible row.
    #[must_use]
    pub const fn focused(&self) -> bool {
        self.focused
    }
}

/// Exact resolved popup geometry and complete row inventory.
#[derive(Clone, Debug)]
pub struct BrowserResolvedPrimaryPopup {
    kind: BrowserPrimaryPopupKind,
    anchor: BrowserChromeElementIdentity,
    rect: BrowserPhysicalRect,
    rows: Box<[BrowserResolvedPrimaryPopupRow]>,
    first_visible_row: usize,
    visible_row_count: usize,
}

impl BrowserResolvedPrimaryPopup {
    /// Popup semantic kind.
    #[must_use]
    pub const fn kind(&self) -> BrowserPrimaryPopupKind {
        self.kind
    }

    /// Exact stable anchor identity.
    #[must_use]
    pub const fn anchor(&self) -> BrowserChromeElementIdentity {
        self.anchor
    }

    /// Clamped physical panel rectangle.
    #[must_use]
    pub const fn rect(&self) -> BrowserPhysicalRect {
        self.rect
    }

    /// Complete row inventory; rows outside the scroll window have no rect.
    #[must_use]
    pub const fn rows(&self) -> &[BrowserResolvedPrimaryPopupRow] {
        &self.rows
    }

    /// First row in the exact visible window.
    #[must_use]
    pub const fn first_visible_row(&self) -> usize {
        self.first_visible_row
    }

    /// Exact visible row count.
    #[must_use]
    pub const fn visible_row_count(&self) -> usize {
        self.visible_row_count
    }
}

/// Frozen primary layout retained by the scene and successful hit receipt.
#[derive(Clone, Debug)]
pub struct BrowserPrimaryChromeLayout {
    preview: BrowserPrimaryLayoutPreview,
    controls: [BrowserResolvedPrimaryControl; MAX_BROWSER_PRIMARY_CONTROLS],
    popup: Option<BrowserResolvedPrimaryPopup>,
}

impl BrowserPrimaryChromeLayout {
    #[allow(clippy::too_many_lines)]
    pub(crate) fn resolve(
        geometry: BrowserChromeGeometry,
        tabs: &[BrowserChromeTab],
        active_tab: Option<BrowserTabIdentity>,
        focus: BrowserChromeFocus,
        state: &BrowserPrimaryChromeState,
    ) -> Result<Self, WebRenderWindowError> {
        let preview =
            BrowserPrimaryLayoutPreview::from_geometry(geometry, state.direction, tabs.len())?;
        validate_controls(state, &preview)?;
        if tabs.is_empty()
            && state.controls[BrowserPrimaryControlKind::AllTabs.index()].availability
                != BrowserElementAvailability::Disabled
        {
            return Err(browser_contract_error(
                WebRenderWindowErrorKind::Contract,
                "empty tab inventory requires a disabled all-tabs control",
            ));
        }
        let popup_anchor = state.popup.as_ref().map(BrowserPrimaryPopup::anchor);
        let controls = std::array::from_fn(|index| {
            let control = &state.controls[index];
            let preview_control = preview.controls[index];
            BrowserResolvedPrimaryControl {
                element: control.element,
                kind: control.kind,
                placement: preview_control.placement,
                rect: preview_control.rect,
                availability: control.availability,
                interaction: control.interaction,
                focused: match focus {
                    BrowserChromeFocus::PrimaryControl(element) => element == control.element,
                    BrowserChromeFocus::AddressBar => {
                        control.kind == BrowserPrimaryControlKind::UrlBar
                    }
                    _ => false,
                },
                open: popup_anchor == Some(control.element),
                loading: control.kind == BrowserPrimaryControlKind::ReloadStop
                    && state.reload_stop_mode == BrowserReloadStopMode::Stop,
            }
        });
        let popup = state
            .popup
            .as_ref()
            .map(|popup| resolve_popup(geometry, &preview, tabs, active_tab, focus, state, popup))
            .transpose()?;
        validate_focus(focus, tabs, state, &preview, popup.as_ref())?;
        Ok(Self {
            preview,
            controls,
            popup,
        })
    }

    /// Kind-level exact geometry and overflow inventory.
    #[must_use]
    pub const fn preview(&self) -> &BrowserPrimaryLayoutPreview {
        &self.preview
    }

    /// Exact fixed resolved controls in canonical semantic order.
    #[must_use]
    pub const fn controls(&self) -> &[BrowserResolvedPrimaryControl; MAX_BROWSER_PRIMARY_CONTROLS] {
        &self.controls
    }

    /// Sole resolved open popup, if any.
    #[must_use]
    pub const fn popup(&self) -> Option<&BrowserResolvedPrimaryPopup> {
        self.popup.as_ref()
    }

    /// Resolves one fixed control by semantic kind.
    #[must_use]
    pub const fn control(&self, kind: BrowserPrimaryControlKind) -> BrowserResolvedPrimaryControl {
        self.controls[kind.index()]
    }
}

fn validate_controls(
    state: &BrowserPrimaryChromeState,
    preview: &BrowserPrimaryLayoutPreview,
) -> Result<(), WebRenderWindowError> {
    if state.controls.len() != MAX_BROWSER_PRIMARY_CONTROLS {
        return Err(browser_contract_error(
            WebRenderWindowErrorKind::Contract,
            "primary chrome does not contain the exact fixed control inventory",
        ));
    }
    for (index, expected) in BrowserPrimaryControlKind::ALL.into_iter().enumerate() {
        let control = &state.controls[index];
        if control.kind != expected {
            return Err(browser_contract_error(
                WebRenderWindowErrorKind::Contract,
                "primary controls are absent, duplicated, or outside canonical order",
            ));
        }
        if state.controls[..index]
            .iter()
            .any(|prior| prior.element == control.element)
        {
            return Err(browser_contract_error(
                WebRenderWindowErrorKind::Contract,
                "primary controls contain duplicate opaque element identities",
            ));
        }
        validate_shaped_chrome_text(&control.label)?;
        validate_availability_interaction(control.availability, control.interaction)?;
    }
    let overflow = &state.controls[BrowserPrimaryControlKind::Overflow.index()];
    let expected_overflow = if preview.hidden_controls.is_empty() || preview.popup_row_capacity == 0
    {
        BrowserElementAvailability::Disabled
    } else {
        BrowserElementAvailability::Enabled
    };
    if overflow.availability != expected_overflow {
        return Err(browser_contract_error(
            WebRenderWindowErrorKind::Contract,
            "overflow availability differs from the exact resolved hidden inventory",
        ));
    }
    if preview.popup_row_capacity == 0 {
        for kind in [
            BrowserPrimaryControlKind::SiteIdentity,
            BrowserPrimaryControlKind::AllTabs,
            BrowserPrimaryControlKind::ApplicationMenu,
            BrowserPrimaryControlKind::Overflow,
        ] {
            if state.controls[kind.index()].availability != BrowserElementAvailability::Disabled {
                return Err(browser_contract_error(
                    WebRenderWindowErrorKind::Contract,
                    "zero-capacity surface requires every popup anchor to be disabled",
                ));
            }
        }
        if state.popup.is_some() {
            return Err(browser_contract_error(
                WebRenderWindowErrorKind::Contract,
                "zero-capacity surface cannot retain an open primary popup",
            ));
        }
    }
    if state.site_identity == BrowserSiteIdentityKind::Empty
        && state.controls[BrowserPrimaryControlKind::SiteIdentity.index()].availability
            != BrowserElementAvailability::Disabled
    {
        return Err(browser_contract_error(
            WebRenderWindowErrorKind::Contract,
            "empty site identity cannot expose an enabled identity action",
        ));
    }
    Ok(())
}

fn validate_availability_interaction(
    availability: BrowserElementAvailability,
    interaction: BrowserElementInteraction,
) -> Result<(), WebRenderWindowError> {
    if availability == BrowserElementAvailability::Disabled
        && interaction != BrowserElementInteraction::Idle
    {
        return Err(browser_contract_error(
            WebRenderWindowErrorKind::Contract,
            "a disabled primary element cannot be hovered or pressed",
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn resolve_popup(
    geometry: BrowserChromeGeometry,
    preview: &BrowserPrimaryLayoutPreview,
    tabs: &[BrowserChromeTab],
    active_tab: Option<BrowserTabIdentity>,
    focus: BrowserChromeFocus,
    state: &BrowserPrimaryChromeState,
    popup: &BrowserPrimaryPopup,
) -> Result<BrowserResolvedPrimaryPopup, WebRenderWindowError> {
    if popup.rows.is_empty() || popup.rows.len() > MAX_BROWSER_PRIMARY_POPUP_ROWS {
        return Err(browser_resource_error(
            "primary popup row count is zero or exceeds its fixed limit",
        ));
    }
    let expected_anchor_kind = popup.kind.anchor_kind();
    let anchor_control = &state.controls[expected_anchor_kind.index()];
    if popup.anchor != anchor_control.element
        || anchor_control.availability != BrowserElementAvailability::Enabled
    {
        return Err(browser_contract_error(
            WebRenderWindowErrorKind::Contract,
            "primary popup anchor is foreign, mismatched, or disabled",
        ));
    }
    let Some(anchor_rect) = preview.control(expected_anchor_kind).rect else {
        return Err(browser_contract_error(
            WebRenderWindowErrorKind::Contract,
            "primary popup anchor is not visible in the resolved layout",
        ));
    };
    for (index, row) in popup.rows.iter().enumerate() {
        validate_availability_interaction(row.availability, row.interaction)?;
        if row.expansion != BrowserElementExpansion::Leaf {
            return Err(browser_contract_error(
                WebRenderWindowErrorKind::Contract,
                "W7 primary popup rows cannot claim an unimplemented child view",
            ));
        }
        if popup.rows[..index]
            .iter()
            .any(|prior| prior.element == row.element)
        {
            return Err(browser_contract_error(
                WebRenderWindowErrorKind::Contract,
                "primary popup contains duplicate row identities",
            ));
        }
        if popup.kind != BrowserPrimaryPopupKind::Overflow
            && state
                .controls
                .iter()
                .any(|control| control.element == row.element)
        {
            return Err(browser_contract_error(
                WebRenderWindowErrorKind::Contract,
                "non-overflow popup row collides with a fixed control identity",
            ));
        }
        if let Some(label) = row.action_label.as_ref() {
            validate_shaped_chrome_text(label)?;
        }
    }
    match popup.kind {
        BrowserPrimaryPopupKind::AllTabs => {
            validate_all_tabs_popup(tabs, active_tab, popup)?;
        }
        BrowserPrimaryPopupKind::Overflow => validate_overflow_popup(preview, state, popup)?,
        BrowserPrimaryPopupKind::ApplicationMenu => {
            validate_application_popup(active_tab, state, popup)?;
        }
        BrowserPrimaryPopupKind::SiteIdentity => validate_site_popup(popup)?,
    }

    let scale = geometry.surface().descriptor().scale.get();
    let margin = scaled_axis(POPUP_MARGIN_CSS_PX, scale)?;
    let padding = scaled_axis(POPUP_PADDING_CSS_PX, scale)?;
    let row_height = scaled_axis(POPUP_ROW_HEIGHT_CSS_PX, scale)?;
    let requested_width = scaled_axis(POPUP_WIDTH_CSS_PX, scale)?;
    let min_width = scaled_axis(POPUP_MIN_WIDTH_CSS_PX, scale)?;
    let max_height = scaled_axis(POPUP_MAX_HEIGHT_CSS_PX, scale)?;
    let surface = geometry.surface().size();
    let available_width = surface.width.saturating_sub(margin.saturating_mul(2));
    let available_height = surface
        .height
        .saturating_sub(geometry.content().y())
        .saturating_sub(margin.saturating_mul(2));
    if available_width < min_width
        || available_height < row_height.saturating_add(padding.saturating_mul(2))
    {
        return Err(browser_contract_error(
            WebRenderWindowErrorKind::Contract,
            "popup was supplied after the pure layout collapsed its viewport",
        ));
    }
    let panel_width = requested_width.min(available_width);
    let panel_capacity_height = max_height.min(available_height);
    let visible_capacity = preview.popup_row_capacity;
    if visible_capacity == 0 {
        return Err(browser_contract_error(
            WebRenderWindowErrorKind::Contract,
            "popup was supplied with zero resolved row capacity",
        ));
    }
    let recomputed_capacity = usize::try_from(
        panel_capacity_height.saturating_sub(padding.saturating_mul(2)) / row_height,
    )
    .map_err(|_| browser_resource_error("primary popup visible capacity overflowed"))?
    .min(MAX_BROWSER_PRIMARY_POPUP_ROWS);
    if recomputed_capacity != visible_capacity {
        return Err(browser_contract_error(
            WebRenderWindowErrorKind::Contract,
            "primary popup capacity drifted from the pure layout preview",
        ));
    }
    let visible_row_count = popup.rows.len().min(visible_capacity);
    let maximum_first = popup.rows.len().saturating_sub(visible_row_count);
    if popup.first_visible_row > maximum_first {
        return Err(browser_contract_error(
            WebRenderWindowErrorKind::Contract,
            "primary popup scroll window skips beyond its exact row inventory",
        ));
    }
    let visible_u32 = u32::try_from(visible_row_count)
        .map_err(|_| browser_resource_error("primary popup visible row count overflowed"))?;
    let panel_height = padding
        .saturating_mul(2)
        .saturating_add(row_height.saturating_mul(visible_u32));
    let unclamped_x = match state.direction {
        BrowserChromeDirection::LeftToRight => anchor_rect
            .x()
            .saturating_add(anchor_rect.width())
            .saturating_sub(panel_width),
        BrowserChromeDirection::RightToLeft => anchor_rect.x(),
    };
    let minimum_x = margin;
    let maximum_x = surface
        .width
        .saturating_sub(margin)
        .saturating_sub(panel_width);
    let panel_x = unclamped_x.clamp(minimum_x, maximum_x);
    let panel_y = geometry.content().y().saturating_add(margin);
    let panel_rect = BrowserPhysicalRect::new(panel_x, panel_y, panel_width, panel_height);

    let mut rows = Vec::new();
    rows.try_reserve_exact(popup.rows.len())
        .map_err(|_| browser_resource_error("could not reserve resolved primary popup rows"))?;
    let visible_end = popup
        .first_visible_row
        .checked_add(visible_row_count)
        .ok_or_else(|| browser_resource_error("primary popup visible range overflowed"))?;
    for (index, row) in popup.rows.iter().enumerate() {
        let label = resolve_popup_label(row, tabs, state)?;
        let rect = (popup.first_visible_row..visible_end)
            .contains(&index)
            .then(|| {
                let visible_index = u32::try_from(index - popup.first_visible_row)
                    .expect("bounded visible popup index");
                BrowserPhysicalRect::new(
                    panel_rect.x().saturating_add(padding),
                    panel_rect
                        .y()
                        .saturating_add(padding)
                        .saturating_add(row_height.saturating_mul(visible_index)),
                    panel_rect.width().saturating_sub(padding.saturating_mul(2)),
                    row_height,
                )
            });
        rows.push(BrowserResolvedPrimaryPopupRow {
            element: row.element,
            kind: row.kind,
            label,
            rect,
            availability: row.availability,
            interaction: row.interaction,
            selection: row.selection,
            expansion: row.expansion,
            focused: matches!(focus, BrowserChromeFocus::PopupRow(element) if element == row.element),
        });
    }
    Ok(BrowserResolvedPrimaryPopup {
        kind: popup.kind,
        anchor: popup.anchor,
        rect: panel_rect,
        rows: rows.into_boxed_slice(),
        first_visible_row: popup.first_visible_row,
        visible_row_count,
    })
}

fn validate_all_tabs_popup(
    tabs: &[BrowserChromeTab],
    active_tab: Option<BrowserTabIdentity>,
    popup: &BrowserPrimaryPopup,
) -> Result<(), WebRenderWindowError> {
    if popup.rows.len() != tabs.len() {
        return Err(browser_contract_error(
            WebRenderWindowErrorKind::Contract,
            "all-tabs popup does not contain the exact live tab inventory",
        ));
    }
    for (row, tab) in popup.rows.iter().zip(tabs) {
        if row.kind != BrowserPrimaryPopupRowKind::Tab(tab.identity())
            || row.action_label.is_some()
            || row.availability != BrowserElementAvailability::Enabled
        {
            return Err(browser_contract_error(
                WebRenderWindowErrorKind::Contract,
                "all-tabs row identity, order, or semantic tab differs from live tabs",
            ));
        }
        let expected_selection = if active_tab == Some(tab.identity()) {
            BrowserElementSelection::Selected
        } else {
            BrowserElementSelection::NotSelected
        };
        if row.selection != expected_selection {
            return Err(browser_contract_error(
                WebRenderWindowErrorKind::Contract,
                "all-tabs selected state differs from the exact active tab",
            ));
        }
    }
    Ok(())
}

fn validate_overflow_popup(
    preview: &BrowserPrimaryLayoutPreview,
    state: &BrowserPrimaryChromeState,
    popup: &BrowserPrimaryPopup,
) -> Result<(), WebRenderWindowError> {
    if popup.rows.len() != preview.hidden_controls.len() {
        return Err(browser_contract_error(
            WebRenderWindowErrorKind::Contract,
            "overflow popup differs from the exact resolved hidden-control inventory",
        ));
    }
    for (row, hidden) in popup
        .rows
        .iter()
        .zip(preview.hidden_controls.iter().copied())
    {
        let control = &state.controls[hidden.index()];
        if row.kind != BrowserPrimaryPopupRowKind::Control(hidden)
            || row.element != control.element
            || row.action_label.is_some()
            || row.availability != control.availability
            || row.interaction != control.interaction
            || row.selection != BrowserElementSelection::NotSelected
        {
            return Err(browser_contract_error(
                WebRenderWindowErrorKind::Contract,
                "overflow row did not retain exact control identity, kind, or canonical state",
            ));
        }
    }
    Ok(())
}

fn validate_application_popup(
    active_tab: Option<BrowserTabIdentity>,
    state: &BrowserPrimaryChromeState,
    popup: &BrowserPrimaryPopup,
) -> Result<(), WebRenderWindowError> {
    for (index, row) in popup.rows.iter().enumerate() {
        let BrowserPrimaryPopupRowKind::Action(action) = row.kind else {
            return Err(browser_contract_error(
                WebRenderWindowErrorKind::Contract,
                "application popup contains a non-action row",
            ));
        };
        if row.action_label.is_none()
            || row.selection != BrowserElementSelection::NotSelected
            || popup.rows[..index].iter().any(
                |prior| matches!(prior.kind, BrowserPrimaryPopupRowKind::Action(prior) if prior == action),
            )
        {
            return Err(browser_contract_error(
                WebRenderWindowErrorKind::Contract,
                "application popup action is unlabeled, selected, or duplicated",
            ));
        }
        let expected = match action {
            BrowserPrimaryActionKind::NewTab => Some(BrowserPrimaryControlKind::NewTab),
            BrowserPrimaryActionKind::Back => Some(BrowserPrimaryControlKind::Back),
            BrowserPrimaryActionKind::Forward => Some(BrowserPrimaryControlKind::Forward),
            BrowserPrimaryActionKind::ReloadStop => Some(BrowserPrimaryControlKind::ReloadStop),
            BrowserPrimaryActionKind::CloseTab => None,
            BrowserPrimaryActionKind::SiteInformation
            | BrowserPrimaryActionKind::SitePermissions => {
                return Err(browser_contract_error(
                    WebRenderWindowErrorKind::Contract,
                    "application popup contains an action outside its W7 dispatch mapping",
                ));
            }
        };
        if let Some(control_kind) = expected {
            if row.availability != state.controls[control_kind.index()].availability {
                return Err(browser_contract_error(
                    WebRenderWindowErrorKind::Contract,
                    "application action availability differs from its exact control action",
                ));
            }
        } else if row.availability == BrowserElementAvailability::Enabled && active_tab.is_none() {
            return Err(browser_contract_error(
                WebRenderWindowErrorKind::Contract,
                "close-tab application action is enabled without an active tab",
            ));
        }
    }
    Ok(())
}

fn validate_site_popup(popup: &BrowserPrimaryPopup) -> Result<(), WebRenderWindowError> {
    for (index, row) in popup.rows.iter().enumerate() {
        let BrowserPrimaryPopupRowKind::Action(action) = row.kind else {
            return Err(browser_contract_error(
                WebRenderWindowErrorKind::Contract,
                "site identity popup contains a non-informational row",
            ));
        };
        if !matches!(
            action,
            BrowserPrimaryActionKind::SiteInformation
                | BrowserPrimaryActionKind::SitePermissions
        ) || row.action_label.is_none()
            || row.availability != BrowserElementAvailability::Disabled
            || row.selection != BrowserElementSelection::NotSelected
            || popup.rows[..index].iter().any(
                |prior| matches!(prior.kind, BrowserPrimaryPopupRowKind::Action(prior) if prior == action),
            )
        {
            return Err(browser_contract_error(
                WebRenderWindowErrorKind::Contract,
                "W7 site identity rows must be unique disabled informational actions",
            ));
        }
    }
    Ok(())
}

fn resolve_popup_label(
    row: &BrowserPrimaryPopupRow,
    tabs: &[BrowserChromeTab],
    state: &BrowserPrimaryChromeState,
) -> Result<Arc<ShapedText>, WebRenderWindowError> {
    match row.kind {
        BrowserPrimaryPopupRowKind::Tab(identity) => tabs
            .iter()
            .find(|tab| tab.identity() == identity)
            .map(|tab| Arc::clone(tab.title()))
            .ok_or_else(|| {
                browser_contract_error(
                    WebRenderWindowErrorKind::Contract,
                    "popup row names a foreign tab identity",
                )
            }),
        BrowserPrimaryPopupRowKind::Control(kind) => {
            Ok(Arc::clone(state.controls[kind.index()].label()))
        }
        BrowserPrimaryPopupRowKind::Action(_) => row.action_label.clone().ok_or_else(|| {
            browser_contract_error(
                WebRenderWindowErrorKind::Text,
                "popup action has no exact shaped label",
            )
        }),
    }
}

fn validate_focus(
    focus: BrowserChromeFocus,
    tabs: &[BrowserChromeTab],
    state: &BrowserPrimaryChromeState,
    preview: &BrowserPrimaryLayoutPreview,
    popup: Option<&BrowserResolvedPrimaryPopup>,
) -> Result<(), WebRenderWindowError> {
    match focus {
        BrowserChromeFocus::PrimaryControl(element) => {
            let Some(control) = state
                .controls
                .iter()
                .find(|control| control.element == element)
            else {
                return Err(browser_contract_error(
                    WebRenderWindowErrorKind::Contract,
                    "focused primary control identity is absent",
                ));
            };
            if control.kind == BrowserPrimaryControlKind::UrlBar
                || control.availability == BrowserElementAvailability::Disabled
                || preview.control(control.kind).rect.is_none()
            {
                return Err(browser_contract_error(
                    WebRenderWindowErrorKind::Contract,
                    "URL-editor or disabled focus must use its canonical focus path",
                ));
            }
        }
        BrowserChromeFocus::PopupRow(element) => {
            let Some(row) =
                popup.and_then(|popup| popup.rows.iter().find(|row| row.element == element))
            else {
                return Err(browser_contract_error(
                    WebRenderWindowErrorKind::Contract,
                    "focused popup row identity is absent from the sole open popup",
                ));
            };
            if row.rect.is_none() || row.availability == BrowserElementAvailability::Disabled {
                return Err(browser_contract_error(
                    WebRenderWindowErrorKind::Contract,
                    "focused popup row is scrolled out or disabled",
                ));
            }
        }
        BrowserChromeFocus::AddressBar => {
            if state.controls[BrowserPrimaryControlKind::UrlBar.index()].availability
                == BrowserElementAvailability::Disabled
            {
                return Err(browser_contract_error(
                    WebRenderWindowErrorKind::Contract,
                    "disabled URL editor cannot own address focus",
                ));
            }
        }
        BrowserChromeFocus::Tab(identity) => {
            if !tabs.iter().any(|tab| tab.identity() == identity) {
                return Err(browser_contract_error(
                    WebRenderWindowErrorKind::Contract,
                    "focused tab identity is absent from primary layout",
                ));
            }
        }
        BrowserChromeFocus::None | BrowserChromeFocus::Page => {}
    }
    Ok(())
}

fn logical_rect(
    container: BrowserPhysicalRect,
    logical_x: u32,
    width: u32,
    height: u32,
    direction: BrowserChromeDirection,
) -> BrowserPhysicalRect {
    let logical_x = logical_x.min(container.width());
    let width = width.min(container.width().saturating_sub(logical_x));
    let x = match direction {
        BrowserChromeDirection::LeftToRight => container.x().saturating_add(logical_x),
        BrowserChromeDirection::RightToLeft => container.x().saturating_add(
            container
                .width()
                .saturating_sub(logical_x)
                .saturating_sub(width),
        ),
    };
    BrowserPhysicalRect::new(x, container.y(), width, height.min(container.height()))
}

fn collapsed_logical_rect(
    container: BrowserPhysicalRect,
    direction: BrowserChromeDirection,
) -> BrowserPhysicalRect {
    logical_rect(container, 0, 0, 0, direction)
}

fn split_logical_rect(
    container: BrowserPhysicalRect,
    index: usize,
    count: usize,
    direction: BrowserChromeDirection,
) -> BrowserPhysicalRect {
    if count == 0 {
        return BrowserPhysicalRect::new(container.x(), container.y(), 0, container.height());
    }
    let count = u32::try_from(count).expect("primary tab count is hard bounded");
    let index = u32::try_from(index).expect("primary tab index is hard bounded");
    let base = container.width() / count;
    let remainder = container.width() % count;
    let logical_x = base
        .saturating_mul(index)
        .saturating_add(index.min(remainder));
    let width = base + u32::from(index < remainder);
    logical_rect(container, logical_x, width, container.height(), direction)
}

fn directional_tab_close(
    geometry: BrowserChromeGeometry,
    tab: BrowserPhysicalRect,
    direction: BrowserChromeDirection,
) -> BrowserPhysicalRect {
    let ltr = geometry.tab_close_rect(tab);
    let x = match direction {
        BrowserChromeDirection::LeftToRight => ltr.x(),
        BrowserChromeDirection::RightToLeft => tab.x(),
    };
    BrowserPhysicalRect::new(x, tab.y(), ltr.width(), ltr.height())
}

fn directional_tab_title(
    tab: BrowserPhysicalRect,
    close: BrowserPhysicalRect,
    direction: BrowserChromeDirection,
) -> BrowserPhysicalRect {
    let close_width = close.width().min(tab.width());
    let available = tab.width().saturating_sub(close_width);
    let outer_inset = 8.min(available / 4);
    let close_gap = 4.min(available.saturating_sub(outer_inset) / 3);
    let width = available
        .saturating_sub(outer_inset)
        .saturating_sub(close_gap);
    let logical_x = match direction {
        BrowserChromeDirection::LeftToRight => outer_inset,
        BrowserChromeDirection::RightToLeft => close_width.saturating_add(close_gap),
    };
    BrowserPhysicalRect::new(
        tab.x().saturating_add(logical_x),
        tab.y(),
        width,
        tab.height(),
    )
}

trait BrowserPhysicalRectExt {
    fn with_y(self, y: u32) -> Self;
}

impl BrowserPhysicalRectExt for BrowserPhysicalRect {
    fn with_y(self, y: u32) -> Self {
        Self::new(self.x(), y, self.width(), self.height())
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;
    use std::sync::Arc;

    use wild_buzzard_platform::{
        PhysicalSize, PixelFormat, ScaleFactor, SurfaceDescriptor, SurfaceIdAllocator,
        SurfaceNamespace, SurfaceRole,
    };
    use wild_buzzard_text::{TextLimits, TextRequest, TextSystem};

    use super::{
        BrowserChromeDirection, BrowserChromeElementIdentity, BrowserElementAvailability,
        BrowserElementInteraction, BrowserElementSelection, BrowserPrimaryActionKind,
        BrowserPrimaryChromeState, BrowserPrimaryControl, BrowserPrimaryControlKind,
        BrowserPrimaryControlPlacement, BrowserPrimaryLayoutPreview, BrowserPrimaryPopup,
        BrowserPrimaryPopupKind, BrowserPrimaryPopupRow, BrowserReloadStopMode,
        BrowserSiteIdentityKind,
    };
    use crate::{
        BrowserChromeFocus, BrowserChromeRevision, BrowserChromeScene, BrowserChromeState,
        BrowserChromeTab, BrowserPhysicalRect, BrowserTabIdentity, MAX_BROWSER_CHROME_RUNS,
        MAX_BROWSER_CHROME_TABS, MAX_BROWSER_CHROME_TEXTS, WebRenderSurfaceSnapshot,
        WebRenderWindowErrorKind,
    };

    fn surface(width: u32, height: u32, scale: f64) -> WebRenderSurfaceSnapshot {
        let mut allocator = SurfaceIdAllocator::new(
            SurfaceNamespace::new(7_004).expect("nonzero surface namespace"),
        );
        WebRenderSurfaceSnapshot::initial(SurfaceDescriptor {
            id: allocator.allocate().expect("surface identity"),
            size: PhysicalSize::new(width, height).expect("bounded surface"),
            scale: ScaleFactor::new(scale).expect("valid scale"),
            format: PixelFormat::Rgba8Srgb,
            role: SurfaceRole::Window,
        })
    }

    fn shape(text: &mut TextSystem, value: &str) -> Arc<wild_buzzard_text::ShapedText> {
        text.shape(&TextRequest::new(value, 14.0))
            .expect("fixture label shapes")
    }

    fn controls(
        text: &mut TextSystem,
        preview: &BrowserPrimaryLayoutPreview,
    ) -> Box<[BrowserPrimaryControl]> {
        BrowserPrimaryControlKind::ALL
            .into_iter()
            .enumerate()
            .map(|(index, kind)| {
                let availability = match kind {
                    BrowserPrimaryControlKind::Forward => BrowserElementAvailability::Disabled,
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
                BrowserPrimaryControl::new(
                    BrowserChromeElementIdentity::new(
                        100 + u64::try_from(index).expect("small control index"),
                    )
                    .expect("nonzero control identity"),
                    kind,
                    shape(text, &format!("{kind:?}")),
                    availability,
                )
            })
            .collect::<Vec<_>>()
            .into_boxed_slice()
    }

    fn tab(text: &mut TextSystem, identity: u64, title: &str) -> BrowserChromeTab {
        BrowserChromeTab::new(
            BrowserTabIdentity::new(identity).expect("nonzero tab identity"),
            shape(text, title),
        )
    }

    fn primary_scene(
        surface: WebRenderSurfaceSnapshot,
        direction: BrowserChromeDirection,
        tabs: Box<[BrowserChromeTab]>,
        active: Option<BrowserTabIdentity>,
        popup: Option<BrowserPrimaryPopup>,
        focus: BrowserChromeFocus,
    ) -> BrowserChromeScene {
        let mut text = TextSystem::new_deterministic(TextLimits::default())
            .expect("deterministic font initializes");
        let preview = BrowserPrimaryLayoutPreview::for_surface(surface, direction, tabs.len())
            .expect("primary preview");
        let controls = controls(&mut text, &preview);
        let primary = BrowserPrimaryChromeState::new(
            direction,
            controls,
            BrowserReloadStopMode::Stop,
            BrowserSiteIdentityKind::LoopbackHttp,
        )
        .with_popup(popup);
        BrowserChromeScene::new(
            BrowserChromeRevision::new(1).expect("chrome revision"),
            surface,
            BrowserChromeState::new(tabs, active, shape(&mut text, "http://127.0.0.1/"))
                .with_focus(focus)
                .with_primary_chrome(Some(primary)),
        )
        .expect("primary scene")
    }

    fn preview_signature(preview: &BrowserPrimaryLayoutPreview) -> String {
        let mut signature = format!(
            "dir={:?};url={:?};cap={};hidden={:?};tabs={:?};closes={:?}",
            preview.direction(),
            preview.address_field(),
            preview.popup_row_capacity(),
            preview.hidden_controls(),
            preview.tab_rects(),
            preview.tab_close_rects(),
        );
        for control in preview.controls() {
            write!(
                signature,
                ";{:?}={:?}:{:?}",
                control.kind(),
                control.placement(),
                control.rect()
            )
            .expect("String writes do not fail");
        }
        signature
    }

    fn assert_bounded(rect: BrowserPhysicalRect, size: PhysicalSize) {
        assert!(rect.x() <= size.width);
        assert!(rect.y() <= size.height);
        assert!(rect.width() <= size.width.saturating_sub(rect.x()));
        assert!(rect.height() <= size.height.saturating_sub(rect.y()));
    }

    fn assert_preview_is_bounded(preview: &BrowserPrimaryLayoutPreview, tab_count: usize) {
        let size = preview.surface().size();
        assert_eq!(preview.tab_rects().len(), tab_count);
        assert_eq!(preview.tab_title_rects().len(), tab_count);
        assert_eq!(preview.tab_close_rects().len(), tab_count);
        assert_bounded(preview.url_container(), size);
        assert_bounded(preview.address_field(), size);
        for control in preview.controls() {
            if let Some(rect) = control.rect() {
                assert_bounded(rect, size);
            }
        }
        for ((tab, title), close) in preview
            .tab_rects()
            .iter()
            .zip(preview.tab_title_rects())
            .zip(preview.tab_close_rects())
        {
            assert_bounded(*tab, size);
            assert_bounded(*title, size);
            assert_bounded(*close, size);
            assert!(title.x() >= tab.x());
            assert!(close.x() >= tab.x());
            assert!(title.width() <= tab.width().saturating_sub(title.x() - tab.x()));
            assert!(close.width() <= tab.width().saturating_sub(close.x() - tab.x()));
            match preview.direction() {
                BrowserChromeDirection::LeftToRight => {
                    assert!(title.x().saturating_add(title.width()) <= close.x());
                }
                BrowserChromeDirection::RightToLeft => {
                    assert!(close.x().saturating_add(close.width()) <= title.x());
                }
            }
        }
    }

    #[test]
    fn deterministic_primary_layout_reftest_covers_wide_narrow_rtl_and_scale() {
        let wide = BrowserPrimaryLayoutPreview::for_surface(
            surface(800, 600, 1.0),
            BrowserChromeDirection::LeftToRight,
            2,
        )
        .expect("wide LTR preview");
        assert_eq!(wide.popup_row_capacity(), 11);
        assert!(wide.hidden_controls().is_empty());
        assert_eq!(
            wide.address_field(),
            BrowserPhysicalRect::new(146, 42, 612, 32)
        );
        assert_eq!(wide.tab_rects()[0], BrowserPhysicalRect::new(0, 0, 364, 36));
        assert_eq!(
            wide.tab_rects()[1],
            BrowserPhysicalRect::new(364, 0, 364, 36)
        );
        assert_eq!(
            wide.control(BrowserPrimaryControlKind::NewTab).rect(),
            Some(BrowserPhysicalRect::new(732, 2, 32, 32))
        );
        assert_eq!(
            wide.control(BrowserPrimaryControlKind::Overflow)
                .placement(),
            BrowserPrimaryControlPlacement::Hidden
        );

        let rtl = BrowserPrimaryLayoutPreview::for_surface(
            surface(800, 600, 1.0),
            BrowserChromeDirection::RightToLeft,
            2,
        )
        .expect("wide RTL preview");
        assert_eq!(
            rtl.address_field(),
            BrowserPhysicalRect::new(42, 42, 612, 32)
        );
        assert_eq!(
            rtl.tab_rects()[0],
            BrowserPhysicalRect::new(436, 0, 364, 36)
        );
        assert_eq!(rtl.tab_rects()[1], BrowserPhysicalRect::new(72, 0, 364, 36));
        assert_eq!(rtl.tab_close_rects()[0].x(), 436);
        assert_eq!(
            rtl.control(BrowserPrimaryControlKind::Back).rect(),
            Some(BrowserPhysicalRect::new(762, 42, 32, 32))
        );

        let scaled = BrowserPrimaryLayoutPreview::for_surface(
            surface(1_600, 1_200, 2.0),
            BrowserChromeDirection::LeftToRight,
            2,
        )
        .expect("scaled preview");
        assert_eq!(
            scaled.address_field(),
            BrowserPhysicalRect::new(292, 84, 1_224, 64)
        );
        assert_eq!(
            scaled.tab_rects()[0],
            BrowserPhysicalRect::new(0, 0, 728, 72)
        );
        assert_eq!(scaled.popup_row_capacity(), 11);

        let narrow = BrowserPrimaryLayoutPreview::for_surface(
            surface(360, 600, 1.0),
            BrowserChromeDirection::LeftToRight,
            1,
        )
        .expect("narrow preview");
        assert_eq!(
            narrow.hidden_controls(),
            [BrowserPrimaryControlKind::NewTab]
        );
        assert_eq!(
            narrow
                .control(BrowserPrimaryControlKind::NewTab)
                .placement(),
            BrowserPrimaryControlPlacement::OverflowPanel
        );
        assert_eq!(
            narrow.control(BrowserPrimaryControlKind::NewTab).rect(),
            None
        );
        assert_eq!(
            narrow.control(BrowserPrimaryControlKind::Overflow).rect(),
            Some(BrowserPhysicalRect::new(286, 42, 32, 32))
        );

        assert_eq!(
            preview_signature(&narrow),
            "dir=LeftToRight;url=BrowserPhysicalRect { x: 146, y: 42, width: 136, height: 32 };cap=11;hidden=[NewTab];tabs=[BrowserPhysicalRect { x: 0, y: 0, width: 324, height: 36 }];closes=[BrowserPhysicalRect { x: 296, y: 0, width: 28, height: 36 }];Back=Toolbar:Some(BrowserPhysicalRect { x: 6, y: 42, width: 32, height: 32 });Forward=Toolbar:Some(BrowserPhysicalRect { x: 42, y: 42, width: 32, height: 32 });ReloadStop=Toolbar:Some(BrowserPhysicalRect { x: 78, y: 42, width: 32, height: 32 });SiteIdentity=AddressField:Some(BrowserPhysicalRect { x: 114, y: 42, width: 28, height: 32 });UrlBar=AddressField:Some(BrowserPhysicalRect { x: 146, y: 42, width: 136, height: 32 });NewTab=OverflowPanel:None;AllTabs=Toolbar:Some(BrowserPhysicalRect { x: 328, y: 2, width: 32, height: 32 });ApplicationMenu=Toolbar:Some(BrowserPhysicalRect { x: 322, y: 42, width: 32, height: 32 });Overflow=Toolbar:Some(BrowserPhysicalRect { x: 286, y: 42, width: 32, height: 32 })"
        );
    }

    #[test]
    fn tab_title_reftest_reserves_the_physical_close_edge_in_both_directions() {
        let ltr = BrowserPrimaryLayoutPreview::for_surface(
            surface(800, 600, 1.0),
            BrowserChromeDirection::LeftToRight,
            2,
        )
        .expect("LTR title layout");
        assert_eq!(
            ltr.tab_title_rects()[0],
            BrowserPhysicalRect::new(8, 0, 324, 36)
        );
        assert_eq!(
            ltr.tab_close_rects()[0],
            BrowserPhysicalRect::new(336, 0, 28, 36)
        );

        let rtl = BrowserPrimaryLayoutPreview::for_surface(
            surface(800, 600, 1.0),
            BrowserChromeDirection::RightToLeft,
            2,
        )
        .expect("RTL title layout");
        assert_eq!(
            rtl.tab_close_rects()[0],
            BrowserPhysicalRect::new(436, 0, 28, 36)
        );
        assert_eq!(
            rtl.tab_title_rects()[0],
            BrowserPhysicalRect::new(468, 0, 324, 36)
        );

        for direction in [
            BrowserChromeDirection::LeftToRight,
            BrowserChromeDirection::RightToLeft,
        ] {
            let narrow = BrowserPrimaryLayoutPreview::for_surface(
                surface(288, 600, 1.0),
                direction,
                MAX_BROWSER_CHROME_TABS,
            )
            .expect("maximum narrow-tab layout");
            assert_preview_is_bounded(&narrow, MAX_BROWSER_CHROME_TABS);
            assert!(
                narrow
                    .tab_title_rects()
                    .iter()
                    .all(|title| title.width() > 0)
            );
            assert!(
                narrow
                    .tab_close_rects()
                    .iter()
                    .all(|close| close.width() > 0)
            );
        }
    }

    #[test]
    fn every_nonzero_width_is_total_and_bounded_through_navigation_threshold() {
        const NAVIGATION_THRESHOLD: u32 = 288;
        for direction in [
            BrowserChromeDirection::LeftToRight,
            BrowserChromeDirection::RightToLeft,
        ] {
            for height in [1, 79, 80, 600] {
                for width in 1..=NAVIGATION_THRESHOLD + 1 {
                    let preview = BrowserPrimaryLayoutPreview::for_surface(
                        surface(width, height, 1.0),
                        direction,
                        MAX_BROWSER_CHROME_TABS,
                    )
                    .expect("every nonzero drawable width has a layout");
                    assert_preview_is_bounded(&preview, MAX_BROWSER_CHROME_TABS);
                    if height == 600 && width < NAVIGATION_THRESHOLD {
                        assert_eq!(preview.address_field().width(), 0);
                        assert_eq!(preview.popup_row_capacity(), 0);
                    }
                }
            }

            let below = BrowserPrimaryLayoutPreview::for_surface(
                surface(NAVIGATION_THRESHOLD - 1, 600, 1.0),
                direction,
                1,
            )
            .expect("threshold minus one collapses");
            let exact = BrowserPrimaryLayoutPreview::for_surface(
                surface(NAVIGATION_THRESHOLD, 600, 1.0),
                direction,
                1,
            )
            .expect("exact threshold paints");
            let above = BrowserPrimaryLayoutPreview::for_surface(
                surface(NAVIGATION_THRESHOLD + 1, 600, 1.0),
                direction,
                1,
            )
            .expect("threshold plus one paints");
            assert_eq!(below.address_field().width(), 0);
            assert_eq!(below.popup_row_capacity(), 0);
            assert_eq!(exact.address_field().width(), 64);
            assert_eq!(above.address_field().width(), 65);
            assert!(exact.popup_row_capacity() > 0);
            assert_eq!(exact.popup_row_capacity(), above.popup_row_capacity());

            let scaled_below =
                BrowserPrimaryLayoutPreview::for_surface(surface(575, 1_200, 2.0), direction, 1)
                    .expect("scale-two threshold minus one collapses");
            let scaled_exact =
                BrowserPrimaryLayoutPreview::for_surface(surface(576, 1_200, 2.0), direction, 1)
                    .expect("scale-two exact threshold paints");
            let scaled_above =
                BrowserPrimaryLayoutPreview::for_surface(surface(577, 1_200, 2.0), direction, 1)
                    .expect("scale-two threshold plus one paints");
            assert_eq!(scaled_below.address_field().width(), 0);
            assert_eq!(scaled_exact.address_field().width(), 128);
            assert_eq!(scaled_above.address_field().width(), 129);
        }
    }

    #[test]
    fn tiny_scene_construction_collapses_without_losing_semantic_membership() {
        for direction in [
            BrowserChromeDirection::LeftToRight,
            BrowserChromeDirection::RightToLeft,
        ] {
            for (width, height) in [
                (1, 1),
                (1, 600),
                (287, 600),
                (288, 1),
                (288, 600),
                (289, 600),
            ] {
                let mut text = TextSystem::new_deterministic(TextLimits::default())
                    .expect("deterministic font initializes");
                let shared_title = shape(&mut text, "T");
                let tabs = (1..=MAX_BROWSER_CHROME_TABS)
                    .map(|identity| {
                        BrowserChromeTab::new(
                            BrowserTabIdentity::new(
                                u64::try_from(identity).expect("bounded identity"),
                            )
                            .expect("nonzero tab identity"),
                            Arc::clone(&shared_title),
                        )
                    })
                    .collect::<Vec<_>>()
                    .into_boxed_slice();
                let active = Some(tabs[0].identity());
                let scene = primary_scene(
                    surface(width, height, 1.0),
                    direction,
                    tabs,
                    active,
                    None,
                    BrowserChromeFocus::AddressBar,
                );
                let preview = scene.primary_layout().expect("primary layout").preview();
                assert_preview_is_bounded(preview, MAX_BROWSER_CHROME_TABS);
                assert_eq!(
                    preview.controls().len(),
                    BrowserPrimaryControlKind::ALL.len()
                );
                if width < 288 || height < 80 {
                    assert_eq!(preview.popup_row_capacity(), 0);
                }
            }
        }
    }

    #[test]
    fn resolved_control_state_has_one_source_for_loading_focus_hover_and_open() {
        let surface = surface(800, 600, 1.0);
        let mut text = TextSystem::new_deterministic(TextLimits::default())
            .expect("deterministic font initializes");
        let tabs = vec![tab(&mut text, 1, "Selected")].into_boxed_slice();
        let active = Some(tabs[0].identity());
        let preview = BrowserPrimaryLayoutPreview::for_surface(
            surface,
            BrowserChromeDirection::LeftToRight,
            tabs.len(),
        )
        .expect("preview");
        let mut controls = controls(&mut text, &preview);
        let reload = BrowserPrimaryControlKind::ReloadStop.index();
        controls[reload] = controls[reload]
            .clone()
            .with_interaction(BrowserElementInteraction::Hovered);
        let focus_element = controls[reload].element();
        let primary = BrowserPrimaryChromeState::new(
            BrowserChromeDirection::LeftToRight,
            controls,
            BrowserReloadStopMode::Stop,
            BrowserSiteIdentityKind::LoopbackHttp,
        );
        let scene = BrowserChromeScene::new(
            BrowserChromeRevision::new(1).expect("revision"),
            surface,
            BrowserChromeState::new(tabs, active, shape(&mut text, "about:state"))
                .with_focus(BrowserChromeFocus::PrimaryControl(focus_element))
                .with_primary_chrome(Some(primary)),
        )
        .expect("scene");
        let resolved = scene
            .primary_layout()
            .expect("resolved primary")
            .control(BrowserPrimaryControlKind::ReloadStop);
        assert!(resolved.loading());
        assert!(resolved.focused());
        assert_eq!(resolved.interaction(), BrowserElementInteraction::Hovered);
        assert!(!resolved.open());
        assert_eq!(scene.text_count(), 1 + 1 + 9);
    }

    #[test]
    fn zero_popup_capacity_disables_every_panel_anchor_and_admits_no_popup() {
        let surface = surface(360, 100, 1.0);
        let mut text = TextSystem::new_deterministic(TextLimits::default())
            .expect("deterministic font initializes");
        let tabs = vec![tab(&mut text, 1, "Tiny")].into_boxed_slice();
        let preview = BrowserPrimaryLayoutPreview::for_surface(
            surface,
            BrowserChromeDirection::LeftToRight,
            tabs.len(),
        )
        .expect("tiny primary layout still paints");
        assert_eq!(preview.popup_row_capacity(), 0);
        assert_eq!(
            preview.hidden_controls(),
            [BrowserPrimaryControlKind::NewTab]
        );
        let control_inventory = controls(&mut text, &preview);
        for kind in [
            BrowserPrimaryControlKind::SiteIdentity,
            BrowserPrimaryControlKind::AllTabs,
            BrowserPrimaryControlKind::ApplicationMenu,
            BrowserPrimaryControlKind::Overflow,
        ] {
            assert_eq!(
                control_inventory[kind.index()].availability(),
                BrowserElementAvailability::Disabled
            );
        }
        let primary = BrowserPrimaryChromeState::new(
            BrowserChromeDirection::LeftToRight,
            control_inventory,
            BrowserReloadStopMode::Reload,
            BrowserSiteIdentityKind::LoopbackHttp,
        );
        let scene = BrowserChromeScene::new(
            BrowserChromeRevision::new(1).expect("revision"),
            surface,
            BrowserChromeState::new(
                tabs.clone(),
                Some(tabs[0].identity()),
                shape(&mut text, "about:tiny"),
            )
            .with_primary_chrome(Some(primary)),
        )
        .expect("zero-capacity closed primary scene");
        assert_eq!(
            scene
                .primary_layout()
                .expect("primary")
                .control(BrowserPrimaryControlKind::Overflow)
                .availability(),
            BrowserElementAvailability::Disabled
        );

        let mut invalid_controls = controls(&mut text, &preview);
        let overflow_index = BrowserPrimaryControlKind::Overflow.index();
        let overflow_element = invalid_controls[overflow_index].element();
        let overflow_kind = invalid_controls[overflow_index].kind();
        let overflow_label = Arc::clone(invalid_controls[overflow_index].label());
        invalid_controls[overflow_index] = BrowserPrimaryControl::new(
            overflow_element,
            overflow_kind,
            overflow_label,
            BrowserElementAvailability::Enabled,
        );
        let invalid = BrowserPrimaryChromeState::new(
            BrowserChromeDirection::LeftToRight,
            invalid_controls,
            BrowserReloadStopMode::Reload,
            BrowserSiteIdentityKind::LoopbackHttp,
        );
        let error = BrowserChromeScene::new(
            BrowserChromeRevision::new(2).expect("revision"),
            surface,
            BrowserChromeState::new(
                tabs,
                BrowserTabIdentity::new(1),
                shape(&mut text, "about:tiny"),
            )
            .with_primary_chrome(Some(invalid)),
        )
        .expect_err("zero-capacity enabled overflow must reject");
        assert_eq!(error.kind(), WebRenderWindowErrorKind::Contract);
    }

    #[test]
    fn navigation_threshold_rejects_stale_popup_state_then_admits_exact_inventory() {
        for direction in [
            BrowserChromeDirection::LeftToRight,
            BrowserChromeDirection::RightToLeft,
        ] {
            let mut text = TextSystem::new_deterministic(TextLimits::default())
                .expect("deterministic font initializes");
            let tabs = vec![tab(&mut text, 1, "Threshold")].into_boxed_slice();

            let below_surface = surface(287, 600, 1.0);
            let below_preview =
                BrowserPrimaryLayoutPreview::for_surface(below_surface, direction, tabs.len())
                    .expect("below-threshold preview collapses");
            assert_eq!(below_preview.popup_row_capacity(), 0);
            let below_controls = controls(&mut text, &below_preview);
            let below_overflow =
                below_controls[BrowserPrimaryControlKind::Overflow.index()].element();
            let below_new_tab = below_controls[BrowserPrimaryControlKind::NewTab.index()].element();
            let below_popup = BrowserPrimaryPopup::new(
                BrowserPrimaryPopupKind::Overflow,
                below_overflow,
                vec![BrowserPrimaryPopupRow::relocated_control(
                    below_new_tab,
                    BrowserPrimaryControlKind::NewTab,
                    BrowserElementAvailability::Enabled,
                )]
                .into_boxed_slice(),
            );
            let below_primary = BrowserPrimaryChromeState::new(
                direction,
                below_controls,
                BrowserReloadStopMode::Reload,
                BrowserSiteIdentityKind::LoopbackHttp,
            )
            .with_popup(Some(below_popup));
            let error = BrowserChromeScene::new(
                BrowserChromeRevision::new(1).expect("revision"),
                below_surface,
                BrowserChromeState::new(
                    tabs.clone(),
                    Some(tabs[0].identity()),
                    shape(&mut text, "about:below"),
                )
                .with_primary_chrome(Some(below_primary)),
            )
            .expect_err("zero-capacity layout rejects stale supplied popup state");
            assert_eq!(error.kind(), WebRenderWindowErrorKind::Contract);

            let exact_surface = surface(288, 600, 1.0);
            let exact_preview =
                BrowserPrimaryLayoutPreview::for_surface(exact_surface, direction, tabs.len())
                    .expect("exact-threshold preview");
            assert!(exact_preview.popup_row_capacity() > 0);
            let exact_controls = controls(&mut text, &exact_preview);
            let exact_overflow =
                exact_controls[BrowserPrimaryControlKind::Overflow.index()].element();
            let exact_new_tab = exact_controls[BrowserPrimaryControlKind::NewTab.index()].element();
            let exact_popup = BrowserPrimaryPopup::new(
                BrowserPrimaryPopupKind::Overflow,
                exact_overflow,
                vec![BrowserPrimaryPopupRow::relocated_control(
                    exact_new_tab,
                    BrowserPrimaryControlKind::NewTab,
                    BrowserElementAvailability::Enabled,
                )]
                .into_boxed_slice(),
            );
            let exact_primary = BrowserPrimaryChromeState::new(
                direction,
                exact_controls,
                BrowserReloadStopMode::Reload,
                BrowserSiteIdentityKind::LoopbackHttp,
            )
            .with_popup(Some(exact_popup));
            let scene = BrowserChromeScene::new(
                BrowserChromeRevision::new(2).expect("revision"),
                exact_surface,
                BrowserChromeState::new(
                    tabs.clone(),
                    Some(tabs[0].identity()),
                    shape(&mut text, "about:exact"),
                )
                .with_primary_chrome(Some(exact_primary)),
            )
            .expect("exact threshold admits popup inventory");
            assert_eq!(
                scene
                    .primary_layout()
                    .and_then(super::BrowserPrimaryChromeLayout::popup)
                    .expect("resolved popup")
                    .visible_row_count(),
                1
            );
        }
    }

    #[test]
    fn overflow_popup_retains_hidden_control_identity_and_exact_scroll_capacity() {
        let surface = surface(360, 600, 1.0);
        let mut text = TextSystem::new_deterministic(TextLimits::default())
            .expect("deterministic font initializes");
        let tabs = vec![tab(&mut text, 1, "Only tab")].into_boxed_slice();
        let preview = BrowserPrimaryLayoutPreview::for_surface(
            surface,
            BrowserChromeDirection::LeftToRight,
            tabs.len(),
        )
        .expect("preview");
        let controls = controls(&mut text, &preview);
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
        let scene = BrowserChromeScene::new(
            BrowserChromeRevision::new(1).expect("revision"),
            surface,
            BrowserChromeState::new(
                tabs,
                BrowserTabIdentity::new(1),
                shape(&mut text, "about:overflow"),
            )
            .with_focus(BrowserChromeFocus::PopupRow(new_tab))
            .with_primary_chrome(Some(primary)),
        )
        .expect("overflow scene");
        let layout = scene.primary_layout().expect("primary layout");
        assert!(layout.control(BrowserPrimaryControlKind::Overflow).open());
        assert_eq!(
            layout.preview().hidden_controls(),
            [BrowserPrimaryControlKind::NewTab]
        );
        let row = &layout.popup().expect("open popup").rows()[0];
        assert_eq!(row.element(), new_tab);
        assert!(row.focused());
        assert!(row.rect().is_some());
        assert_eq!(layout.popup().expect("popup").visible_row_count(), 1);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn all_tabs_projection_requires_exact_order_active_selection_and_visible_focus() {
        let surface = surface(800, 600, 1.0);
        let mut text = TextSystem::new_deterministic(TextLimits::default())
            .expect("deterministic font initializes");
        let tabs = vec![tab(&mut text, 1, "One"), tab(&mut text, 2, "Two")].into_boxed_slice();
        let active = tabs[1].identity();
        let preview = BrowserPrimaryLayoutPreview::for_surface(
            surface,
            BrowserChromeDirection::LeftToRight,
            tabs.len(),
        )
        .expect("preview");
        let control_inventory = controls(&mut text, &preview);
        let anchor = control_inventory[BrowserPrimaryControlKind::AllTabs.index()].element();
        let first = BrowserChromeElementIdentity::new(501).expect("row identity");
        let second = BrowserChromeElementIdentity::new(502).expect("row identity");
        let rows = vec![
            BrowserPrimaryPopupRow::tab(
                first,
                tabs[0].identity(),
                BrowserElementAvailability::Enabled,
            ),
            BrowserPrimaryPopupRow::tab(
                second,
                tabs[1].identity(),
                BrowserElementAvailability::Enabled,
            )
            .with_selection(BrowserElementSelection::Selected),
        ];
        let popup = BrowserPrimaryPopup::new(
            BrowserPrimaryPopupKind::AllTabs,
            anchor,
            rows.into_boxed_slice(),
        );
        let primary = BrowserPrimaryChromeState::new(
            BrowserChromeDirection::LeftToRight,
            control_inventory,
            BrowserReloadStopMode::Reload,
            BrowserSiteIdentityKind::LoopbackHttp,
        )
        .with_popup(Some(popup));
        let scene = BrowserChromeScene::new(
            BrowserChromeRevision::new(1).expect("revision"),
            surface,
            BrowserChromeState::new(tabs, Some(active), shape(&mut text, "about:tabs"))
                .with_focus(BrowserChromeFocus::PopupRow(second))
                .with_primary_chrome(Some(primary)),
        )
        .expect("exact all-tabs scene");
        assert_eq!(
            scene
                .primary_layout()
                .expect("primary")
                .popup()
                .expect("popup")
                .rows()[1]
                .selection(),
            BrowserElementSelection::Selected
        );

        let mut text = TextSystem::new_deterministic(TextLimits::default())
            .expect("deterministic font initializes");
        let tabs = vec![tab(&mut text, 1, "One"), tab(&mut text, 2, "Two")].into_boxed_slice();
        let preview = BrowserPrimaryLayoutPreview::for_surface(
            surface,
            BrowserChromeDirection::LeftToRight,
            tabs.len(),
        )
        .expect("preview");
        let controls = controls(&mut text, &preview);
        let anchor = controls[BrowserPrimaryControlKind::AllTabs.index()].element();
        let wrong = BrowserPrimaryPopup::new(
            BrowserPrimaryPopupKind::AllTabs,
            anchor,
            vec![
                BrowserPrimaryPopupRow::tab(
                    BrowserChromeElementIdentity::new(501).expect("row"),
                    tabs[0].identity(),
                    BrowserElementAvailability::Enabled,
                ),
                BrowserPrimaryPopupRow::tab(
                    BrowserChromeElementIdentity::new(502).expect("row"),
                    tabs[1].identity(),
                    BrowserElementAvailability::Enabled,
                ),
            ]
            .into_boxed_slice(),
        );
        let primary = BrowserPrimaryChromeState::new(
            BrowserChromeDirection::LeftToRight,
            controls,
            BrowserReloadStopMode::Reload,
            BrowserSiteIdentityKind::LoopbackHttp,
        )
        .with_popup(Some(wrong));
        let error = BrowserChromeScene::new(
            BrowserChromeRevision::new(2).expect("revision"),
            surface,
            BrowserChromeState::new(tabs, Some(active), shape(&mut text, "about:tabs"))
                .with_primary_chrome(Some(primary)),
        )
        .expect_err("active tab selection drift must reject");
        assert_eq!(error.kind(), WebRenderWindowErrorKind::Contract);
    }

    #[test]
    fn disabled_interaction_and_unmapped_application_action_reject() {
        let surface = surface(800, 600, 1.0);
        let mut text = TextSystem::new_deterministic(TextLimits::default())
            .expect("deterministic font initializes");
        let tabs = vec![tab(&mut text, 1, "One")].into_boxed_slice();
        let preview = BrowserPrimaryLayoutPreview::for_surface(
            surface,
            BrowserChromeDirection::LeftToRight,
            tabs.len(),
        )
        .expect("preview");
        let mut invalid_controls = controls(&mut text, &preview);
        let forward = BrowserPrimaryControlKind::Forward.index();
        invalid_controls[forward] = invalid_controls[forward]
            .clone()
            .with_interaction(BrowserElementInteraction::Hovered);
        let invalid = BrowserPrimaryChromeState::new(
            BrowserChromeDirection::LeftToRight,
            invalid_controls,
            BrowserReloadStopMode::Reload,
            BrowserSiteIdentityKind::LoopbackHttp,
        );
        let error = BrowserChromeScene::new(
            BrowserChromeRevision::new(1).expect("revision"),
            surface,
            BrowserChromeState::new(
                tabs.clone(),
                Some(tabs[0].identity()),
                shape(&mut text, "about:invalid"),
            )
            .with_primary_chrome(Some(invalid)),
        )
        .expect_err("disabled hover must reject");
        assert_eq!(error.kind(), WebRenderWindowErrorKind::Contract);

        let app_controls = controls(&mut text, &preview);
        let app = app_controls[BrowserPrimaryControlKind::ApplicationMenu.index()].element();
        let popup = BrowserPrimaryPopup::new(
            BrowserPrimaryPopupKind::ApplicationMenu,
            app,
            vec![BrowserPrimaryPopupRow::action(
                BrowserChromeElementIdentity::new(800).expect("row"),
                BrowserPrimaryActionKind::SiteInformation,
                shape(&mut text, "Not an application action"),
                BrowserElementAvailability::Disabled,
            )]
            .into_boxed_slice(),
        );
        let invalid = BrowserPrimaryChromeState::new(
            BrowserChromeDirection::LeftToRight,
            app_controls,
            BrowserReloadStopMode::Reload,
            BrowserSiteIdentityKind::LoopbackHttp,
        )
        .with_popup(Some(popup));
        let error = BrowserChromeScene::new(
            BrowserChromeRevision::new(2).expect("revision"),
            surface,
            BrowserChromeState::new(
                tabs,
                BrowserTabIdentity::new(1),
                shape(&mut text, "about:invalid"),
            )
            .with_primary_chrome(Some(invalid)),
        )
        .expect_err("unmapped application action must reject");
        assert_eq!(error.kind(), WebRenderWindowErrorKind::Contract);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn application_and_site_panels_admit_only_their_exact_typed_rows() {
        let surface = surface(800, 600, 1.0);
        let mut text = TextSystem::new_deterministic(TextLimits::default())
            .expect("deterministic font initializes");
        let tabs = vec![tab(&mut text, 1, "One")].into_boxed_slice();
        let preview = BrowserPrimaryLayoutPreview::for_surface(
            surface,
            BrowserChromeDirection::LeftToRight,
            tabs.len(),
        )
        .expect("preview");
        let application_controls = controls(&mut text, &preview);
        let app =
            application_controls[BrowserPrimaryControlKind::ApplicationMenu.index()].element();
        let action_specs = [
            (
                BrowserPrimaryActionKind::NewTab,
                BrowserElementAvailability::Enabled,
            ),
            (
                BrowserPrimaryActionKind::CloseTab,
                BrowserElementAvailability::Enabled,
            ),
            (
                BrowserPrimaryActionKind::Back,
                BrowserElementAvailability::Enabled,
            ),
            (
                BrowserPrimaryActionKind::Forward,
                BrowserElementAvailability::Disabled,
            ),
            (
                BrowserPrimaryActionKind::ReloadStop,
                BrowserElementAvailability::Enabled,
            ),
        ];
        let rows = action_specs
            .into_iter()
            .enumerate()
            .map(|(index, (action, availability))| {
                BrowserPrimaryPopupRow::action(
                    BrowserChromeElementIdentity::new(
                        700 + u64::try_from(index).expect("small row index"),
                    )
                    .expect("row identity"),
                    action,
                    shape(&mut text, &format!("{action:?}")),
                    availability,
                )
            })
            .collect::<Vec<_>>();
        let focus = rows[0].element();
        let primary = BrowserPrimaryChromeState::new(
            BrowserChromeDirection::LeftToRight,
            application_controls,
            BrowserReloadStopMode::Reload,
            BrowserSiteIdentityKind::LoopbackHttp,
        )
        .with_popup(Some(BrowserPrimaryPopup::new(
            BrowserPrimaryPopupKind::ApplicationMenu,
            app,
            rows.into_boxed_slice(),
        )));
        let app_scene = BrowserChromeScene::new(
            BrowserChromeRevision::new(1).expect("revision"),
            surface,
            BrowserChromeState::new(
                tabs,
                BrowserTabIdentity::new(1),
                shape(&mut text, "about:app"),
            )
            .with_focus(BrowserChromeFocus::PopupRow(focus))
            .with_primary_chrome(Some(primary)),
        )
        .expect("application popup scene");
        assert_eq!(
            app_scene
                .primary_layout()
                .expect("primary")
                .popup()
                .expect("popup")
                .rows()
                .len(),
            5
        );

        let tabs = vec![tab(&mut text, 1, "One")].into_boxed_slice();
        let controls = controls(&mut text, &preview);
        let identity = controls[BrowserPrimaryControlKind::SiteIdentity.index()].element();
        let site_rows = [
            BrowserPrimaryPopupRow::action(
                BrowserChromeElementIdentity::new(800).expect("row"),
                BrowserPrimaryActionKind::SiteInformation,
                shape(&mut text, "Site information"),
                BrowserElementAvailability::Disabled,
            ),
            BrowserPrimaryPopupRow::action(
                BrowserChromeElementIdentity::new(801).expect("row"),
                BrowserPrimaryActionKind::SitePermissions,
                shape(&mut text, "Site permissions"),
                BrowserElementAvailability::Disabled,
            ),
        ];
        let primary = BrowserPrimaryChromeState::new(
            BrowserChromeDirection::LeftToRight,
            controls,
            BrowserReloadStopMode::Reload,
            BrowserSiteIdentityKind::LoopbackHttp,
        )
        .with_popup(Some(BrowserPrimaryPopup::new(
            BrowserPrimaryPopupKind::SiteIdentity,
            identity,
            Box::new(site_rows),
        )));
        let site_scene = BrowserChromeScene::new(
            BrowserChromeRevision::new(2).expect("revision"),
            surface,
            BrowserChromeState::new(
                tabs,
                BrowserTabIdentity::new(1),
                shape(&mut text, "http://127.0.0.1/"),
            )
            .with_focus(BrowserChromeFocus::PrimaryControl(identity))
            .with_primary_chrome(Some(primary)),
        )
        .expect("site popup scene");
        assert!(
            site_scene
                .primary_layout()
                .expect("primary")
                .control(BrowserPrimaryControlKind::SiteIdentity)
                .open()
        );
    }

    #[test]
    fn all_tabs_scroll_window_keeps_complete_inventory_and_visible_focus_exact() {
        let surface = surface(800, 600, 1.0);
        let mut text = TextSystem::new_deterministic(TextLimits::default())
            .expect("deterministic font initializes");
        let tabs = (0..20)
            .map(|index| {
                tab(
                    &mut text,
                    1 + u64::try_from(index).expect("small tab index"),
                    &format!("Tab {index}"),
                )
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();
        let active = tabs[15].identity();
        let preview = BrowserPrimaryLayoutPreview::for_surface(
            surface,
            BrowserChromeDirection::LeftToRight,
            tabs.len(),
        )
        .expect("preview");
        assert_eq!(preview.popup_row_capacity(), 11);
        let controls = controls(&mut text, &preview);
        let anchor = controls[BrowserPrimaryControlKind::AllTabs.index()].element();
        let rows = tabs
            .iter()
            .enumerate()
            .map(|(index, tab)| {
                let row = BrowserPrimaryPopupRow::tab(
                    BrowserChromeElementIdentity::new(
                        1_000 + u64::try_from(index).expect("small row index"),
                    )
                    .expect("row identity"),
                    tab.identity(),
                    BrowserElementAvailability::Enabled,
                );
                if tab.identity() == active {
                    row.with_selection(BrowserElementSelection::Selected)
                } else {
                    row
                }
            })
            .collect::<Vec<_>>();
        let focused_row = rows[15].element();
        let popup = BrowserPrimaryPopup::new(
            BrowserPrimaryPopupKind::AllTabs,
            anchor,
            rows.into_boxed_slice(),
        )
        .with_first_visible_row(9);
        let primary = BrowserPrimaryChromeState::new(
            BrowserChromeDirection::LeftToRight,
            controls,
            BrowserReloadStopMode::Reload,
            BrowserSiteIdentityKind::LoopbackHttp,
        )
        .with_popup(Some(popup));
        let scene = BrowserChromeScene::new(
            BrowserChromeRevision::new(1).expect("revision"),
            surface,
            BrowserChromeState::new(tabs, Some(active), shape(&mut text, "about:many-tabs"))
                .with_focus(BrowserChromeFocus::PopupRow(focused_row))
                .with_primary_chrome(Some(primary)),
        )
        .expect("scrolled all-tabs scene");
        let popup = scene
            .primary_layout()
            .expect("primary")
            .popup()
            .expect("popup");
        assert_eq!(popup.rows().len(), 20);
        assert_eq!(popup.first_visible_row(), 9);
        assert_eq!(popup.visible_row_count(), 11);
        assert!(popup.rows()[8].rect().is_none());
        assert!(popup.rows()[9].rect().is_some());
        assert!(popup.rows()[15].focused());
        assert!(popup.rows()[19].rect().is_some());
    }

    #[test]
    fn maximum_all_tabs_projection_fits_fixed_text_and_run_accounting() {
        let surface = surface(800, 600, 1.0);
        let mut text = TextSystem::new_deterministic(TextLimits::default())
            .expect("deterministic font initializes");
        let shared_title = shape(&mut text, "T");
        let tabs = (0..64)
            .map(|index| {
                BrowserChromeTab::new(
                    BrowserTabIdentity::new(1 + u64::try_from(index).expect("small tab index"))
                        .expect("tab identity"),
                    Arc::clone(&shared_title),
                )
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();
        let active = tabs[63].identity();
        let preview = BrowserPrimaryLayoutPreview::for_surface(
            surface,
            BrowserChromeDirection::LeftToRight,
            tabs.len(),
        )
        .expect("maximum-tab preview");
        assert_eq!(preview.popup_row_capacity(), 11);
        let controls = controls(&mut text, &preview);
        let anchor = controls[BrowserPrimaryControlKind::AllTabs.index()].element();
        let rows = tabs
            .iter()
            .enumerate()
            .map(|(index, tab)| {
                let row = BrowserPrimaryPopupRow::tab(
                    BrowserChromeElementIdentity::new(
                        2_000 + u64::try_from(index).expect("small row index"),
                    )
                    .expect("row identity"),
                    tab.identity(),
                    BrowserElementAvailability::Enabled,
                );
                if tab.identity() == active {
                    row.with_selection(BrowserElementSelection::Selected)
                } else {
                    row
                }
            })
            .collect::<Vec<_>>();
        let focused = rows[63].element();
        let popup = BrowserPrimaryPopup::new(
            BrowserPrimaryPopupKind::AllTabs,
            anchor,
            rows.into_boxed_slice(),
        )
        .with_first_visible_row(53);
        let primary = BrowserPrimaryChromeState::new(
            BrowserChromeDirection::LeftToRight,
            controls,
            BrowserReloadStopMode::Reload,
            BrowserSiteIdentityKind::LoopbackHttp,
        )
        .with_popup(Some(popup));
        let scene = BrowserChromeScene::new(
            BrowserChromeRevision::new(1).expect("revision"),
            surface,
            BrowserChromeState::new(tabs, Some(active), shape(&mut text, "about:max-tabs"))
                .with_focus(BrowserChromeFocus::PopupRow(focused))
                .with_primary_chrome(Some(primary)),
        )
        .expect("maximum all-tabs scene");
        assert_eq!(scene.text_count(), 138);
        assert!(scene.text_count() <= MAX_BROWSER_CHROME_TEXTS);
        assert_eq!(MAX_BROWSER_CHROME_RUNS, 16_384);
        assert!(scene.run_count() <= MAX_BROWSER_CHROME_RUNS);
        assert_eq!(
            scene
                .primary_layout()
                .expect("primary")
                .popup()
                .expect("popup")
                .rows()
                .len(),
            64
        );
    }

    #[test]
    fn helper_primary_scene_covers_rtl_without_hidden_state_duplication() {
        let surface = surface(800, 600, 1.0);
        let mut text = TextSystem::new_deterministic(TextLimits::default())
            .expect("deterministic font initializes");
        let tabs = vec![tab(&mut text, 1, "RTL")].into_boxed_slice();
        let active = Some(tabs[0].identity());
        let scene = primary_scene(
            surface,
            BrowserChromeDirection::RightToLeft,
            tabs,
            active,
            None,
            BrowserChromeFocus::AddressBar,
        );
        let layout = scene.primary_layout().expect("primary layout");
        assert_eq!(
            layout.preview().direction(),
            BrowserChromeDirection::RightToLeft
        );
        assert!(layout.control(BrowserPrimaryControlKind::UrlBar).focused());
    }
}
