use std::fmt;
use std::num::NonZeroU64;

use crate::{BrowserCommandOutcome, BrowserTabId, BrowserWindowId};

pub const MAX_PRIMARY_UI_PANEL_ROWS: usize = 64;
pub const MAX_PRIMARY_UI_LABEL_BYTES: usize = 256;
pub const MAX_PRIMARY_UI_SCROLL_ROWS: u8 = 64;

/// Never-zero revision of one window's canonical primary-UI state.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PrimaryUiRevision(NonZeroU64);

impl PrimaryUiRevision {
    #[must_use]
    pub const fn new(value: u64) -> Option<Self> {
        match NonZeroU64::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// Inline direction used by primary-chrome geometry and keyboard traversal.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PrimaryUiDirection {
    #[default]
    LeftToRight,
    RightToLeft,
}

/// Fixed functional controls in the primary browser chrome.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum PrimaryUiControl {
    Back = 0,
    Forward = 1,
    ReloadStop = 2,
    SiteIdentity = 3,
    AddressBar = 4,
    NewTab = 5,
    AllTabs = 6,
    ApplicationMenu = 7,
    Overflow = 8,
}

impl PrimaryUiControl {
    pub const ALL: [Self; 9] = [
        Self::Back,
        Self::Forward,
        Self::ReloadStop,
        Self::SiteIdentity,
        Self::AddressBar,
        Self::NewTab,
        Self::AllTabs,
        Self::ApplicationMenu,
        Self::Overflow,
    ];

    const fn bit(self) -> u16 {
        1_u16 << (self as u8)
    }
}

/// Small checked set used to transfer A4-resolved visible/overflow membership.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PrimaryUiControlSet(u16);

impl PrimaryUiControlSet {
    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    #[must_use]
    pub const fn wide_defaults() -> Self {
        let mut bits = 0_u16;
        bits |= PrimaryUiControl::Back.bit();
        bits |= PrimaryUiControl::Forward.bit();
        bits |= PrimaryUiControl::ReloadStop.bit();
        bits |= PrimaryUiControl::SiteIdentity.bit();
        bits |= PrimaryUiControl::AddressBar.bit();
        bits |= PrimaryUiControl::NewTab.bit();
        bits |= PrimaryUiControl::AllTabs.bit();
        bits |= PrimaryUiControl::ApplicationMenu.bit();
        Self(bits)
    }

    #[must_use]
    pub const fn with(mut self, control: PrimaryUiControl) -> Self {
        self.0 |= control.bit();
        self
    }

    #[must_use]
    pub const fn contains(self, control: PrimaryUiControl) -> bool {
        self.0 & control.bit() != 0
    }

    #[must_use]
    pub const fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }

    pub fn iter(self) -> impl Iterator<Item = PrimaryUiControl> {
        PrimaryUiControl::ALL
            .into_iter()
            .filter(move |control| self.contains(*control))
    }
}

/// Exact resolved chrome membership supplied by the native compositor layout.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PrimaryUiLayout {
    visible: PrimaryUiControlSet,
    overflowed: PrimaryUiControlSet,
    panel_row_capacity: usize,
}

impl Default for PrimaryUiLayout {
    fn default() -> Self {
        Self {
            visible: PrimaryUiControlSet::wide_defaults(),
            overflowed: PrimaryUiControlSet::empty(),
            panel_row_capacity: 16,
        }
    }
}

impl PrimaryUiLayout {
    /// Creates an exact layout membership snapshot.
    ///
    /// # Errors
    ///
    /// Rejects overlapping membership, an absent address/application control,
    /// overflow without its visible anchor, or an invalid panel-row bound.
    pub fn new(
        visible: PrimaryUiControlSet,
        overflowed: PrimaryUiControlSet,
        panel_row_capacity: usize,
    ) -> Result<Self, PrimaryUiLayoutError> {
        if visible.intersects(overflowed) {
            return Err(PrimaryUiLayoutError::OverlappingMembership);
        }
        for control in PrimaryUiControl::ALL {
            if control == PrimaryUiControl::Overflow {
                continue;
            }
            if !visible.contains(control) && !overflowed.contains(control) {
                return Err(PrimaryUiLayoutError::MissingControl(control));
            }
        }
        if !visible.contains(PrimaryUiControl::AddressBar)
            || !visible.contains(PrimaryUiControl::AllTabs)
            || !visible.contains(PrimaryUiControl::ApplicationMenu)
        {
            return Err(PrimaryUiLayoutError::MissingRequiredControl);
        }
        if overflowed.contains(PrimaryUiControl::AddressBar)
            || overflowed.contains(PrimaryUiControl::AllTabs)
            || overflowed.contains(PrimaryUiControl::ApplicationMenu)
            || overflowed.contains(PrimaryUiControl::Overflow)
        {
            return Err(PrimaryUiLayoutError::InvalidOverflowMember);
        }
        if overflowed.0 != 0 && !visible.contains(PrimaryUiControl::Overflow) {
            return Err(PrimaryUiLayoutError::MissingOverflowAnchor);
        }
        if overflowed.0 == 0 && visible.contains(PrimaryUiControl::Overflow) {
            return Err(PrimaryUiLayoutError::SpuriousOverflowAnchor);
        }
        if panel_row_capacity > MAX_PRIMARY_UI_PANEL_ROWS {
            return Err(PrimaryUiLayoutError::InvalidPanelRowCapacity {
                actual: panel_row_capacity,
                maximum: MAX_PRIMARY_UI_PANEL_ROWS,
            });
        }
        Ok(Self {
            visible,
            overflowed,
            panel_row_capacity,
        })
    }

    #[must_use]
    pub const fn visible(self) -> PrimaryUiControlSet {
        self.visible
    }

    #[must_use]
    pub const fn overflowed(self) -> PrimaryUiControlSet {
        self.overflowed
    }

    #[must_use]
    pub const fn panel_row_capacity(self) -> usize {
        self.panel_row_capacity
    }
}

/// Invalid compositor-resolved primary-UI layout membership.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrimaryUiLayoutError {
    OverlappingMembership,
    MissingControl(PrimaryUiControl),
    MissingRequiredControl,
    InvalidOverflowMember,
    MissingOverflowAnchor,
    SpuriousOverflowAnchor,
    InvalidPanelRowCapacity { actual: usize, maximum: usize },
}

impl fmt::Display for PrimaryUiLayoutError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "invalid primary-UI layout: {self:?}")
    }
}

impl std::error::Error for PrimaryUiLayoutError {}

/// Sole open primary panel for one browser window.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrimaryUiPanel {
    SiteIdentity,
    AllTabs,
    ApplicationMenu,
    Overflow,
}

impl PrimaryUiPanel {
    #[must_use]
    pub const fn anchor(self) -> PrimaryUiControl {
        match self {
            Self::SiteIdentity => PrimaryUiControl::SiteIdentity,
            Self::AllTabs => PrimaryUiControl::AllTabs,
            Self::ApplicationMenu => PrimaryUiControl::ApplicationMenu,
            Self::Overflow => PrimaryUiControl::Overflow,
        }
    }
}

/// Reload/stop is one control whose mode follows canonical loading state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrimaryReloadStopMode {
    Reload,
    Stop,
}

/// Deliberately conservative identity classification without TLS invention.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrimarySiteIdentityKind {
    NoPage,
    LoopbackHttp,
    InsecureHttp,
    Unverified,
}

/// Whether an element can currently produce its real typed action.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrimaryUiAvailability {
    Disabled,
    Enabled,
}

impl PrimaryUiAvailability {
    #[must_use]
    pub const fn is_enabled(self) -> bool {
        matches!(self, Self::Enabled)
    }
}

/// Interaction exposed by a semantic primary-UI element.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrimaryUiInteraction {
    None,
    Invoke,
    TogglePanel,
    Edit,
}

/// A11y-ready semantic role; this is not an AT-SPI output claim.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrimaryUiRole {
    Tab,
    Button,
    TextField,
    Menu,
    MenuItem,
    Document,
    Status,
}

/// Exact canonical focus owner for one window.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrimaryUiFocus {
    Page,
    Tab(BrowserTabId),
    Control(PrimaryUiControl),
    AddressBar,
    PanelItem(PrimaryUiPanelItemId),
}

/// Stable semantic identity of a primary-UI element.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum PrimaryUiElementId {
    Page,
    Tab(BrowserTabId),
    TabClose(BrowserTabId),
    Control(PrimaryUiControl),
    PanelItem(PrimaryUiPanelItemId),
}

/// Stable identity of a row in one canonical panel.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum PrimaryUiPanelItemId {
    IdentitySummary,
    AllTabsTab(BrowserTabId),
    ApplicationNewTab,
    ApplicationCloseTab,
    ApplicationBack,
    ApplicationForward,
    ApplicationReloadStop,
    OverflowControl(PrimaryUiControl),
}

/// Immutable inspection of one primary control.
#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(clippy::struct_excessive_bools)]
pub struct PrimaryUiControlSnapshot {
    pub control: PrimaryUiControl,
    pub name: Box<str>,
    pub availability: PrimaryUiAvailability,
    pub interaction: PrimaryUiInteraction,
    pub visible: bool,
    pub overflowed: bool,
    pub expanded: bool,
    pub focused: bool,
    pub reload_stop_mode: Option<PrimaryReloadStopMode>,
    pub site_identity: Option<PrimarySiteIdentityKind>,
}

/// Immutable inspection of one live tab's primary representation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PrimaryUiTabSnapshot {
    pub tab: BrowserTabId,
    pub name: Box<str>,
    pub selected: bool,
    pub loading: bool,
    pub focused: bool,
    pub close_availability: PrimaryUiAvailability,
}

/// Product action represented by one enabled panel row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrimaryUiPanelItemAction {
    None,
    ActivateTab(BrowserTabId),
    InvokeControl(PrimaryUiControl),
    CloseActiveTab,
}

/// Immutable inspection of one currently visible panel row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PrimaryUiPanelItemSnapshot {
    pub id: PrimaryUiPanelItemId,
    pub name: Box<str>,
    pub availability: PrimaryUiAvailability,
    pub interaction: PrimaryUiInteraction,
    pub selected: bool,
    pub expanded: bool,
    pub focused: bool,
    pub action: PrimaryUiPanelItemAction,
}

/// Immutable inspection of the sole open panel and its visible row window.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PrimaryUiPanelSnapshot {
    pub panel: PrimaryUiPanel,
    pub anchor: PrimaryUiControl,
    pub items: Box<[PrimaryUiPanelItemSnapshot]>,
    pub selected: Option<PrimaryUiPanelItemId>,
    /// First row in the exact visible scroll window.
    pub scroll_offset: usize,
    /// Maximum rows visible from `scroll_offset` in this layout.
    pub visible_capacity: usize,
    pub total_rows: usize,
}

/// Generic semantic node suitable for a later Linux accessibility adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(clippy::struct_excessive_bools)]
pub struct PrimaryUiSemanticNode {
    pub id: PrimaryUiElementId,
    pub role: PrimaryUiRole,
    pub name: Box<str>,
    pub enabled: bool,
    pub selected: bool,
    pub expanded: bool,
    pub focused: bool,
    /// Whether this node is currently presented rather than offscreen.
    pub visible: bool,
}

/// Exact immutable primary-UI projection from one canonical session revision.
///
/// Every semantic identity is scoped by this snapshot's `window` and
/// `revision`; use [`Self::bind_action`] when routing a rendered pointer hit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PrimaryUiSnapshot {
    pub window: BrowserWindowId,
    pub revision: PrimaryUiRevision,
    pub direction: PrimaryUiDirection,
    pub focus: PrimaryUiFocus,
    pub controls: Box<[PrimaryUiControlSnapshot]>,
    pub tabs: Box<[PrimaryUiTabSnapshot]>,
    pub panel: Option<PrimaryUiPanelSnapshot>,
    pub semantics: Box<[PrimaryUiSemanticNode]>,
}

impl PrimaryUiSnapshot {
    /// Binds one enabled semantic element to its sole direct pointer action.
    ///
    /// The returned opaque binding preserves this snapshot's exact window and
    /// revision. Informational panel rows have no direct action; the page node
    /// binds canonical content focus.
    #[must_use]
    pub fn bind_action(&self, element: PrimaryUiElementId) -> Option<PrimaryUiActionBinding> {
        let node = self
            .semantics
            .iter()
            .find(|candidate| candidate.id == element && candidate.enabled && candidate.visible)?;
        let action = match node.id {
            PrimaryUiElementId::Page => PrimaryUiAction::FocusPage,
            PrimaryUiElementId::Tab(tab) => PrimaryUiAction::ActivateTab(tab),
            PrimaryUiElementId::TabClose(tab) => PrimaryUiAction::CloseTab(tab),
            PrimaryUiElementId::Control(control) => PrimaryUiAction::InvokeControl(control),
            PrimaryUiElementId::PanelItem(item) => {
                let panel = self.panel.as_ref()?;
                let row = panel.items.iter().find(|candidate| candidate.id == item)?;
                if row.action == PrimaryUiPanelItemAction::None {
                    return None;
                }
                PrimaryUiAction::ActivatePanelItem(item)
            }
        };
        Some(PrimaryUiActionBinding {
            window: self.window,
            revision: self.revision,
            source: element,
            action,
        })
    }

    /// Binds an outside-popup hit to dismissal of this exact open panel.
    #[must_use]
    pub fn bind_panel_dismissal(&self) -> Option<PrimaryUiActionBinding> {
        let panel = self.panel.as_ref()?;
        let source = PrimaryUiElementId::Control(panel.anchor);
        self.semantics
            .iter()
            .find(|node| node.id == source && node.enabled && node.visible)?;
        Some(PrimaryUiActionBinding {
            window: self.window,
            revision: self.revision,
            source,
            action: PrimaryUiAction::DismissPanel,
        })
    }

    /// Binds a bounded row scroll for the exact open all-tabs inventory.
    #[must_use]
    pub fn bind_panel_scroll(
        &self,
        direction: PrimaryUiMoveDirection,
        rows: u8,
    ) -> Option<PrimaryUiActionBinding> {
        if rows == 0 || rows > MAX_PRIMARY_UI_SCROLL_ROWS {
            return None;
        }
        let panel = self.panel.as_ref()?;
        if panel.panel != PrimaryUiPanel::AllTabs
            || panel.visible_capacity == 0
            || panel.total_rows <= panel.visible_capacity
        {
            return None;
        }
        let source = PrimaryUiElementId::Control(panel.anchor);
        self.semantics
            .iter()
            .find(|node| node.id == source && node.enabled && node.visible)?;
        Some(PrimaryUiActionBinding {
            window: self.window,
            revision: self.revision,
            source,
            action: PrimaryUiAction::ScrollPanel { direction, rows },
        })
    }
}

/// Opaque window- and revision-scoped action derived from one UI snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PrimaryUiActionBinding {
    window: BrowserWindowId,
    revision: PrimaryUiRevision,
    source: PrimaryUiElementId,
    action: PrimaryUiAction,
}

impl PrimaryUiActionBinding {
    #[must_use]
    pub const fn window(self) -> BrowserWindowId {
        self.window
    }

    #[must_use]
    pub const fn revision(self) -> PrimaryUiRevision {
        self.revision
    }

    #[must_use]
    pub const fn source(self) -> PrimaryUiElementId {
        self.source
    }

    #[must_use]
    pub const fn action(self) -> PrimaryUiAction {
        self.action
    }
}

/// Direction for logical focus or panel-selection movement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrimaryUiMoveDirection {
    Forward,
    Backward,
}

/// Product action admitted only against an exact current UI revision.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrimaryUiAction {
    FocusPage,
    InvokeControl(PrimaryUiControl),
    ActivateTab(BrowserTabId),
    CloseTab(BrowserTabId),
    ActivatePanelItem(PrimaryUiPanelItemId),
    DismissPanel,
    MoveDocumentFocus(PrimaryUiMoveDirection),
    MoveFocus(PrimaryUiMoveDirection),
    MoveToolbarFocus(PrimaryUiMoveDirection),
    MovePanelSelection(PrimaryUiMoveDirection),
    ScrollPanel {
        direction: PrimaryUiMoveDirection,
        rows: u8,
    },
    ActivateFocused,
}

/// Observable result of one exact-revision primary-UI action.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrimaryUiActionOutcome {
    Command(BrowserCommandOutcome),
    FocusChanged {
        previous: PrimaryUiFocus,
        current: PrimaryUiFocus,
    },
    PanelChanged(Option<PrimaryUiPanel>),
    PanelScrolled {
        panel: PrimaryUiPanel,
        first_visible_row: usize,
    },
    Disabled(PrimaryUiElementId),
    Stale {
        expected: PrimaryUiRevision,
        current: PrimaryUiRevision,
    },
    NoChange,
}

#[derive(Clone, Debug)]
pub(crate) struct PrimaryUiWindowState {
    pub(crate) revision: PrimaryUiRevision,
    pub(crate) direction: PrimaryUiDirection,
    pub(crate) focus: PrimaryUiFocus,
    pub(crate) panel: Option<PrimaryUiPanel>,
    pub(crate) panel_selected: Option<PrimaryUiPanelItemId>,
    pub(crate) all_tabs_scroll: usize,
    pub(crate) layout: PrimaryUiLayout,
}

impl PrimaryUiWindowState {
    pub(crate) fn initial(_tab: BrowserTabId) -> Self {
        Self {
            revision: PrimaryUiRevision::new(1).expect("one is nonzero"),
            direction: PrimaryUiDirection::LeftToRight,
            focus: PrimaryUiFocus::AddressBar,
            panel: None,
            panel_selected: None,
            all_tabs_scroll: 0,
            layout: PrimaryUiLayout::default(),
        }
    }

    pub(crate) fn bump(&mut self) -> bool {
        let Some(next) = self.revision.get().checked_add(1) else {
            return false;
        };
        self.revision = PrimaryUiRevision::new(next).expect("incremented revision is nonzero");
        true
    }
}
